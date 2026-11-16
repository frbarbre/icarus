//! Projects and workspaces: a workspace is a git worktree plus the agents and terminals in it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use icarus_git::{CreateWorktree, RemoveError};
use icarus_protocol::RpcError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::{AgentOptions, AgentState};
use crate::hooks::{SetupDecision, setup_key, should_run_setup};
use crate::next_action;
use crate::server::{Server, params, rpc, to_value};
use crate::state::{Workspace, workspace_slug};

const SLUG_MAX: usize = 63;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn internal(error: impl std::fmt::Display) -> RpcError {
    rpc("internal", error)
}

#[derive(Deserialize)]
struct Key {
    id: String,
}

impl Server {
    pub(crate) fn workspace_by_key(&self, key: &str) -> Result<Workspace, RpcError> {
        self.store
            .workspace(key)
            .map_err(internal)?
            .ok_or_else(|| rpc("not_found", format!("workspace {key}")))
    }

    fn workspace_json(&self, workspace: &Workspace) -> Value {
        let agents: Vec<Value> = self
            .agents
            .list()
            .into_iter()
            .filter(|a| a.workspace.as_deref() == Some(workspace.id.as_str()))
            .map(|a| json!({ "id": a.id, "title": a.title, "state": a.state, "provider": a.provider }))
            .collect();
        let terminals: Vec<Value> = self
            .terminals
            .list()
            .into_iter()
            .filter(|t| t.workspace.as_deref() == Some(workspace.id.as_str()))
            .map(|t| json!({ "id": t.id, "label": t.label, "running": t.running }))
            .collect();
        let mut value = serde_json::to_value(workspace).unwrap_or_default();
        value["agents"] = json!(agents);
        value["terminals"] = json!(terminals);
        value["setup"] = json!(self.setup_status(&workspace.id));
        value["sleeping"] = json!(matches!(
            self.store.kv_get(&sleep_key(&workspace.id)),
            Ok(Some(_))
        ));
        value["pr_author"] = workspace
            .branch
            .as_deref()
            .map(|branch| self.github.cached_pr_author(&workspace.project_id, branch))
            .unwrap_or(Value::Null);
        value["pr"] = workspace
            .branch
            .as_deref()
            .map(|branch| self.github.cached_pr_summary(&workspace.project_id, branch))
            .unwrap_or(Value::Null);
        self.enrich_issue(&mut value["issue"]);
        value
    }

    pub(crate) async fn workspace_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        match method {
            "project.add" => {
                #[derive(Deserialize)]
                struct P {
                    path: PathBuf,
                    name: Option<String>,
                    base_ref: Option<String>,
                }
                let p: P = params(p)?;
                let info = icarus_git::repo_info(&p.path)
                    .map_err(|e| rpc("not_a_repo", format!("{e:#}")))?;
                let name = p
                    .name
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| default_project_name(&info.root));
                let base = p
                    .base_ref
                    .filter(|base| !base.trim().is_empty())
                    .or(info.default_base);
                let project = self
                    .store
                    .add_project(&name, &info.root, info.remote.as_deref(), base.as_deref())
                    .map_err(internal)?;
                to_value(project)
            }
            "project.inspect" => {
                #[derive(Deserialize)]
                struct P {
                    path: PathBuf,
                }
                let p: P = params(p)?;
                let info = icarus_git::repo_info(&p.path)
                    .map_err(|e| rpc("not_a_repo", format!("{e:#}")))?;
                let existing = self
                    .store
                    .project_by_root(&info.root)
                    .map_err(internal)?
                    .map(|project| project.id);
                Ok(json!({
                    "root": info.root,
                    "name": default_project_name(&info.root),
                    "remote": info.remote,
                    "base_ref": info.default_base,
                    "existing": existing,
                }))
            }
            "project.list" => to_value(self.store.projects().map_err(internal)?),
            "project.remove" => {
                let key: Key = params(p)?;
                let project = self
                    .store
                    .project(&key.id)
                    .map_err(internal)?
                    .ok_or_else(|| rpc("not_found", &key.id))?;
                Ok(json!({ "removed": self.store.remove_project(&project.id).map_err(internal)? }))
            }
            "workspace.create" => {
                #[derive(Deserialize)]
                struct P {
                    project: String,
                    /// Optional when `issue` links a Linear issue: then it comes from the issue.
                    name: Option<String>,
                    branch: Option<String>,
                    base: Option<String>,
                    #[serde(default)]
                    fetch: bool,
                    status: Option<String>,
                    title: Option<String>,
                    issue: Option<Value>,
                    prompt: Option<String>,
                    /// Every workspace gets an agent unless this is false (plan §3-0).
                    #[serde(default = "yes")]
                    agent: bool,
                    provider: Option<String>,
                    model: Option<String>,
                    agent_command: Option<Vec<String>>,
                    /// Account id or label for the agent; `system` for the provider's own login.
                    account: Option<String>,
                    /// A name under `layouts`, applied once the agent exists (plan D32).
                    layout: Option<String>,
                    pr_url: Option<String>,
                    /// Start the agent with Claude Code's Remote Control; default `agents.remote_control`.
                    remote_control: Option<bool>,
                    /// The project's setup script: run, skip, inherit (its `setupRunPolicy`) or auto.
                    setup: Option<Value>,
                }
                let p: P = params(p)?;
                let project = self
                    .store
                    .project(&p.project)
                    .map_err(internal)?
                    .ok_or_else(|| rpc("not_found", format!("project {}", p.project)))?;
                let setup_decision = SetupDecision::parse(p.setup.as_ref())?;
                // Decided before the worktree exists, so an "ask" project leaves nothing behind.
                let hooks = self.hooks_for(&project.root, &project.root);
                if hooks.setup.is_some() {
                    should_run_setup(hooks.run_policy, setup_decision)?;
                }
                // Checked before the worktree exists, so a typo leaves nothing behind.
                if let Some(key) = p
                    .account
                    .as_deref()
                    .filter(|k| *k != crate::accounts::SYSTEM)
                {
                    self.find_account(key)?;
                }
                let mut issue = p.issue;
                let linked = issue.as_ref().and_then(crate::linear::linear_issue_id);
                let from_linear = match &linked {
                    Some(identifier) => self.linear_issue_for_create(identifier).await,
                    None => None,
                };
                if let (Some(stored), Some(identifier)) = (issue.as_mut(), &linked) {
                    stored["id"] = json!(identifier);
                    if let Some(found) = &from_linear
                        && stored["title"].as_str().is_none_or(str::is_empty)
                    {
                        stored["title"] = json!(found.title);
                    }
                }
                let name = p
                    .name
                    .filter(|name| !name.trim().is_empty())
                    // Why: the identifier alone keeps `<slug>.localhost` dev URLs under the 63-char DNS label limit.
                    .or_else(|| linked.as_ref().map(|identifier| identifier.to_lowercase()))
                    .ok_or_else(|| rpc("invalid_params", "name is required"))?;
                let slug = workspace_slug(&name, SLUG_MAX);
                let repo_dir = workspace_slug(&project.name, SLUG_MAX);
                let path = self.workspace_root.join(repo_dir).join(&slug);
                if path.exists() {
                    return Err(rpc("exists", format!("{} already exists", path.display())));
                }
                let branch = p
                    .branch
                    .or_else(|| from_linear.as_ref()?.branch_name.clone())
                    .unwrap_or_else(|| slug.clone());
                let base = p
                    .base
                    .or_else(|| project.base_ref.clone())
                    .ok_or_else(|| rpc("no_base", "project has no base ref; pass base"))?;
                let root = project.root.clone();
                let (path_c, branch_c, base_c, fetch) =
                    (path.clone(), branch.clone(), base.clone(), p.fetch);
                tokio::task::spawn_blocking(move || {
                    icarus_git::create_worktree(&CreateWorktree {
                        repo: &root,
                        path: &path_c,
                        branch: &branch_c,
                        base: &base_c,
                        fetch,
                    })
                })
                .await
                .map_err(internal)?
                .map_err(|e| rpc("git_failed", format!("{e:#}")))?;
                let workspace = Workspace {
                    id: format!("ws-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]),
                    project_id: project.id.clone(),
                    name: slug,
                    path: path.clone(),
                    branch: Some(branch),
                    base_ref: Some(base),
                    status: p.status,
                    title: p.title,
                    issue,
                    pull_request: None,
                    created_at_ms: now_ms(),
                    pinned: false,
                };
                self.store.insert_workspace(&workspace).map_err(internal)?;
                if let Err(error) = self.start_setup(&workspace, &project.root, setup_decision) {
                    tracing::warn!("setup for {}: {}", workspace.name, error.message);
                    self.record_setup_failure(&workspace.id, error.message);
                }
                if p.agent {
                    let settings = self.resolved_settings(Some(&project.root));
                    let provider = p
                        .provider
                        .unwrap_or_else(|| next_action::default_provider(&settings));
                    let command = p
                        .agent_command
                        .or_else(|| next_action::configured_command(&settings, &provider));
                    let mut options = AgentOptions {
                        provider,
                        cwd: path,
                        workspace: Some(workspace.id.clone()),
                        title: Some("Agent".into()),
                        model: p.model,
                        bypass_permissions: true,
                        resume: None,
                        command,
                        env: Default::default(),
                        account: None,
                        remote_control: p
                            .remote_control
                            .unwrap_or_else(|| next_action::remote_control_default(&settings)),
                    };
                    self.apply_account(&mut options, p.account.as_deref())?;
                    let agent = self
                        .agents
                        .create(options)
                        .await
                        .map_err(|e| rpc("spawn_failed", format!("{e:#}")))?;
                    if let Some(prompt) = p.prompt {
                        if self.agent_waits_for_setup(&workspace.id) {
                            // `wait-for-setup`: the agent exists now, its first prompt goes once setup ends.
                            let server = Arc::clone(self);
                            let workspace_id = workspace.id.clone();
                            tokio::spawn(async move {
                                server.wait_for_setup(&workspace_id).await;
                                if let Err(error) = agent.send(&prompt, &[]).await {
                                    tracing::warn!(
                                        "first prompt after setup of {workspace_id}: {error:#}"
                                    );
                                }
                            });
                        } else {
                            agent
                                .send(&prompt, &[])
                                .await
                                .map_err(|e| rpc("send_failed", format!("{e:#}")))?;
                        }
                    }
                }
                let mut value = self.workspace_json(&workspace);
                let layout = p.layout.or_else(|| {
                    p.agent
                        .then(|| icarus_protocol::layout::DEFAULT_LAYOUT.to_owned())
                });
                if let Some(layout) = layout {
                    value["layout"] = self.apply_layout(&workspace, &layout, p.pr_url).await?;
                }
                Ok(value)
            }
            "workspace.list" => {
                #[derive(Deserialize)]
                struct P {
                    project: Option<String>,
                }
                let p: P = params(p)?;
                let project_id = match p.project {
                    Some(key) => Some(
                        self.store
                            .project(&key)
                            .map_err(internal)?
                            .ok_or_else(|| rpc("not_found", &key))?
                            .id,
                    ),
                    None => None,
                };
                let workspaces = self
                    .store
                    .workspaces(project_id.as_deref())
                    .map_err(internal)?;
                Ok(json!(
                    workspaces
                        .iter()
                        .map(|w| self.workspace_json(w))
                        .collect::<Vec<_>>()
                ))
            }
            "workspace.info" => {
                let key: Key = params(p)?;
                Ok(self.workspace_json(&self.workspace_by_key(&key.id)?))
            }
            "workspace.set" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    status: Option<String>,
                    title: Option<String>,
                    base: Option<String>,
                    issue: Option<Value>,
                    pull_request: Option<Value>,
                    pinned: Option<bool>,
                }
                let p: P = params(p)?;
                let mut workspace = self.workspace_by_key(&p.id)?;
                let pin_changed = p.pinned.is_some_and(|pinned| pinned != workspace.pinned);
                if let Some(pinned) = p.pinned {
                    workspace.pinned = pinned;
                }
                if let Some(status) = p.status {
                    workspace.status = if status.is_empty() {
                        None
                    } else {
                        Some(status)
                    };
                }
                if let Some(title) = p.title {
                    workspace.title = Some(title);
                }
                if let Some(base) = p.base {
                    workspace.base_ref = Some(base);
                }
                if let Some(issue) = p.issue {
                    workspace.issue = if issue.is_null() { None } else { Some(issue) };
                }
                if let Some(pr) = p.pull_request {
                    workspace.pull_request = if pr.is_null() { None } else { Some(pr) };
                }
                self.store.update_workspace(&workspace).map_err(internal)?;
                if pin_changed {
                    self.broadcast(
                        "workspace.changed",
                        json!({ "workspaces": [workspace.id], "source": "pin" }),
                    );
                }
                Ok(self.workspace_json(&workspace))
            }
            "workspace.remove" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    #[serde(default)]
                    force: bool,
                    #[serde(default = "yes")]
                    delete_branch: bool,
                    /// Remove without running the project's archive script.
                    #[serde(default)]
                    skip_archive: bool,
                    /// Remove even if the archive script fails (`force` implies it).
                    #[serde(default)]
                    allow_failed_archive: bool,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.id)?;
                let unsent = self.unsent_drafts(&workspace.id);
                if unsent > 0 && !p.force {
                    return Err(rpc(
                        "unsent_review",
                        format!(
                            "{} holds {unsent} unsent review comment(s); submit or delete them, or remove with force",
                            workspace.name
                        ),
                    ));
                }
                let project = self
                    .store
                    .project(&workspace.project_id)
                    .map_err(internal)?
                    .ok_or_else(|| rpc("not_found", "project"))?;
                // Orca runs the archive hook before any teardown, and a failure stops the removal.
                let archive = if p.skip_archive || !workspace.path.is_dir() {
                    None
                } else {
                    self.run_archive(&workspace, &project.root).await
                };
                if let Some(outcome) = &archive
                    && !outcome.success
                    && !(p.force || p.allow_failed_archive)
                {
                    let output: String = outcome
                        .output
                        .chars()
                        .rev()
                        .take(2000)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    return Err(rpc(
                        "archive_failed",
                        format!(
                            "the archive script failed, so {} was kept; remove with force (or allow_failed_archive) to remove it anyway\n{output}",
                            workspace.name
                        ),
                    ));
                }
                let (root, path, force, delete_branch) = (
                    project.root,
                    workspace.path.clone(),
                    p.force,
                    p.delete_branch,
                );
                let result = tokio::task::spawn_blocking(move || {
                    if path.exists() {
                        icarus_git::remove_worktree(&root, &path, force, delete_branch)
                    } else {
                        Ok(())
                    }
                })
                .await
                .map_err(internal)?;
                match result {
                    Err(RemoveError::Dirty) => return Err(rpc("dirty", RemoveError::Dirty)),
                    Err(error) => return Err(rpc("git_failed", format!("{error:#}"))),
                    Ok(()) => {}
                }
                for agent in self.agents.all() {
                    if agent.info().workspace.as_deref() == Some(workspace.id.as_str()) {
                        let _ = agent.kill().await;
                        self.agents.remove(agent.id());
                    }
                }
                for terminal in self.terminals.list() {
                    if terminal.workspace.as_deref() == Some(workspace.id.as_str())
                        && let Some(session) = self.terminals.remove(&terminal.id)
                    {
                        let _ = session.kill();
                    }
                }
                self.store
                    .remove_workspace(&workspace.id)
                    .map_err(internal)?;
                let _ = self.store.kv_delete(&sleep_key(&workspace.id));
                let _ = self.store.kv_delete(&setup_key(&workspace.id));
                let runner = self
                    .paths
                    .home
                    .join("setup-runners")
                    .join(format!("{}.sh", workspace.id));
                if runner.exists()
                    && let Err(error) = std::fs::remove_file(&runner)
                {
                    tracing::warn!("removing {}: {error}", runner.display());
                }
                Ok(json!({ "removed": true, "archive": archive }))
            }
            "workspace.setup" => {
                let key: Key = params(p)?;
                let workspace = self.workspace_by_key(&key.id)?;
                if self
                    .setup_status(&workspace.id)
                    .is_some_and(|s| s.state == crate::hooks::SetupState::Running)
                {
                    return Err(rpc("setup_running", "setup is already running"));
                }
                let project = self
                    .store
                    .project(&workspace.project_id)
                    .map_err(internal)?
                    .ok_or_else(|| rpc("not_found", "project"))?;
                match self.start_setup(&workspace, &project.root, SetupDecision::Run)? {
                    Some(status) => Ok(json!(status)),
                    None => Err(rpc(
                        "no_setup_script",
                        "this project has no setup script (scripts.setup in orca.yaml or project.hooks.scripts.setup)",
                    )),
                }
            }
            "workspace.sleep" => {
                let key: Key = params(p)?;
                let workspace = self.workspace_by_key(&key.id)?;
                let mut parked = self.parked(&workspace.id);
                let mut parking = Vec::new();
                for agent in self.agents.all() {
                    let info = agent.info();
                    if info.workspace.as_deref() != Some(workspace.id.as_str())
                        || matches!(info.state, AgentState::Exited | AgentState::Sleeping)
                    {
                        continue;
                    }
                    agent
                        .sleep()
                        .await
                        .map_err(|e| rpc("sleep_failed", format!("{e:#}")))?;
                    parked.agents.push(info.id);
                    parking.push(agent);
                }
                let mut stopping = Vec::new();
                for terminal in self.terminals.list() {
                    if terminal.workspace.as_deref() != Some(workspace.id.as_str())
                        || !terminal.running
                    {
                        continue;
                    }
                    if let Some(session) = self.terminals.get(&terminal.id) {
                        session
                            .kill()
                            .map_err(|e| rpc("kill_failed", format!("{e:#}")))?;
                        parked.terminals.push(terminal.id);
                        stopping.push(session);
                    }
                }
                let deadline = Instant::now() + Duration::from_secs(5);
                while (stopping.iter().any(|s| s.is_running())
                    || parking
                        .iter()
                        .any(|a| !matches!(a.state(), AgentState::Sleeping | AgentState::Exited)))
                    && Instant::now() < deadline
                {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                parked.agents.sort();
                parked.agents.dedup();
                parked.terminals.sort();
                parked.terminals.dedup();
                self.store
                    .kv_set(&sleep_key(&workspace.id), &json!(parked))
                    .map_err(internal)?;
                self.broadcast(
                    "workspace.changed",
                    json!({ "workspaces": [workspace.id], "source": "sleep" }),
                );
                Ok(
                    json!({ "sleeping": true, "agents": parked.agents, "terminals": parked.terminals }),
                )
            }
            "workspace.wake" => {
                let key: Key = params(p)?;
                let workspace = self.workspace_by_key(&key.id)?;
                let parked = self.parked(&workspace.id);
                let mut agents = Vec::new();
                for id in &parked.agents {
                    if let Some(agent) = self.agents.get(id)
                        && agent
                            .wake()
                            .await
                            .map_err(|e| rpc("spawn_failed", format!("{e:#}")))?
                    {
                        agents.push(id.clone());
                    }
                }
                let mut terminals = Vec::new();
                for id in &parked.terminals {
                    if let Some(session) = self.terminals.get(id) {
                        session
                            .respawn()
                            .map_err(|e| rpc("spawn_failed", format!("{e:#}")))?;
                        terminals.push(id.clone());
                    }
                }
                self.store
                    .kv_delete(&sleep_key(&workspace.id))
                    .map_err(internal)?;
                self.broadcast(
                    "workspace.changed",
                    json!({ "workspaces": [workspace.id], "source": "wake" }),
                );
                Ok(json!({ "sleeping": false, "agents": agents, "terminals": terminals }))
            }
            other => Err(rpc("unknown_method", other)),
        }
    }
}

fn yes() -> bool {
    true
}

fn default_project_name(root: &std::path::Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".into())
}

fn sleep_key(workspace_id: &str) -> String {
    format!("workspace.sleep.{workspace_id}")
}

/// What `workspace.sleep` stopped, so `workspace.wake` restarts exactly that.
#[derive(Default, Serialize, Deserialize)]
struct Parked {
    agents: Vec<String>,
    terminals: Vec<String>,
}

impl Server {
    fn parked(&self, workspace_id: &str) -> Parked {
        self.store
            .kv_get(&sleep_key(workspace_id))
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }
}
