//! Projects and workspaces: a workspace is a git worktree plus the agents and terminals in it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use icarus_git::RemoveError;
use icarus_protocol::RpcError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::{AgentOptions, AgentState};
use crate::create::{
    CancelFlag, PrHead, WorktreePlan, add_worktree, branch_exists, discard_worktree,
    materialize_shared_paths, or_cancelled, parse_pr, pull_ref, read_worktree_include,
    relink_shared_paths, unlink_shared_paths,
};
use crate::hooks::{ScriptKind, SetupDecision, setup_key, should_run_setup, untrusted_message};
use crate::next_action;
use crate::server::{Server, params, rpc, to_value};
use crate::state::{Discovery, ORIGIN_ADOPTED, ORIGIN_NATIVE, Workspace, workspace_slug};

const SLUG_MAX: usize = 63;
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// Stamps the workspace's activity time (throttled) and tells clients, which re-sort Recent lists.
    pub(crate) fn touch_activity(&self, workspace_key: &str) {
        // Terminals keep the workspace key they were created with, which may be a name or path.
        let workspace_id = match self.store.workspace(workspace_key) {
            Ok(Some(workspace)) => workspace.id,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!("activity for {workspace_key}: {error:#}");
                return;
            }
        };
        let workspace_id = workspace_id.as_str();
        match self.store.touch_activity(workspace_id, now_ms()) {
            Ok(true) => {
                self.broadcast(
                    "workspace.changed",
                    json!({ "workspaces": [workspace_id], "source": "activity" }),
                );
            }
            Ok(false) => {}
            Err(error) => tracing::warn!("activity for {workspace_id}: {error:#}"),
        }
    }

    /// Marks the workspace unread after an agent finished or needs input, unless a client shows it.
    pub(crate) fn agent_news(&self, workspace_key: &str, kind: UnreadKind) {
        let workspace_id = match self.store.workspace(workspace_key) {
            Ok(Some(workspace)) => workspace.id,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!("unread for {workspace_key}: {error:#}");
                return;
            }
        };
        if self.focused_anywhere(&workspace_id) {
            return;
        }
        // A question ends its turn right after it is asked; the turn's end must not hide the question.
        let waiting = self
            .unread(&workspace_id)
            .is_some_and(|unread| unread.kind == UnreadKind::NeedsInput);
        if waiting && kind == UnreadKind::Done {
            return;
        }
        self.set_unread(&workspace_id, Some(kind));
    }

    fn focused_anywhere(&self, workspace_id: &str) -> bool {
        self.focus
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .any(|focused| focused == workspace_id)
    }

    pub(crate) fn unread(&self, workspace_id: &str) -> Option<Unread> {
        self.store
            .kv_get(&unread_key(workspace_id))
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_value(value).ok())
    }

    /// Sets or clears the unread state and tells clients when it changed.
    pub(crate) fn set_unread(&self, workspace_id: &str, kind: Option<UnreadKind>) {
        let previous = self.unread(workspace_id);
        let next = kind.map(|kind| Unread {
            kind,
            at_ms: now_ms(),
        });
        let stored = match &next {
            Some(unread) => self
                .store
                .kv_set(&unread_key(workspace_id), &json!(unread))
                .map(|()| true),
            None => self.store.kv_delete(&unread_key(workspace_id)),
        };
        match stored {
            Ok(changed) if changed && (previous.is_some() || next.is_some()) => {
                self.broadcast(
                    "workspace.changed",
                    json!({ "workspaces": [workspace_id], "source": "unread", "unread": next }),
                );
            }
            Ok(_) => {}
            Err(error) => tracing::warn!("unread for {workspace_id}: {error:#}"),
        }
    }

    /// `client.focus`: the workspace a client shows now (by id, name or a path inside it), or none.
    pub(crate) fn set_focus(&self, connection: &str, p: Value) -> Result<Value, RpcError> {
        #[derive(Deserialize)]
        struct P {
            workspace: Option<String>,
            path: Option<PathBuf>,
        }
        let p: P = params(p)?;
        let workspace = match (p.workspace, p.path) {
            (Some(key), _) => Some(self.workspace_by_key(&key)?),
            (None, Some(path)) => self
                .store
                .workspaces(None)
                .map_err(internal)?
                .into_iter()
                .filter(|workspace| path.starts_with(&workspace.path))
                .max_by_key(|workspace| workspace.path.components().count()),
            (None, None) => None,
        };
        let mut focus = self.focus.lock().unwrap_or_else(|p| p.into_inner());
        match &workspace {
            Some(workspace) => focus.insert(connection.to_owned(), workspace.id.clone()),
            None => focus.remove(connection),
        };
        drop(focus);
        if let Some(workspace) = &workspace {
            self.set_unread(&workspace.id, None);
        }
        Ok(json!({ "workspace": workspace.map(|workspace| workspace.id) }))
    }

    pub(crate) fn forget_focus(&self, connection: &str) {
        self.focus
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(connection);
    }

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
            .map(|a| a.summary())
            .collect();
        let terminals: Vec<Value> = self
            .terminals
            .list()
            .into_iter()
            .filter(|t| t.workspace.as_deref() == Some(workspace.id.as_str()))
            .map(|t| json!({ "id": t.id, "label": t.label, "running": t.running }))
            .collect();
        let mut value = serde_json::to_value(workspace).unwrap_or_default();
        value["source_label"] = json!(crate::discovery::source_label(workspace));
        value["primary"] = json!(workspace.discovery.origin == crate::discovery::ORIGIN_PRIMARY);
        value["agents"] = json!(agents);
        value["terminals"] = json!(terminals);
        value["setup"] = json!(self.setup_status(&workspace.id));
        value["unread"] = json!(self.unread(&workspace.id));
        value["sleeping"] = json!(matches!(
            self.store.kv_get(&sleep_key(&workspace.id)),
            Ok(Some(_))
        ));
        let primary = workspace.discovery.origin == crate::discovery::ORIGIN_PRIMARY;
        // As Orca: the main checkout's branch is the base, so a PR headed by it (a release) isn't its own.
        let pr_branch = workspace.branch.as_deref().filter(|_| !primary);
        value["pr_author"] = pr_branch
            .map(|branch| self.github.cached_pr_author(&workspace.project_id, branch))
            .unwrap_or(Value::Null);
        value["pr"] = pr_branch
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
                let info = repo_info_off_thread(&p.path).await?;
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
                let info = repo_info_off_thread(&p.path).await?;
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
                let p: CreateParams = params(p)?;
                let create_id = p
                    .create_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
                let flag = self.creates.begin(&create_id);
                let result = self.create_workspace(p, &flag).await;
                self.creates.finish(&create_id);
                result
            }
            "workspace.adopt" => {
                let p: AdoptParams = params(p)?;
                self.adopt_workspace(p).await
            }
            "workspace.discover"
            | "workspace.inbox"
            | "workspace.visibility"
            | "workspace.show_source"
            | "workspace.inbox_dismiss" => self.discovery_dispatch(method, p).await,
            "workspace.create_cancel" => {
                #[derive(Deserialize)]
                struct P {
                    create_id: String,
                }
                let p: P = params(p)?;
                Ok(json!({ "cancelled": self.creates.cancel(&p.create_id) }))
            }
            "project.scripts" | "project.trust" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    /// Read `orca.yaml` from this workspace's checkout as well as the project root.
                    workspace: Option<String>,
                    /// Orca's "Always trust": every committed script, now and after it changes.
                    #[serde(default)]
                    always: bool,
                }
                let p: P = params(p)?;
                let project = self
                    .store
                    .project(&p.id)
                    .map_err(internal)?
                    .ok_or_else(|| rpc("not_found", &p.id))?;
                let yaml_dir = match &p.workspace {
                    Some(key) => self.workspace_by_key(key)?.path,
                    None => project.root.clone(),
                };
                let hooks = self.hooks_for(&project.root, &yaml_dir);
                if method == "project.trust" {
                    if p.always {
                        self.trust_always(&project.id)?;
                    }
                    self.trust_hooks(&project.id, &hooks)?;
                    if p.workspace.is_some() {
                        self.trust_hooks(
                            &project.id,
                            &self.hooks_for(&project.root, &project.root),
                        )?;
                    }
                }
                Ok(self.scripts_view(&project.id, &hooks))
            }
            "project.untrust" => {
                let key: Key = params(p)?;
                let project = self
                    .store
                    .project(&key.id)
                    .map_err(internal)?
                    .ok_or_else(|| rpc("not_found", &key.id))?;
                self.untrust_project(&project.id)?;
                Ok(json!({ "trusted": false }))
            }
            "workspace.list" => {
                #[derive(Deserialize)]
                struct P {
                    project: Option<String>,
                    /// Rescan the worktrees now rather than at most every few seconds.
                    #[serde(default)]
                    refresh: bool,
                    /// Hidden discovered workspaces too, marked `hidden`.
                    #[serde(default)]
                    include_hidden: bool,
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
                if p.refresh {
                    self.discover(project_id.as_deref(), true).await;
                } else {
                    // Never wait on git here: a repository macOS hasn't let the server into blocks it.
                    let server = Arc::clone(self);
                    let project_id = project_id.clone();
                    tokio::spawn(async move {
                        server.discover(project_id.as_deref(), false).await;
                    });
                }
                let workspaces = if p.include_hidden {
                    self.store.known_workspaces(project_id.as_deref())
                } else {
                    self.store.workspaces(project_id.as_deref())
                }
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
            "workspace.take_agent_tabs" => {
                let key: Key = params(p)?;
                let workspace = self.workspace_by_key(&key.id)?;
                Ok(json!({ "agents": self.take_agent_tabs(&workspace.id) }))
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
                // serde reads an explicit `null` as absent, and `null` is how a link is removed.
                let clear_issue = p.get("issue").is_some_and(Value::is_null);
                let clear_pr = p.get("pull_request").is_some_and(Value::is_null);
                let p: P = params(p)?;
                let mut workspace = self.workspace_by_key(&p.id)?;
                if clear_issue {
                    workspace.issue = None;
                    self.linear.wake_poll();
                }
                if clear_pr {
                    workspace.pull_request = None;
                }
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
                let base_changed = p
                    .base
                    .as_ref()
                    .is_some_and(|base| workspace.base_ref.as_deref().unwrap_or("") != base.trim());
                if let Some(base) = p.base {
                    // An empty base follows the repository's primary branch again, as Orca's "Use Primary".
                    workspace.base_ref =
                        Some(base.trim().to_string()).filter(|base| !base.is_empty());
                }
                if let Some(issue) = p.issue {
                    workspace.issue = if issue.is_null() { None } else { Some(issue) };
                    self.linear.wake_poll();
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
                if base_changed {
                    self.git_writes
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    self.broadcast(
                        crate::gitops::GIT_CHANGED_TOPIC,
                        json!({ "workspace": workspace.id, "op": "base" }),
                    );
                    self.publish_next_action(workspace.clone());
                }
                Ok(self.workspace_json(&workspace))
            }
            "workspace.refs" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    #[serde(default)]
                    query: String,
                    limit: Option<usize>,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.id)?;
                let limit = p.limit.unwrap_or(20).clamp(1, 200);
                let path = workspace.path.clone();
                let refs = tokio::task::spawn_blocking(move || {
                    icarus_git::search_base_refs(&path, &p.query, limit)
                })
                .await
                .map_err(internal)?
                .map_err(|e| rpc("git_failed", format!("{e:#}")))?;
                Ok(json!({
                    "base_ref": workspace.base_ref,
                    "default_base": refs.default_base,
                    "remote_count": refs.remote_count,
                    "refs": refs.refs,
                }))
            }
            "workspace.reorder" => {
                #[derive(Deserialize)]
                struct P {
                    ids: Vec<String>,
                }
                let p: P = params(p)?;
                let ids: Vec<String> = p
                    .ids
                    .iter()
                    .map(|key| self.workspace_by_key(key).map(|workspace| workspace.id))
                    .collect::<Result<_, _>>()?;
                self.store.set_manual_order(&ids).map_err(internal)?;
                self.broadcast(
                    "workspace.changed",
                    json!({ "workspaces": ids, "source": "order" }),
                );
                Ok(json!({ "ordered": ids.len() }))
            }
            "workspace.set_unread" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    unread: bool,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.id)?;
                self.set_unread(&workspace.id, p.unread.then_some(UnreadKind::Done));
                Ok(json!({ "unread": self.unread(&workspace.id) }))
            }
            "workspace.remove" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    #[serde(default)]
                    force: bool,
                    /// Defaults to deleting it only with a worktree Icarus created.
                    delete_branch: Option<bool>,
                    /// Defaults to true only for worktrees Icarus created; others are hidden instead.
                    delete_worktree: Option<bool>,
                    /// Remove without running the project's archive script.
                    #[serde(default)]
                    skip_archive: bool,
                    /// Remove even if the archive script fails (`force` implies it).
                    #[serde(default)]
                    allow_failed_archive: bool,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.id)?;
                if workspace.discovery.origin == crate::discovery::ORIGIN_PRIMARY {
                    return Err(rpc(
                        "primary",
                        format!(
                            "{} is the repository's main checkout and can't be removed",
                            workspace.name
                        ),
                    ));
                }
                let created = self.created_by_icarus(&workspace);
                if !p.delete_worktree.unwrap_or(created) {
                    self.stop_workspace_processes(&workspace.id).await;
                    self.store
                        .set_visibility(&workspace.id, Some("hide"), true)
                        .map_err(internal)?;
                    self.broadcast(
                        "workspace.changed",
                        json!({ "workspaces": [workspace.id], "source": "hide" }),
                    );
                    return Ok(
                        json!({ "removed": true, "hidden": true, "worktree_deleted": false }),
                    );
                }
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
                let shared = self
                    .hooks_for(&project.root, &project.root)
                    .shared_directories;
                let unlinked = unlink_shared_paths(&project.root, &workspace.path, &shared);
                let (root, path, force, delete_branch) = (
                    project.root.clone(),
                    workspace.path.clone(),
                    p.force,
                    p.delete_branch.unwrap_or(created),
                );
                let result = tokio::task::spawn_blocking(move || {
                    if path.exists() {
                        // An adopted worktree can belong to another clone of the project's repository.
                        let repo = icarus_git::repo_info(&path)
                            .map(|info| info.root)
                            .unwrap_or(root);
                        icarus_git::remove_worktree(&repo, &path, force, delete_branch)
                    } else {
                        Ok(())
                    }
                })
                .await
                .map_err(internal)?;
                if result.is_err() && workspace.path.is_dir() {
                    relink_shared_paths(&project.root, &workspace.path, &unlinked);
                }
                match result {
                    Err(RemoveError::Dirty) => return Err(rpc("dirty", RemoveError::Dirty)),
                    Err(error) => return Err(rpc("git_failed", format!("{error:#}"))),
                    Ok(()) => {}
                }
                self.stop_workspace_processes(&workspace.id).await;
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
                Ok(json!({ "removed": true, "archive": archive, "worktree_deleted": true }))
            }
            "workspace.setup" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    #[serde(default)]
                    trust: bool,
                }
                let key: P = params(p)?;
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
                if key.trust {
                    self.trust_hooks(&project.id, &self.hooks_for(&project.root, &workspace.path))?;
                }
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(from = "Value")]
enum Trust {
    #[default]
    No,
    AsTheyAre,
    Always,
}

impl From<Value> for Trust {
    fn from(value: Value) -> Self {
        match value {
            Value::Bool(true) => Self::AsTheyAre,
            Value::String(text) if text == "always" => Self::Always,
            Value::String(text) if text == "once" => Self::AsTheyAre,
            _ => Self::No,
        }
    }
}

impl Trust {
    fn granted(self) -> bool {
        self != Self::No
    }
}

/// The GitHub item a new workspace is linked to (Orca's linked work item): its number and URL.
fn linked_github_item(
    issue: Option<&Value>,
    pr: Option<(u64, &str)>,
) -> Option<(Option<u64>, String)> {
    if let Some((number, url)) = pr {
        return Some((Some(number), url.to_owned()));
    }
    let url = issue?["url"].as_str()?;
    let rest = url.strip_prefix("https://github.com/")?;
    let mut parts = rest.split('/');
    let (_owner, _repo, kind, number) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if !matches!(kind, "issues" | "pull") {
        return None;
    }
    Some((number.parse().ok(), url.to_owned()))
}

#[derive(Deserialize)]
struct CreateParams {
    project: String,
    /// Optional when `issue` links a Linear issue or `pr` names a pull request.
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
    /// Trust the project's committed scripts as they are now (`true`), or always (`"always"`).
    #[serde(default)]
    trust: Trust,
    /// Run the project's issue command for a linked GitHub item (Orca's issue automation).
    #[serde(default = "yes")]
    issue_command: bool,
    /// Start from this pull request (number or URL): its head branch, with its base as the base.
    pr: Option<Value>,
    /// Chosen by the client so `workspace.create_cancel` can name this create.
    create_id: Option<String>,
}

/// What a pull request start point resolved to on GitHub.
struct PrStart {
    head: PrHead,
    base_branch: String,
    url: String,
    title: String,
}

impl Server {
    async fn resolve_pr_start(
        &self,
        project: &crate::state::Project,
        pr: &Value,
    ) -> Result<PrStart, RpcError> {
        let (named, number) = parse_pr(pr)?;
        let repo = crate::github::repo_ref(project).ok_or_else(|| {
            rpc(
                "no_github",
                format!(
                    "{} has no GitHub remote to find pull requests in",
                    project.name
                ),
            )
        })?;
        if let Some((owner, name)) = &named
            && !(owner.eq_ignore_ascii_case(&repo.owner) && name.eq_ignore_ascii_case(&repo.name))
        {
            return Err(rpc(
                "wrong_repository",
                format!(
                    "that pull request is in {owner}/{name}, but {} is {}",
                    project.name,
                    repo.slug()
                ),
            ));
        }
        let details = self
            .github
            .client
            .pull_request(&repo, number)
            .await
            .map_err(|e| rpc("github_failed", format!("pull request #{number}: {e}")))?;
        Ok(PrStart {
            head: PrHead {
                number,
                head_branch: details.pr.head_ref_name.clone(),
                cross_repository: details.is_cross_repository,
            },
            base_branch: details.pr.base_ref_name.clone(),
            url: details.pr.url.clone(),
            title: details.pr.title.clone(),
        })
    }

    async fn create_workspace(
        self: &Arc<Self>,
        p: CreateParams,
        flag: &CancelFlag,
    ) -> Result<Value, RpcError> {
        let project = self
            .store
            .project(&p.project)
            .map_err(internal)?
            .ok_or_else(|| rpc("not_found", format!("project {}", p.project)))?;
        let setup_decision = SetupDecision::parse(p.setup.as_ref())?;
        // Decided before the worktree exists, so an "ask" or untrusted project leaves nothing behind.
        let root_hooks = self.hooks_for(&project.root, &project.root);
        let runs_setup = match root_hooks.setup {
            Some(_) => should_run_setup(root_hooks.run_policy, setup_decision)?,
            None => false,
        };
        if p.trust == Trust::Always {
            self.trust_always(&project.id)?;
        }
        if p.trust.granted() {
            self.trust_hooks(&project.id, &root_hooks)?;
        } else if runs_setup
            && setup_decision != SetupDecision::Auto
            && let Some(shared) = &root_hooks.shared_setup
            && !self.is_trusted(&project.id, ScriptKind::Setup, shared)
        {
            return Err(rpc(
                "untrusted_scripts",
                untrusted_message(&project.id, "setup script"),
            ));
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
        let (from_linear, pr) = or_cancelled(flag, async {
            let from_linear = match &linked {
                Some(identifier) => self.linear_issue_for_create(identifier).await,
                None => None,
            };
            let pr = match &p.pr {
                Some(pr) => Some(self.resolve_pr_start(&project, pr).await?),
                None => None,
            };
            Ok((from_linear, pr))
        })
        .await?;
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
            .or_else(|| pr.as_ref().map(|pr| pr.head.head_branch.clone()))
            .ok_or_else(|| rpc("invalid_params", "name is required"))?;
        let slug = workspace_slug(&name, SLUG_MAX);
        let repo_dir = workspace_slug(&project.name, SLUG_MAX);
        let path = self.workspace_root.join(repo_dir).join(&slug);
        if path.exists() {
            return Err(rpc("exists", format!("{} already exists", path.display())));
        }
        let branch = p
            .branch
            .or_else(|| pr.as_ref().map(|pr| pr.head.head_branch.clone()))
            .or_else(|| from_linear.as_ref()?.branch_name.clone())
            .unwrap_or_else(|| slug.clone());
        let base = p
            .base
            .or_else(|| pr.as_ref().map(|pr| format!("origin/{}", pr.base_branch)))
            .or_else(|| project.base_ref.clone())
            .ok_or_else(|| rpc("no_base", "project has no base ref; pass base"))?;
        let plan = WorktreePlan {
            repo: project.root.clone(),
            path: path.clone(),
            branch_existed: branch_exists(&project.root, &branch).await,
            branch: branch.clone(),
            start: match &pr {
                Some(pr) => pull_ref(pr.head.number),
                None => base.clone(),
            },
            fetch: (p.fetch || pr.is_some()).then(|| base.clone()),
            pr: pr.as_ref().map(|pr| pr.head.clone()),
        };
        let prepared = or_cancelled(flag, async {
            add_worktree(&plan).await?;
            let includes = read_worktree_include(&project.root);
            Ok(materialize_shared_paths(
                &project.root,
                &path,
                &root_hooks.shared_directories,
                &includes,
            )
            .await)
        })
        .await;
        let warnings = match prepared {
            Ok(warnings) => warnings,
            Err(error) => {
                discard_worktree(&plan).await;
                return Err(error);
            }
        };
        for warning in &warnings {
            tracing::warn!("shared paths for {slug}: {warning}");
        }
        if p.trust.granted() {
            self.trust_hooks(&project.id, &self.hooks_for(&project.root, &path))?;
        }
        let workspace = Workspace {
            id: format!("ws-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]),
            project_id: project.id.clone(),
            name: slug,
            path: path.clone(),
            branch: Some(branch),
            base_ref: Some(base),
            status: p.status,
            title: p.title.or_else(|| pr.as_ref().map(|pr| pr.title.clone())),
            issue,
            pull_request: pr
                .as_ref()
                .map(|pr| json!({ "number": pr.head.number, "url": pr.url })),
            created_at_ms: now_ms(),
            pinned: false,
            // Orca stamps a new workspace at the top of the Manual order.
            manual_order: Some(now_ms() as i64),
            activity_at_ms: None,
            discovery: Discovery::default(),
        };
        self.store
            .insert_owned_workspace(&workspace)
            .map_err(internal)?;
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
                permission_mode: None,
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
                            tracing::warn!("first prompt after setup of {workspace_id}: {error:#}");
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
        let layout = p.layout.or_else(|| {
            p.agent
                .then(|| icarus_protocol::layout::DEFAULT_LAYOUT.to_owned())
        });
        // Orca opens `defaultTabs` when the workspace first opens, so only a workspace with a layout gets them.
        let mut default_tabs = if layout.is_some() {
            self.start_default_tabs(&workspace, &project.root)
        } else {
            Vec::new()
        };
        let mut warnings = warnings;
        if p.issue_command
            && let Some((number, url)) = linked_github_item(
                workspace.issue.as_ref(),
                pr.as_ref().map(|pr| (pr.head.number, pr.url.as_str())),
            )
        {
            match self.start_issue_command(&workspace, &project.root, number, &url) {
                Ok(Some(terminal)) => default_tabs.push(terminal),
                Ok(None) => {}
                Err(reason) => warnings.push(format!("the issue command didn't run: {reason}")),
            }
        }
        let mut value = self.workspace_json(&workspace);
        if let Some(layout) = layout {
            value["layout"] = self
                .apply_layout_with(
                    &workspace,
                    &layout,
                    p.pr_url.or_else(|| pr.map(|pr| pr.url)),
                    &default_tabs,
                )
                .await?;
        }
        if !warnings.is_empty() {
            value["warnings"] = json!(warnings);
        }
        Ok(value)
    }

    async fn adopt_workspace(&self, p: AdoptParams) -> Result<Value, RpcError> {
        let inspecting = tokio::task::spawn_blocking(move || inspect_worktree(&p.path));
        let found = tokio::time::timeout(Duration::from_secs(20), inspecting)
            .await
            .map_err(|_| {
                rpc(
                    "timeout",
                    "git did not answer for the worktree in time; if it is in a protected folder, allow the server in macOS Privacy & Security settings",
                )
            })?
            .map_err(internal)??;
        let project = match &p.project {
            Some(key) => self
                .store
                .project(key)
                .map_err(internal)?
                .ok_or_else(|| rpc("not_found", format!("project {key}")))?,
            None => self.project_for_repo(&found.repo)?.ok_or_else(|| {
                rpc(
                    "no_project",
                    format!(
                        "no project matches the repository at {}; add it with `icarus project add {}`, or pass --project",
                        found.repo.root.display(),
                        found.repo.root.display()
                    ),
                )
            })?,
        };
        let existing = self.store.all_workspaces(None).map_err(internal)?;
        let same = existing
            .iter()
            .find(|workspace| icarus_git::same_path(&workspace.path, &found.path));
        if let Some(workspace) = same.filter(|w| w.discovery.discovered) {
            // Discovery got there first: adopting keeps its record and shows it whatever the settings.
            let name = match p.name.filter(|name| !name.trim().is_empty()) {
                Some(name) => workspace_slug(&name, SLUG_MAX),
                None => workspace.name.clone(),
            };
            if existing.iter().any(|other| {
                other.id != workspace.id
                    && other.project_id == workspace.project_id
                    && other.name == name
            }) {
                return Err(rpc(
                    "exists",
                    format!("the project already has a workspace named {name}; pass --name"),
                ));
            }
            self.store
                .adopt_discovered(&workspace.id, &name)
                .map_err(internal)?;
            let adopted = self.workspace_by_key(&workspace.id)?;
            self.broadcast(
                "workspace.changed",
                json!({ "workspaces": [adopted.id], "source": "adopt" }),
            );
            return Ok(self.workspace_json(&adopted));
        }
        if let Some(workspace) = same.filter(|w| w.discovery.missing_at_ms.is_none()) {
            return Err(rpc(
                "exists",
                format!(
                    "{} is already the workspace {}",
                    found.path.display(),
                    workspace.name
                ),
            ));
        }
        let name = p
            .name
            .filter(|name| !name.trim().is_empty())
            .or_else(|| {
                found
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        let slug = workspace_slug(&name, SLUG_MAX);
        if existing.iter().any(|workspace| {
            workspace.project_id == project.id
                && workspace.name == slug
                && workspace.discovery.missing_at_ms.is_none()
        }) {
            return Err(rpc(
                "exists",
                format!(
                    "{} already has a workspace named {slug}; pass --name",
                    project.name
                ),
            ));
        }
        let workspace = Workspace {
            id: format!("ws-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]),
            project_id: project.id.clone(),
            name: slug,
            path: found.path,
            branch: Some(found.branch),
            base_ref: project.base_ref.clone(),
            status: None,
            title: None,
            issue: None,
            pull_request: None,
            created_at_ms: now_ms(),
            pinned: false,
            manual_order: Some(now_ms() as i64),
            activity_at_ms: None,
            discovery: Discovery {
                origin: ORIGIN_ADOPTED.into(),
                ..Discovery::default()
            },
        };
        self.store
            .insert_owned_workspace(&workspace)
            .map_err(internal)?;
        self.broadcast(
            "workspace.changed",
            json!({ "workspaces": [workspace.id], "source": "adopt" }),
        );
        Ok(self.workspace_json(&workspace))
    }

    /// Only worktrees Icarus made itself, in its own folder, may be deleted without being asked to.
    fn created_by_icarus(&self, workspace: &Workspace) -> bool {
        let canonical = |path: &std::path::Path| {
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        };
        workspace.discovery.origin == ORIGIN_NATIVE
            && !workspace.discovery.discovered
            && canonical(&workspace.path).starts_with(canonical(&self.workspace_root))
    }

    async fn stop_workspace_processes(&self, workspace_id: &str) {
        for agent in self.agents.all() {
            if agent.info().workspace.as_deref() == Some(workspace_id) {
                let _ = agent.kill().await;
                self.agents.remove(agent.id());
            }
        }
        for terminal in self.terminals.list() {
            if terminal.workspace.as_deref() == Some(workspace_id)
                && let Some(session) = self.terminals.remove(&terminal.id)
            {
                let _ = session.kill();
            }
        }
    }

    /// The project registered for this repository's main checkout, else for its canonical remote.
    fn project_for_repo(
        &self,
        repo: &icarus_git::RepoInfo,
    ) -> Result<Option<crate::state::Project>, RpcError> {
        if let Some(project) = self.store.project_by_root(&repo.root).map_err(internal)? {
            return Ok(Some(project));
        }
        let Some(remote) = &repo.remote else {
            return Ok(None);
        };
        Ok(self
            .store
            .projects()
            .map_err(internal)?
            .into_iter()
            .find(|project| project.remote.as_ref() == Some(remote)))
    }
}

#[derive(Deserialize)]
struct AdoptParams {
    path: PathBuf,
    /// Overrides matching the worktree's repository to a project.
    project: Option<String>,
    /// Defaults to the worktree's folder name.
    name: Option<String>,
}

/// A linked worktree that `workspace.adopt` can record as it is.
struct FoundWorktree {
    path: PathBuf,
    branch: String,
    repo: icarus_git::RepoInfo,
}

fn inspect_worktree(path: &std::path::Path) -> Result<FoundWorktree, RpcError> {
    let shown = path.display();
    let output = icarus_git::git_timeout(
        path,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ],
        GIT_TIMEOUT,
    )
    .map_err(|e| {
        rpc(
            "not_a_worktree",
            format!("{shown} is not a git worktree: {e:#}"),
        )
    })?;
    let mut lines = output.lines().map(PathBuf::from);
    let (Some(top), Some(git_dir), Some(common)) = (lines.next(), lines.next(), lines.next())
    else {
        return Err(rpc(
            "not_a_worktree",
            format!("{shown} is not a git worktree"),
        ));
    };
    if icarus_git::same_path(&git_dir, &common) {
        return Err(rpc(
            "main_checkout",
            format!(
                "{} is a repository's main checkout, not a linked worktree; register it with `icarus project add`",
                top.display()
            ),
        ));
    }
    let branch = icarus_git::git_timeout(
        &top,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        GIT_TIMEOUT,
    )
    .map(|branch| branch.trim().to_owned())
    .ok()
    .filter(|branch| !branch.is_empty())
    .ok_or_else(|| {
        rpc(
            "detached_head",
            format!(
                "{} has a detached HEAD; check out a branch in it first",
                top.display()
            ),
        )
    })?;
    let repo = icarus_git::repo_info(&top).map_err(|e| rpc("not_a_repo", format!("{e:#}")))?;
    Ok(FoundWorktree {
        path: top,
        branch,
        repo,
    })
}

async fn repo_info_off_thread(path: &std::path::Path) -> Result<icarus_git::RepoInfo, RpcError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || icarus_git::repo_info(&path))
        .await
        .map_err(internal)?
        .map_err(|e| rpc("not_a_repo", format!("{e:#}")))
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

fn unread_key(workspace_id: &str) -> String {
    format!("workspace.unread.{workspace_id}")
}

fn agent_tabs_key(workspace_id: &str) -> String {
    format!("workspace.agent_tabs.{workspace_id}")
}

/// Offers and withdrawals read, change and write one list; this keeps them from interleaving.
static AGENT_TABS: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Server {
    /// Remembers an agent the server started for a workspace until an app shows it as a tab there,
    /// and tells the apps, which open it at once if that workspace is open.
    pub(crate) fn offer_agent_tab(&self, agent: &crate::agent::AgentInfo) {
        let Some(workspace) = self.agent_workspace(agent) else {
            return;
        };
        self.edit_agent_tabs(&workspace.id, |ids| {
            if !ids.contains(&agent.id) {
                ids.push(agent.id.clone());
            }
        });
        self.send_to_apps(
            "agent_tabs",
            json!({ "workspace_id": workspace.id, "path": workspace.path }),
        );
    }

    /// Something attached to the agent (an agent tab, usually), so no app needs to open one for it.
    pub(crate) fn withdraw_agent_tab(&self, agent: &crate::agent::AgentInfo) {
        let Some(workspace) = self.agent_workspace(agent) else {
            return;
        };
        self.edit_agent_tabs(&workspace.id, |ids| ids.retain(|id| id != &agent.id));
    }

    fn agent_workspace(&self, agent: &crate::agent::AgentInfo) -> Option<Workspace> {
        self.store
            .workspace(agent.workspace.as_deref()?)
            .ok()
            .flatten()
    }

    /// The offered agents of a workspace that still run (or sleep), handed out once.
    pub(crate) fn take_agent_tabs(&self, workspace_id: &str) -> Vec<String> {
        let mut taken = Vec::new();
        self.edit_agent_tabs(workspace_id, |ids| taken = std::mem::take(ids));
        taken
            .into_iter()
            .filter(|id| {
                self.agents
                    .get(id)
                    .is_some_and(|agent| agent.state() != AgentState::Exited)
            })
            .collect()
    }

    fn edit_agent_tabs(&self, workspace_id: &str, edit: impl FnOnce(&mut Vec<String>)) {
        let _guard = AGENT_TABS.lock().unwrap_or_else(|p| p.into_inner());
        let key = agent_tabs_key(workspace_id);
        let mut ids: Vec<String> = self
            .store
            .kv_get(&key)
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        let before = ids.clone();
        edit(&mut ids);
        if ids == before {
            return;
        }
        let stored = if ids.is_empty() {
            self.store.kv_delete(&key).map(|_| ())
        } else {
            self.store.kv_set(&key, &json!(ids))
        };
        if let Err(error) = stored {
            tracing::warn!("agent tabs for {workspace_id}: {error:#}");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnreadKind {
    /// A turn ended (or failed) while nobody looked.
    Done,
    /// The agent asked a question or hit a permission it could not get.
    NeedsInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unread {
    pub kind: UnreadKind,
    pub at_ms: u64,
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
