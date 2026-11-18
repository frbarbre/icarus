//! User automation scripts (spec/automations.md): build a scope, run the project's script on it, apply
//! the actions it returns and record the run.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use icarus_protocol::RpcError;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::next_action::setting;
use crate::server::{Server, params as rpc_params, rpc};
use crate::state::{Project, Workspace};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

const DEFAULT_INTERVAL_MS: u64 = 60_000;
const MIN_INTERVAL_MS: u64 = 5_000;
const DEFAULT_MAX_AGENTS: u64 = 3;
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(60);
const MEMORY_LIMIT: usize = 256 * 1024;
const OUTPUT_LIMIT: usize = 512 * 1024;
const KEEP_RECENT_MS: u64 = 24 * 3600 * 1000;
const KEEP_RECENT_RUNS: i64 = 500;
const KEEP_NOTABLE_MS: u64 = 7 * 24 * 3600 * 1000;
/// Methods that start an agent, counted against `automations.max_agents_per_run`.
const AGENT_METHODS: &[&str] = &["agent.create", "workspace.run_next_action"];
/// A script can't drive the engine itself, the server's lifecycle, or event streams it can't read.
const DENIED_PREFIXES: &[&str] = &["automation.", "server.", "client.drive"];
const DENIED_SUFFIXES: &[&str] = &[".attach", ".detach", ".wait", ".events"];
const BASE_TYPES: &str = include_str!("automations.d.ts");

/// Methods a script may return as actions; the generated `Method` type lists them.
pub(crate) const METHODS: &[&str] = &[
    "agent.answer",
    "agent.create",
    "agent.interrupt",
    "agent.kill",
    "agent.send",
    "agent.set_mode",
    "agent.set_model",
    "agent.sleep",
    "git.commit",
    "git.pull",
    "git.push",
    "git.sync",
    "issue.assign",
    "issue.refresh",
    "note.add",
    "note.resolve",
    "notification.clear",
    "notification.send",
    "pr.operate",
    "review.request_reviewers",
    "review.set_base",
    "review.workspace",
    "tab.open",
    "terminal.create",
    "terminal.kill",
    "terminal.write",
    "workspace.create",
    "workspace.layout",
    "workspace.remove",
    "workspace.run_next_action",
    "workspace.set",
    "workspace.set_unread",
    "workspace.sleep",
    "workspace.visibility",
    "workspace.sync_to_pr_head",
    "workspace.wake",
];

#[derive(Default)]
pub struct Automations {
    db: Mutex<Option<Connection>>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    buttons: Mutex<HashMap<String, Buttons>>,
    /// Runs whose script is running now: whether each is dry, and the `icarus` calls it made.
    active: Mutex<HashMap<String, ActiveRun>>,
    last_types: Mutex<Option<String>>,
    dirty: tokio::sync::Notify,
    /// Per project, each workspace's scope as of the last event check.
    seen: Mutex<HashMap<String, HashMap<String, Value>>>,
}

const EVENT_TOPICS: &[&str] = &[
    "workspace.changed",
    crate::gitops::GIT_CHANGED_TOPIC,
    "agent.state",
    "config.changed",
];
const EVENT_DEBOUNCE: Duration = Duration::from_millis(1500);

impl Automations {
    pub(crate) fn observe(&self, topic: &str) {
        if EVENT_TOPICS.contains(&topic) {
            self.dirty.notify_one();
        }
    }
}

#[derive(Default)]
struct ActiveRun {
    dry_run: bool,
    calls: Vec<Value>,
}

/// Calls from a script's own `icarus` that the server records against its run.
pub(crate) fn watched(method: &str) -> bool {
    METHODS.contains(&method) || denied(method)
}

/// What a script decided to show as one workspace's buttons, and which run decided it.
#[derive(Clone, Debug)]
pub(crate) struct Buttons {
    pub run: String,
    pub list: Vec<Value>,
}

#[derive(Debug, Clone)]
struct Config {
    enabled: bool,
    armed: bool,
    interval_ms: u64,
    max_agents: u64,
    hold: Option<String>,
    default_project: Option<String>,
    scripts: BTreeMap<String, String>,
}

fn config(settings: &Value) -> Config {
    let get = |key: &str| setting(settings, &["automations", key]);
    Config {
        enabled: get("enabled").and_then(Value::as_bool).unwrap_or(true),
        armed: get("armed").and_then(Value::as_bool).unwrap_or(false),
        interval_ms: get("interval_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_INTERVAL_MS)
            .max(MIN_INTERVAL_MS),
        max_agents: get("max_agents_per_run")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_AGENTS),
        hold: get("hold_status")
            .and_then(Value::as_str)
            .map(str::to_owned),
        default_project: get("default_project")
            .and_then(Value::as_str)
            .map(str::to_owned),
        scripts: get("scripts")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn script_path(config: &Config, project: &Project) -> Option<PathBuf> {
    let raw = config
        .scripts
        .get(&project.name)
        .or_else(|| config.scripts.get(&project.id))?;
    let path = match raw.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()?.join(rest),
        None => PathBuf::from(raw),
    };
    if path.is_absolute() {
        Some(path)
    } else {
        Some(icarus_settings::layers::config_dir()?.join(path))
    }
}

pub(crate) fn generated_dir() -> Option<PathBuf> {
    Some(
        icarus_settings::layers::config_dir()?
            .join("automations")
            .join(".generated"),
    )
}

fn hash(value: &Value) -> String {
    let digest = Sha256::digest(value.to_string().as_bytes());
    digest.iter().take(12).map(|b| format!("{b:02x}")).collect()
}

fn lookup<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .filter(|s| !s.is_empty())
        .try_fold(value, |v, key| match v {
            Value::Array(items) => items.get(key.parse::<usize>().ok()?),
            _ => v.get(key),
        })
}

/// Substitutes each `{{path}}` from `context`; a missing path renders empty.
pub(crate) fn render_template(template: &str, context: &Value) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let Some(end) = rest[start + 2..].find("}}") else {
            break;
        };
        out.push_str(&rest[..start]);
        let path = rest[start + 2..start + 2 + end].trim();
        match lookup(context, path) {
            Some(Value::String(s)) => out.push_str(s),
            Some(Value::Null) | None => {}
            Some(other) => out.push_str(&other.to_string()),
        }
        rest = &rest[start + 2 + end + 2..];
    }
    out.push_str(rest);
    out
}

/// What a prompt template sees for one workspace.
fn prompt_context(scope: &Value, workspace: &Value) -> Value {
    json!({
        "project": scope.get("project"),
        "workspace": workspace,
        "pr": workspace.get("pr"),
        "git": workspace.get("git"),
        "issue": workspace.get("issue"),
        "linear": workspace.get("issue"),
        "trigger": scope.get("trigger"),
    })
}

fn scope_workspace<'a>(scope: &'a Value, id: &str) -> Option<&'a Value> {
    scope
        .get("workspaces")?
        .as_array()?
        .iter()
        .find(|w| w.get("id").and_then(Value::as_str) == Some(id))
}

#[derive(Debug, Default)]
struct Decision {
    actions: Vec<Value>,
    memory: Option<Value>,
    buttons: Option<Map<String, Value>>,
}

/// Reads the script's stdout: an action array, or `{ actions, memory, buttons }`. Logging belongs on
/// stderr, but a script that prints lines before its JSON still works: the last line is tried too.
fn parse_decision(stdout: &str) -> Result<Decision, String> {
    let text = stdout.trim();
    if text.is_empty() {
        return Ok(Decision::default());
    }
    let value: Value = serde_json::from_str(text)
        .or_else(|first| {
            text.lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .and_then(|l| serde_json::from_str(l).ok())
                .ok_or(first)
        })
        .map_err(|e| format!("stdout is not JSON: {e}"))?;
    match value {
        Value::Array(actions) => Ok(Decision {
            actions,
            ..Decision::default()
        }),
        Value::Object(mut map) => {
            let actions = match map.remove("actions") {
                Some(Value::Array(a)) => a,
                None | Some(Value::Null) => Vec::new(),
                Some(_) => return Err("`actions` must be an array".into()),
            };
            let buttons = match map.remove("buttons") {
                Some(Value::Object(b)) => Some(b),
                None | Some(Value::Null) => None,
                Some(_) => return Err("`buttons` must map workspace ids to lists".into()),
            };
            Ok(Decision {
                actions,
                memory: map.remove("memory"),
                buttons,
            })
        }
        _ => Err("stdout must be an array of actions or an object".into()),
    }
}

#[derive(Debug, Deserialize)]
struct Action {
    method: String,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    force: bool,
}

fn action_workspace(action: &Action) -> Option<String> {
    let params = &action.params;
    params
        .get("workspace")
        .and_then(Value::as_str)
        .or_else(|| {
            action
                .method
                .starts_with("workspace.")
                .then(|| params.get("id").and_then(Value::as_str))
                .flatten()
        })
        .map(str::to_owned)
}

fn denied(method: &str) -> bool {
    DENIED_PREFIXES.iter().any(|p| method.starts_with(p))
        || DENIED_SUFFIXES.iter().any(|s| method.ends_with(s))
}

struct ScriptOutput {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    error: Option<String>,
}

fn clip(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(OUTPUT_LIMIT)]).into_owned()
}

async fn run_script(
    path: &Path,
    input: &[u8],
    run_id: &str,
    dry_run: bool,
    home: &Path,
) -> ScriptOutput {
    let failed = |error: String| ScriptOutput {
        stdout: String::new(),
        stderr: String::new(),
        code: None,
        error: Some(error),
    };
    if !path.exists() {
        return failed(format!("script {} does not exist", path.display()));
    }
    let typescript = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("ts" | "mts" | "js" | "mjs")
    );
    let mut command = if typescript {
        let mut c = tokio::process::Command::new("bun");
        c.arg(path);
        c
    } else {
        tokio::process::Command::new(path)
    };
    if let Some(dir) = path.parent() {
        command.current_dir(dir);
    }
    command
        .env("ICARUS_RUN_ID", run_id)
        .env("ICARUS_HOME", home)
        .env("ICARUS_DRY_RUN", if dry_run { "1" } else { "0" })
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let program = if typescript {
                "bun".to_owned()
            } else {
                path.display().to_string()
            };
            return failed(format!("could not start {program}: {error}"));
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let input = input.to_vec();
        tokio::spawn(async move {
            let _ = stdin.write_all(&input).await;
        });
    }
    match tokio::time::timeout(SCRIPT_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => ScriptOutput {
            stdout: clip(&output.stdout),
            stderr: clip(&output.stderr),
            code: output.status.code(),
            error: (!output.status.success())
                .then(|| format!("script exited with {}", output.status)),
        },
        Ok(Err(error)) => failed(error.to_string()),
        Err(_) => failed(format!(
            "script timed out after {}s",
            SCRIPT_TIMEOUT.as_secs()
        )),
    }
}

/// How a run was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wait {
    /// A tick: skipped while the project's previous run is still going.
    Skip,
    /// An explicit request: queues behind it.
    Queue,
}

impl Automations {
    fn with_db<T>(
        &self,
        home: &Path,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> Result<T, RpcError> {
        let mut guard = self.db.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            let connection = Connection::open(home.join("automations.db"))
                .and_then(|c| {
                    c.execute_batch(
                        "PRAGMA journal_mode = WAL;
                         CREATE TABLE IF NOT EXISTS runs (
                            id TEXT PRIMARY KEY, project TEXT NOT NULL, started_ms INTEGER NOT NULL,
                            duration_ms INTEGER NOT NULL, trigger TEXT NOT NULL, scope TEXT NOT NULL,
                            dry_run INTEGER NOT NULL, failed INTEGER NOT NULL, acted INTEGER NOT NULL,
                            record TEXT NOT NULL);
                         CREATE INDEX IF NOT EXISTS runs_by_project ON runs(project, started_ms);
                         CREATE TABLE IF NOT EXISTS scopes (hash TEXT PRIMARY KEY, scope TEXT NOT NULL);",
                    )?;
                    Ok(c)
                })
                .map_err(|e| rpc("internal", format!("automations.db: {e}")))?;
            *guard = Some(connection);
        }
        let connection = guard.as_ref().ok_or_else(|| rpc("internal", "no db"))?;
        f(connection).map_err(|e| rpc("internal", e))
    }

    fn lock_for(&self, project: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            self.locks
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entry(project.to_owned())
                .or_default(),
        )
    }

    fn begin(&self, run: &str, dry_run: bool) {
        self.active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                run.to_owned(),
                ActiveRun {
                    dry_run,
                    calls: Vec::new(),
                },
            );
    }

    fn finish(&self, run: &str) -> Vec<Value> {
        self.active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(run)
            .map(|r| r.calls)
            .unwrap_or_default()
    }

    pub(crate) fn buttons(&self, workspace: &str) -> Option<Buttons> {
        self.buttons
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(workspace)
            .cloned()
    }
}

/// Starts the ticks: one run per project with a script, only while an app window is connected.
pub(crate) fn spawn(server: Arc<Server>) {
    let events = Arc::clone(&server);
    tokio::spawn(async move {
        events.automation_events().await;
        loop {
            events.automations.dirty.notified().await;
            tokio::time::sleep(EVENT_DEBOUNCE).await;
            events.automation_events().await;
        }
    });
    tokio::spawn(async move {
        if let Err(error) = server.write_automation_types(false) {
            tracing::warn!("automation types: {}", error.message);
        }
        loop {
            let interval = config(&server.resolved_settings(None)).interval_ms;
            tokio::time::sleep(Duration::from_millis(interval)).await;
            if server.app_count() == 0 {
                continue;
            }
            let Ok(projects) = server.store.projects() else {
                continue;
            };
            for project in projects {
                let server = Arc::clone(&server);
                tokio::spawn(async move {
                    let settings = server.resolved_settings(Some(&project.root));
                    let config = config(&settings);
                    if !config.enabled || script_path(&config, &project).is_none() {
                        return;
                    }
                    if let Err(error) = server
                        .automation_run(&project, json!({ "kind": "tick" }), false, Wait::Skip)
                        .await
                    {
                        tracing::warn!("automation of {}: {}", project.name, error.message);
                    }
                });
            }
        }
    });
}

impl Server {
    /// Diffs each scripted project's workspaces against the last check and runs its script once with what changed.
    async fn automation_events(self: &Arc<Self>) {
        let Ok(projects) = self.store.projects() else {
            return;
        };
        for project in projects {
            let settings = self.resolved_settings(Some(&project.root));
            let config = config(&settings);
            if !config.enabled || script_path(&config, &project).is_none() {
                continue;
            }
            let core = self.automation_scope(&project, &settings).await;
            let now: HashMap<String, Value> = core["workspaces"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|w| Some((w["id"].as_str()?.to_owned(), w.clone())))
                .collect();
            let before = self
                .automations
                .seen
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(project.id.clone(), now.clone());
            // Events while no app is open are dropped, not replayed (the spec's "only while the app is open").
            let Some(before) = before.filter(|_| self.app_count() > 0) else {
                continue;
            };
            let events = diff_events(&before, &now);
            let Some(first) = events.first() else {
                continue;
            };
            let mut trigger = first.clone();
            trigger["kind"] = json!("event");
            trigger["events"] = json!(events);
            let server = Arc::clone(self);
            tokio::spawn(async move {
                if let Err(error) = server
                    .automation_run(&project, trigger, false, Wait::Queue)
                    .await
                {
                    tracing::warn!("automation event of {}: {}", project.name, error.message);
                }
            });
        }
    }

    /// Everything the script sees, apart from the trigger, time, memory and previous scope.
    async fn automation_scope(self: &Arc<Self>, project: &Project, settings: &Value) -> Value {
        let workspaces = self.store.workspaces(Some(&project.id)).unwrap_or_default();
        let agents = self.agents.list();
        let mut list = Vec::new();
        for workspace in &workspaces {
            list.push(self.scope_workspace(workspace, &agents).await);
        }
        let inbox = self.scope_inbox(project, &workspaces).await;
        json!({
            "inbox": inbox,
            "project": {
                "id": project.id,
                "name": project.name,
                "root": project.root,
                "remote": project.remote,
                "base": project.base_ref,
            },
            "workspaces": list,
            "settings": {
                "statuses": setting(settings, &["workspace", "statuses"]).cloned().unwrap_or(json!([])),
                "prompts": setting(settings, &["prompts"]).cloned().unwrap_or(json!({})),
            },
            "viewer": self.github.viewer(),
            "icarus_version": crate::updates::VERSION,
        })
    }

    /// Open PRs requesting the viewer's review, from the cached inbox query; `workspace` is the one already on its branch.
    async fn scope_inbox(&self, project: &Project, workspaces: &[Workspace]) -> Value {
        let Some(repo) = crate::github::repo_ref(project) else {
            return json!([]);
        };
        match self.github.review_inbox(std::slice::from_ref(&repo)).await {
            Ok(inbox) => json!(
                inbox
                    .pulls
                    .iter()
                    .map(|d| {
                        let workspace = workspaces
                            .iter()
                            .find(|w| {
                                w.branch.as_deref() == Some(d.pr.head_ref_name.as_str())
                                    || w.pull_request
                                        .as_ref()
                                        .is_some_and(|l| l["number"] == json!(d.pr.number))
                            })
                            .map(|w| w.id.clone());
                        let mut pr = serde_json::to_value(&d.pr).unwrap_or(Value::Null);
                        pr["workspace"] = json!(workspace);
                        pr["is_cross_repository"] = json!(d.is_cross_repository);
                        pr
                    })
                    .collect::<Vec<_>>()
            ),
            Err(error) => json!({ "error": error.to_string() }),
        }
    }

    async fn scope_workspace(
        self: &Arc<Self>,
        workspace: &Workspace,
        agents: &[crate::agent::AgentInfo],
    ) -> Value {
        let mut value = json!({
            "id": workspace.id,
            "name": workspace.name,
            "title": workspace.title,
            "path": workspace.path,
            "branch": workspace.branch,
            "base": workspace.base_ref,
            "status": workspace.status,
            "pinned": workspace.pinned,
            "hidden": workspace.discovery.hidden,
            "origin": workspace.discovery.origin,
            "created_ms": workspace.created_at_ms,
            "activity_ms": workspace.activity_at_ms,
            "issue": workspace.issue,
            "git": null,
            "pr": null,
            "suggested_action": null,
            "linked_pr": workspace.pull_request,
            "review_pending": self.unsent_drafts(&workspace.id),
            "agents": agents
                .iter()
                .filter(|a| a.workspace.as_deref() == Some(workspace.id.as_str()))
                .map(|a| json!({
                    "id": a.id,
                    "provider": a.provider,
                    "title": a.title,
                    "state": a.state,
                    "mode": a.mode,
                    "turns": a.turns,
                    "created_ms": a.created_at_ms,
                    "last_activity_ms": a.last_activity_ms,
                }))
                .collect::<Vec<_>>(),
        });
        match self.resolve_next(workspace).await {
            Ok((action, local, view)) => {
                value["git"] = json!({
                    "branch": local.branch,
                    "head": local.head_oid,
                    "upstream": local.upstream,
                    "ahead": local.unpushed_commits,
                    "behind": local.behind_upstream,
                    "uncommitted": local.has_uncommitted_changes,
                    "conflicts": local.has_local_conflicts,
                    "default_branch": local.is_default_branch,
                });
                if let Some(pr) = &view.pr {
                    let mut pr = serde_json::to_value(pr).unwrap_or(Value::Null);
                    if let Some(details) = &view.details {
                        pr["additions"] = json!(details.additions);
                        pr["deletions"] = json!(details.deletions);
                        pr["changed_files"] = json!(details.changed_files);
                    }
                    value["pr"] = pr;
                }
                if let Some(pr) = &view.pr {
                    value["head_is_local"] =
                        json!(local.head_oid.as_deref() == Some(pr.head_ref_oid.as_str()));
                    value["base_follows_target"] = json!(icarus_rules::facts::base_follows(
                        workspace.base_ref.as_deref(),
                        &pr.base_ref_name
                    ));
                }
                value["pr_known"] = json!(view.known);
                if let Some(error) = &view.error {
                    value["pr_error"] = json!(error);
                }
                value["suggested_action"] = crate::next_action::action_json(action);
            }
            Err(error) => value["git_error"] = json!(error.message),
        }
        value
    }

    fn automation_project(&self, key: &str) -> Result<Project, RpcError> {
        self.store
            .project(key)
            .map_err(|e| rpc("internal", e))?
            .ok_or_else(|| rpc("not_found", format!("project {key}")))
    }

    /// The project owning `path`: the deepest workspace or project root containing it.
    fn project_for_path(&self, path: &Path) -> Result<Project, RpcError> {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let projects = self.store.projects().map_err(|e| rpc("internal", e))?;
        let mut best: Option<(usize, String)> = None;
        let mut consider = |root: &Path, project: &str| {
            let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
            let depth = root.components().count();
            if path.starts_with(&root) && best.as_ref().is_none_or(|(d, _)| depth > *d) {
                best = Some((depth, project.to_owned()));
            }
        };
        for project in &projects {
            consider(&project.root, &project.id);
        }
        for workspace in self.store.workspaces(None).unwrap_or_default() {
            consider(&workspace.path, &workspace.project_id);
        }
        let (_, id) = best.ok_or_else(|| {
            rpc(
                "not_found",
                format!("no project contains {}", path.display()),
            )
        })?;
        self.automation_project(&id)
    }

    /// Runs the project's script once and applies (or, dry, only records) what it decides.
    pub(crate) async fn automation_run(
        self: &Arc<Self>,
        project: &Project,
        trigger: Value,
        force_dry: bool,
        wait: Wait,
    ) -> Result<Option<Value>, RpcError> {
        let settings = self.resolved_settings(Some(&project.root));
        let config = config(&settings);
        if !config.enabled {
            return Err(rpc("disabled", "automations.enabled is false"));
        }
        let script = script_path(&config, project).ok_or_else(|| {
            rpc(
                "no_script",
                format!(
                    "no script for {}: set automations.scripts.{}",
                    project.name, project.name
                ),
            )
        })?;
        let lock = self.automations.lock_for(&project.id);
        let _guard = match wait {
            Wait::Queue => lock.lock().await,
            Wait::Skip => match lock.try_lock() {
                Ok(guard) => guard,
                Err(_) => return Ok(None),
            },
        };
        let started = now_ms();
        let run_id = format!("run-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let dry_run = force_dry || !config.armed;
        let memory_key = format!("automations.memory.{}", project.id);
        let previous_key = format!("automations.previous.{}", project.id);
        let memory_before = self
            .store
            .kv_get(&memory_key)
            .ok()
            .flatten()
            .unwrap_or_else(|| json!({}));
        let previous = self
            .store
            .kv_get(&previous_key)
            .ok()
            .flatten()
            .unwrap_or(Value::Null);
        let core = self.automation_scope(project, &settings).await;
        let scope = full_scope(
            &core,
            &trigger,
            started,
            &memory_before,
            &previous,
            dry_run,
            &run_id,
        );
        self.automations.begin(&run_id, dry_run);
        let output = run_script(
            &script,
            scope.to_string().as_bytes(),
            &run_id,
            dry_run,
            &self.paths.home,
        )
        .await;
        let cli_calls = self.automations.finish(&run_id);
        let decision = match &output.error {
            Some(_) => Ok(Decision::default()),
            None => parse_decision(&output.stdout),
        };
        let (decision, decision_error) = match decision {
            Ok(d) => (d, None),
            Err(e) => (Decision::default(), Some(e)),
        };
        let results = if dry_run {
            decision
                .actions
                .iter()
                .enumerate()
                .map(|(i, a)| json!({ "index": i, "action": a, "dry_run": true }))
                .collect()
        } else {
            self.apply_actions(&decision.actions, &config, &scope).await
        };
        let outcomes = || results.iter().chain(&cli_calls).filter_map(|r| r.get("ok"));
        let acted = outcomes().any(|ok| ok == &Value::Bool(true));
        let failed = output.error.is_some()
            || decision_error.is_some()
            || outcomes().any(|ok| ok == &Value::Bool(false));
        let mut memory_after = memory_before.clone();
        let mut memory_error = None;
        if let Some(memory) = &decision.memory {
            if memory.to_string().len() > MEMORY_LIMIT {
                memory_error = Some(format!("memory over {MEMORY_LIMIT} bytes was not saved"));
            } else {
                memory_after = memory.clone();
                if !dry_run {
                    let _ = self.store.kv_set(&memory_key, memory);
                }
            }
        }
        if acted {
            let _ = self.store.kv_set(&previous_key, &core);
        }
        if let Some(buttons) = &decision.buttons
            && !dry_run
        {
            self.store_buttons(project, &run_id, buttons);
        }
        let started_agents: Vec<Value> = results
            .iter()
            .filter_map(|r| {
                let result = r.get("result")?;
                result
                    .get("started_agent")
                    .filter(|v| v.is_string())
                    .or_else(|| {
                        (r.get("method").and_then(Value::as_str) == Some("agent.create"))
                            .then(|| result.get("id"))
                            .flatten()
                    })
                    .cloned()
            })
            .collect();
        let error = output.error.clone().or(decision_error).or(memory_error);
        let shadow = if trigger["kind"] == "tick" {
            self.last_rules_report()
                .filter(|report| {
                    report["at_ms"].as_u64().unwrap_or(0) + 2 * config.interval_ms > started
                })
                .map_or(Value::Null, |report| {
                    shadow_diff(&core, &json!(decision.actions), &report["planned"])
                })
        } else {
            Value::Null
        };
        let record = json!({
            "shadow": shadow,
            "stdout": output.stdout,
            "stderr": output.stderr,
            "exit_code": output.code,
            "error": error,
            "actions": decision.actions,
            "results": results,
            "cli_calls": cli_calls,
            "buttons": decision.buttons,
            "memory_before": memory_before,
            "memory_after": memory_after,
            "script": script,
            "now": started,
        });
        let duration = now_ms().saturating_sub(started);
        let scope_hash = hash(&core);
        let home = self.paths.home.clone();
        self.automations.with_db(&home, |db| {
            db.execute(
                "INSERT OR IGNORE INTO scopes (hash, scope) VALUES (?1, ?2)",
                params![scope_hash, core.to_string()],
            )?;
            db.execute(
                "INSERT INTO runs (id, project, started_ms, duration_ms, trigger, scope, dry_run, failed, acted, record)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    run_id,
                    project.id,
                    started as i64,
                    duration as i64,
                    trigger.to_string(),
                    scope_hash,
                    dry_run,
                    failed,
                    acted,
                    record.to_string()
                ],
            )?;
            prune(db, started)
        })?;
        Ok(Some(json!({
            "id": run_id,
            "project": project.name,
            "trigger": trigger,
            "dry_run": dry_run,
            "failed": failed,
            "acted": acted,
            "error": error,
            "actions": decision.actions,
            "results": results,
            "cli_calls": record["cli_calls"],
            "started_agents": started_agents,
            "duration_ms": duration,
            "stderr": record["stderr"],
        })))
    }

    fn store_buttons(
        self: &Arc<Self>,
        project: &Project,
        run_id: &str,
        decided: &Map<String, Value>,
    ) {
        let ids: HashSet<String> = self
            .store
            .workspaces(Some(&project.id))
            .unwrap_or_default()
            .into_iter()
            .map(|w| w.id)
            .collect();
        let mut changed = Vec::new();
        {
            let mut buttons = self
                .automations
                .buttons
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            for id in &ids {
                let next = decided.get(id).map(|list| Buttons {
                    run: run_id.to_owned(),
                    list: list.as_array().cloned().unwrap_or_default(),
                });
                let before = buttons.get(id).map(|b| b.list.clone());
                if before != next.as_ref().map(|b| b.list.clone()) {
                    changed.push(id.clone());
                }
                match next {
                    Some(next) => buttons.insert(id.clone(), next),
                    None => buttons.remove(id),
                };
            }
        }
        for id in changed {
            if let Ok(Some(workspace)) = self.store.workspace(&id) {
                let server = Arc::clone(self);
                tokio::spawn(async move {
                    if let Ok(reply) = server.next_action_reply(&workspace).await {
                        server.broadcast(crate::pr_refresh::NEXT_ACTION_TOPIC, reply);
                    }
                });
            }
        }
    }

    async fn apply_actions(
        self: &Arc<Self>,
        actions: &[Value],
        config: &Config,
        scope: &Value,
    ) -> Vec<Value> {
        let mut results = Vec::new();
        let mut failed_workspaces = HashSet::new();
        let mut agents_started = 0;
        for (index, raw) in actions.iter().enumerate() {
            let mut result = json!({ "index": index, "action": raw });
            let action: Action = match serde_json::from_value(raw.clone()) {
                Ok(action) => action,
                Err(error) => {
                    result["ok"] = json!(false);
                    result["error"] = json!(format!("not an action: {error}"));
                    results.push(result);
                    continue;
                }
            };
            result["method"] = json!(action.method);
            let workspace =
                action_workspace(&action).and_then(|key| self.store.workspace(&key).ok().flatten());
            let refuse = |reason: String| -> Option<String> { Some(reason) };
            let refusal = if denied(&action.method) {
                refuse(format!("{} can't be called from a script", action.method))
            } else if let Some(w) = workspace
                .as_ref()
                .filter(|w| failed_workspaces.contains(&w.id))
            {
                refuse(format!("skipped: an earlier action on {} failed", w.name))
            } else if let Some(w) = workspace
                .as_ref()
                .filter(|w| !action.force && config.hold.is_some() && w.status == config.hold)
            {
                refuse(format!("skipped: {} is in the hold column", w.name))
            } else if AGENT_METHODS.contains(&action.method.as_str())
                && agents_started >= config.max_agents
            {
                refuse(format!(
                    "skipped: this run already started {agents_started} agents (automations.max_agents_per_run)"
                ))
            } else {
                None
            };
            if let Some(reason) = refusal {
                result["ok"] = json!(false);
                result["error"] = json!(reason);
                results.push(result);
                continue;
            }
            let mut params = match action.params {
                Value::Null => json!({}),
                other => other,
            };
            if action.method == "workspace.run_next_action" {
                params["builtin"] = json!(true);
            }
            if let Some(id) = params.get("prompt_id").and_then(Value::as_str)
                && params.get("prompt").is_none()
            {
                let template = scope["settings"]["prompts"][id].as_str();
                let Some(template) = template else {
                    result["ok"] = json!(false);
                    result["error"] = json!(format!("no prompt {id} in settings"));
                    results.push(result);
                    continue;
                };
                let ws_scope = workspace
                    .as_ref()
                    .and_then(|w| scope_workspace(scope, &w.id))
                    .cloned()
                    .unwrap_or(Value::Null);
                params["prompt"] =
                    json!(render_template(template, &prompt_context(scope, &ws_scope)));
            }
            match self.call_internal(&action.method, params).await {
                Ok(value) => {
                    self.automations.observe("workspace.changed");
                    if AGENT_METHODS.contains(&action.method.as_str()) {
                        agents_started += 1;
                    }
                    result["ok"] = json!(true);
                    result["result"] = value;
                }
                Err(error) => {
                    if let Some(w) = &workspace {
                        failed_workspaces.insert(w.id.clone());
                    }
                    result["ok"] = json!(false);
                    result["error"] = json!(format!("{}: {}", error.code, error.message));
                }
            }
            results.push(result);
        }
        results
    }

    pub(crate) async fn automations_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        match method {
            "automation.status" => self.automation_status(),
            "automation.run" => {
                #[derive(Deserialize)]
                struct P {
                    project: Option<String>,
                    #[serde(default)]
                    dry_run: bool,
                }
                let p: P = rpc_params(p)?;
                let projects = match &p.project {
                    Some(key) => vec![self.automation_project(key)?],
                    None => self.store.projects().map_err(|e| rpc("internal", e))?,
                };
                let mut runs = Vec::new();
                for project in projects {
                    let has_script = script_path(
                        &config(&self.resolved_settings(Some(&project.root))),
                        &project,
                    )
                    .is_some();
                    if !has_script && p.project.is_none() {
                        continue;
                    }
                    let run = self
                        .automation_run(
                            &project,
                            json!({ "kind": "manual" }),
                            p.dry_run,
                            Wait::Queue,
                        )
                        .await?;
                    runs.extend(run);
                }
                Ok(json!({ "runs": runs }))
            }
            "automation.emit" => {
                #[derive(Deserialize)]
                struct P {
                    name: String,
                    project: Option<String>,
                    path: Option<PathBuf>,
                    workspace: Option<String>,
                    #[serde(default)]
                    payload: Value,
                    #[serde(default)]
                    dry_run: bool,
                }
                let p: P = rpc_params(p)?;
                let project = match (&p.project, &p.path) {
                    (Some(key), _) => self.automation_project(key)?,
                    (None, Some(path)) => self.project_for_path(path)?,
                    (None, None) => {
                        let fallback = config(&self.resolved_settings(None)).default_project;
                        let key = fallback.ok_or_else(|| {
                            rpc(
                                "invalid_params",
                                "pass --project or --path, or set automations.default_project",
                            )
                        })?;
                        self.automation_project(&key)?
                    }
                };
                let trigger = json!({
                    "kind": "event",
                    "name": p.name,
                    "workspace": p.workspace,
                    "payload": p.payload,
                });
                let run = self
                    .automation_run(&project, trigger, p.dry_run, Wait::Queue)
                    .await?;
                Ok(run.unwrap_or(Value::Null))
            }
            "automation.runs" => {
                #[derive(Deserialize)]
                struct P {
                    project: Option<String>,
                    #[serde(default)]
                    failed: bool,
                    #[serde(default)]
                    acted: bool,
                    #[serde(default)]
                    shadow: bool,
                    limit: Option<u32>,
                }
                let p: P = rpc_params(p)?;
                let project = p
                    .project
                    .as_deref()
                    .map(|k| self.automation_project(k).map(|p| p.id))
                    .transpose()?;
                let home = self.paths.home.clone();
                let runs = self.automations.with_db(&home, |db| {
                    let mut statement = db.prepare(
                        "SELECT id, project, started_ms, duration_ms, trigger, dry_run, failed, acted, record FROM runs
                         WHERE (?1 IS NULL OR project = ?1) AND (?2 = 0 OR failed = 1) AND (?3 = 0 OR acted = 1)
                           AND (?5 = 0 OR json_extract(record, '$.shadow.agree') = 0)
                         ORDER BY started_ms DESC LIMIT ?4",
                    )?;
                    let rows = statement.query_map(
                        params![project, p.failed, p.acted, p.limit.unwrap_or(50), p.shadow],
                        |row| {
                            let record: Value =
                                serde_json::from_str(&row.get::<_, String>(8)?).unwrap_or_default();
                            Ok(json!({
                                "id": row.get::<_, String>(0)?,
                                "project": row.get::<_, String>(1)?,
                                "started_ms": row.get::<_, i64>(2)?,
                                "duration_ms": row.get::<_, i64>(3)?,
                                "trigger": serde_json::from_str::<Value>(&row.get::<_, String>(4)?).unwrap_or_default(),
                                "dry_run": row.get::<_, bool>(5)?,
                                "failed": row.get::<_, bool>(6)?,
                                "acted": row.get::<_, bool>(7)?,
                                "actions": record["actions"].as_array().map_or(0, Vec::len),
                                "error": record["error"],
                            }))
                        },
                    )?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()
                })?;
                Ok(json!({ "runs": runs }))
            }
            "automation.show" => {
                #[derive(Deserialize)]
                struct P {
                    run: String,
                    #[serde(default)]
                    scope: bool,
                }
                let p: P = rpc_params(p)?;
                let (mut run, scope) = self.load_run(&p.run)?;
                if p.scope {
                    run["scope"] = scope;
                }
                Ok(run)
            }
            "automation.replay" => {
                #[derive(Deserialize)]
                struct P {
                    run: String,
                    script: Option<PathBuf>,
                }
                let p: P = rpc_params(p)?;
                let (run, core) = self.load_run(&p.run)?;
                let script = match p.script {
                    Some(path) => path,
                    None => run["record"]["script"]
                        .as_str()
                        .map(PathBuf::from)
                        .ok_or_else(|| rpc("not_found", "the run has no script path"))?,
                };
                let scope = full_scope(
                    &core,
                    &run["trigger"],
                    run["record"]["now"].as_u64().unwrap_or_default(),
                    &run["record"]["memory_before"],
                    &Value::Null,
                    true,
                    &p.run,
                );
                let replay_id = format!(
                    "replay-{}",
                    &uuid::Uuid::new_v4().simple().to_string()[..12]
                );
                self.automations.begin(&replay_id, true);
                let output = run_script(
                    &script,
                    scope.to_string().as_bytes(),
                    &replay_id,
                    true,
                    &self.paths.home,
                )
                .await;
                let cli_calls = self.automations.finish(&replay_id);
                let decision = match &output.error {
                    Some(error) => Err(error.clone()),
                    None => parse_decision(&output.stdout),
                };
                let (actions, buttons, memory, error) = match decision {
                    Ok(d) => (d.actions, d.buttons, d.memory, None),
                    Err(e) => (Vec::new(), None, None, Some(e)),
                };
                Ok(json!({
                    "run": p.run,
                    "script": script,
                    "actions": actions,
                    "buttons": buttons,
                    "memory": memory,
                    "error": error,
                    "stderr": output.stderr,
                    "cli_calls": cli_calls,
                    "recorded_actions": run["record"]["actions"],
                    "same_as_recorded": json!(actions) == run["record"]["actions"],
                }))
            }
            "automation.schema" => self.write_automation_types(true),
            "prompt.list" => {
                #[derive(Deserialize)]
                struct P {
                    project: Option<String>,
                }
                let p: P = rpc_params(p)?;
                let root = p
                    .project
                    .as_deref()
                    .map(|k| self.automation_project(k).map(|p| p.root))
                    .transpose()?;
                let settings = self.resolved_settings(root.as_deref());
                Ok(setting(&settings, &["prompts"])
                    .cloned()
                    .unwrap_or(json!({})))
            }
            "prompt.render" => {
                #[derive(Deserialize)]
                struct P {
                    id: Option<String>,
                    template: Option<String>,
                    workspace: String,
                }
                let p: P = rpc_params(p)?;
                let workspace = self
                    .store
                    .workspace(&p.workspace)
                    .map_err(|e| rpc("internal", e))?
                    .ok_or_else(|| rpc("not_found", format!("workspace {}", p.workspace)))?;
                let project = self.automation_project(&workspace.project_id)?;
                let settings = self.resolved_settings(Some(&project.root));
                let template = match (&p.template, &p.id) {
                    (Some(t), _) => t.clone(),
                    (None, Some(id)) => setting(&settings, &["prompts", id])
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(|| rpc("not_found", format!("no prompt {id} in settings")))?,
                    (None, None) => return Err(rpc("invalid_params", "pass an id or a template")),
                };
                let scope = json!({
                    "project": { "id": project.id, "name": project.name, "root": project.root },
                });
                let ws = self.scope_workspace(&workspace, &self.agents.list()).await;
                Ok(json!({ "prompt": render_template(&template, &prompt_context(&scope, &ws)) }))
            }
            "workspace.button" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                }
                let p: P = rpc_params(p)?;
                let workspace = self
                    .store
                    .workspace(&p.id)
                    .map_err(|e| rpc("internal", e))?
                    .ok_or_else(|| rpc("not_found", format!("workspace {}", p.id)))?;
                let suggested = self
                    .resolve_next(&workspace)
                    .await
                    .map(|(action, _, _)| crate::next_action::action_json(action))
                    .unwrap_or(Value::Null);
                let Some(buttons) = self.automations.buttons(&workspace.id) else {
                    return Ok(json!({
                        "workspace": workspace.id,
                        "decided_by": "built-in",
                        "buttons": if suggested.is_object() { vec![suggested.clone()] } else { Vec::new() },
                        "suggested_action": suggested,
                    }));
                };
                let (run, core) = self.load_run(&buttons.run)?;
                let ws_scope = scope_workspace(&core, &workspace.id)
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(json!({
                    "workspace": workspace.id,
                    "decided_by": "script",
                    "run": buttons.run,
                    "run_started_ms": run["started_ms"],
                    "trigger": run["trigger"],
                    "buttons": buttons.list,
                    "suggested_action": suggested,
                    "scope_then": {
                        "status": ws_scope["status"],
                        "git": ws_scope["git"],
                        "pr": ws_scope["pr"].as_object().map(|pr| json!({
                            "number": pr.get("number"),
                            "state": pr.get("state"),
                            "is_draft": pr.get("is_draft"),
                            "mergeable": pr.get("mergeable"),
                            "merge_state_status": pr.get("merge_state_status"),
                            "review_decision": pr.get("review_decision"),
                            "checks_rollup": pr.get("checks_rollup"),
                            "is_in_merge_queue": pr.get("is_in_merge_queue"),
                            "auto_merge": pr.get("auto_merge"),
                            "updated_at": pr.get("updated_at"),
                        })),
                        "suggested_action": ws_scope["suggested_action"],
                    },
                }))
            }
            other => Err(rpc("unknown_method", other)),
        }
    }

    /// A call from a script's own `icarus`: recorded against its run, and only described in a dry run.
    pub(crate) async fn script_call(
        self: &Arc<Self>,
        run: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        if denied(method) {
            return Err(rpc(
                "forbidden",
                format!("{method} can't be called from inside an automation run"),
            ));
        }
        // A run that already ended (or an unknown id) is treated as dry: never act for a stale script.
        let dry_run = self
            .automations
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(run)
            .is_none_or(|r| r.dry_run);
        let mut entry = json!({ "method": method, "params": params });
        let reply = if dry_run {
            Ok(json!({ "dry_run": true, "would_call": method, "params": params }))
        } else {
            self.call_internal(method, params).await
        };
        match &reply {
            Ok(_) if dry_run => entry["dry_run"] = json!(true),
            Ok(_) => entry["ok"] = json!(true),
            Err(error) => {
                entry["ok"] = json!(false);
                entry["error"] = json!(format!("{}: {}", error.code, error.message));
            }
        }
        if let Some(active) = self
            .automations
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(run)
        {
            active.calls.push(entry);
        }
        reply
    }

    /// A click on a script's button: an event run for that workspace, decided on a fresh scope.
    pub(crate) async fn run_script_button(
        self: &Arc<Self>,
        workspace: &Workspace,
        buttons: &Buttons,
        button: Option<String>,
    ) -> Result<Value, RpcError> {
        let chosen = match &button {
            Some(id) => buttons
                .list
                .iter()
                .find(|b| b.get("id").and_then(Value::as_str) == Some(id.as_str())),
            None => buttons.list.first(),
        }
        .cloned()
        .ok_or_else(|| rpc("no_action", "nothing to do for this workspace"))?;
        let id = chosen.get("id").cloned().unwrap_or(Value::Null);
        let project = self.automation_project(&workspace.project_id)?;
        let trigger = json!({
            "kind": "event",
            "name": "button",
            "workspace": workspace.id,
            "payload": { "id": id },
        });
        let run = self
            .automation_run(&project, trigger, false, Wait::Queue)
            .await?
            .unwrap_or(Value::Null);
        Ok(json!({
            "action": button_action(&chosen),
            "started_agent": run["started_agents"].get(0),
            "github": null,
            "run": run,
        }))
    }

    fn load_run(&self, id: &str) -> Result<(Value, Value), RpcError> {
        let home = self.paths.home.clone();
        let row = self.automations.with_db(&home, |db| {
            db.query_row(
                "SELECT r.id, r.project, r.started_ms, r.duration_ms, r.trigger, r.dry_run, r.failed, r.acted, r.record, s.scope
                 FROM runs r LEFT JOIN scopes s ON s.hash = r.scope WHERE r.id = ?1",
                params![id],
                |row| {
                    let parse = |text: Option<String>| {
                        text.and_then(|t| serde_json::from_str::<Value>(&t).ok())
                            .unwrap_or(Value::Null)
                    };
                    Ok((
                        json!({
                            "id": row.get::<_, String>(0)?,
                            "project": row.get::<_, String>(1)?,
                            "started_ms": row.get::<_, i64>(2)?,
                            "duration_ms": row.get::<_, i64>(3)?,
                            "trigger": parse(row.get(4)?),
                            "dry_run": row.get::<_, bool>(5)?,
                            "failed": row.get::<_, bool>(6)?,
                            "acted": row.get::<_, bool>(7)?,
                            "record": parse(row.get(8)?),
                        }),
                        parse(row.get(9)?),
                    ))
                },
            )
            .optional()
        })?;
        row.ok_or_else(|| {
            rpc(
                "not_found",
                format!("run {id} (runs older than a day are pruned unless they acted or failed)"),
            )
        })
    }

    fn automation_status(&self) -> Result<Value, RpcError> {
        let global = config(&self.resolved_settings(None));
        let projects = self.store.projects().map_err(|e| rpc("internal", e))?;
        let home = self.paths.home.clone();
        let mut list = Vec::new();
        for project in projects {
            let config = config(&self.resolved_settings(Some(&project.root)));
            let script = script_path(&config, &project);
            let last = self.automations.with_db(&home, |db| {
                db.query_row(
                    "SELECT id, started_ms, failed, acted, dry_run FROM runs WHERE project = ?1 ORDER BY started_ms DESC LIMIT 1",
                    params![project.id],
                    |row| {
                        Ok(json!({
                            "id": row.get::<_, String>(0)?,
                            "started_ms": row.get::<_, i64>(1)?,
                            "failed": row.get::<_, bool>(2)?,
                            "acted": row.get::<_, bool>(3)?,
                            "dry_run": row.get::<_, bool>(4)?,
                        }))
                    },
                )
                .optional()
            })?;
            let shadow = self.automations.with_db(&home, |db| {
                db.query_row(
                    "SELECT COUNT(*), COALESCE(SUM(json_extract(record, '$.shadow.agree') = 0), 0), MIN(started_ms)
                     FROM runs WHERE project = ?1 AND json_extract(record, '$.shadow') IS NOT NULL",
                    params![project.id],
                    |row| {
                        Ok(json!({
                            "compared": row.get::<_, i64>(0)?,
                            "differed": row.get::<_, i64>(1)?,
                            "since_ms": row.get::<_, Option<i64>>(2)?,
                        }))
                    },
                )
            })?;
            list.push(json!({
                "shadow": shadow,
                "project": project.name,
                "id": project.id,
                "enabled": config.enabled,
                "armed": config.armed,
                "script": script,
                "script_exists": script.as_ref().is_some_and(|s| s.exists()),
                "last_run": last,
            }));
        }
        Ok(json!({
            "enabled": global.enabled,
            "armed": global.armed,
            "interval_ms": global.interval_ms,
            "running": self.app_count() > 0,
            "projects": list,
        }))
    }

    /// Writes `icarus.d.ts` and `schema.json` with this machine's status and prompt ids; unless
    /// `force`, only when they changed.
    pub(crate) fn write_automation_types(&self, force: bool) -> Result<Value, RpcError> {
        let dir = generated_dir().ok_or_else(|| rpc("internal", "no config directory"))?;
        // Nobody writes scripts here yet: don't create folders in the user's config unasked.
        if !force && dir.parent().is_none_or(|automations| !automations.exists()) {
            return Ok(json!({ "written": false }));
        }
        let settings = self.resolved_settings(None);
        let mut statuses: Vec<String> = Vec::new();
        let mut prompts: Vec<String> = Vec::new();
        let mut roots: Vec<Option<PathBuf>> = vec![None];
        roots.extend(
            self.store
                .projects()
                .unwrap_or_default()
                .into_iter()
                .map(|p| Some(p.root)),
        );
        for root in roots {
            let s = match &root {
                None => settings.clone(),
                Some(root) => self.resolved_settings(Some(root)),
            };
            for status in setting(&s, &["workspace", "statuses"])
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(id) = status.get("id").and_then(Value::as_str)
                    && !statuses.iter().any(|s| s == id)
                {
                    statuses.push(id.to_owned());
                }
            }
            for id in setting(&s, &["prompts"])
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|m| m.keys())
            {
                if !prompts.contains(id) {
                    prompts.push(id.clone());
                }
            }
        }
        let types = generated_types(&statuses, &prompts);
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Icarus automation decision",
            "type": ["array", "object"],
            "$defs": {
                "StatusId": { "enum": statuses },
                "PromptId": { "enum": prompts },
                "Method": { "enum": METHODS },
                "Action": {
                    "type": "object",
                    "required": ["method"],
                    "properties": {
                        "method": { "$ref": "#/$defs/Method" },
                        "params": { "type": "object" },
                        "force": { "type": "boolean" },
                    },
                },
                "Button": {
                    "type": "object",
                    "required": ["id", "label"],
                    "properties": {
                        "id": { "type": "string" },
                        "label": { "type": "string" },
                        "icon": { "type": "string" },
                        "style": { "enum": ["primary", "outlined", "subtle", "danger"] },
                        "tooltip": { "type": "string" },
                        "reason": { "type": "string" },
                    },
                },
            },
            "items": { "$ref": "#/$defs/Action" },
            "properties": {
                "actions": { "type": "array", "items": { "$ref": "#/$defs/Action" } },
                "memory": {},
                "buttons": {
                    "type": "object",
                    "additionalProperties": { "type": "array", "items": { "$ref": "#/$defs/Button" } },
                },
            },
        });
        let types_path = dir.join("icarus.d.ts");
        let schema_path = dir.join("schema.json");
        let reply = json!({ "types": types_path, "schema": schema_path, "statuses": statuses, "prompts": prompts, "written": true });
        {
            let mut last = self
                .automations
                .last_types
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if !force && last.as_deref() == Some(types.as_str()) && types_path.exists() {
                let mut unchanged = reply;
                unchanged["written"] = json!(false);
                return Ok(unchanged);
            }
            *last = Some(types.clone());
        }
        std::fs::create_dir_all(&dir).map_err(|e| rpc("internal", e))?;
        std::fs::write(&types_path, types).map_err(|e| rpc("internal", e))?;
        std::fs::write(
            &schema_path,
            serde_json::to_string_pretty(&schema).unwrap_or_default(),
        )
        .map_err(|e| rpc("internal", e))?;
        Ok(reply)
    }
}

/// What a plan changes, as comparable keys like `status:<ws>=done` or `review:#12`.
fn script_effects(core: &Value, actions: &Value) -> BTreeSet<String> {
    let status_of = |id: &str| {
        core["workspaces"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|w| w["id"] == id || w["name"] == id)
            .map(|w| {
                (
                    w["id"].as_str().unwrap_or(id).to_owned(),
                    w["status"].clone(),
                )
            })
    };
    let mut keys = BTreeSet::new();
    for action in actions.as_array().into_iter().flatten() {
        let p = &action["params"];
        let workspace = p["workspace"]
            .as_str()
            .or(p["id"].as_str())
            .unwrap_or_default();
        let id = status_of(workspace).map_or(workspace.to_owned(), |(id, _)| id);
        let key = match action["method"].as_str().unwrap_or_default() {
            "workspace.set" if p["status"].is_string() => {
                if status_of(workspace).is_some_and(|(_, status)| status == p["status"]) {
                    continue;
                }
                format!("status:{id}={}", p["status"].as_str().unwrap_or_default())
            }
            "workspace.remove" => format!("delete:{id}"),
            "review.workspace" => format!("review:#{}", p["number"]),
            "agent.create" => format!("agent:{id}"),
            "tab.open" if p["type"] == "agent" => format!("agent:{id}"),
            "workspace.sync_to_pr_head" => format!("sync:{id}"),
            "terminal.create" => format!("command:{id}"),
            _ => continue,
        };
        keys.insert(key);
    }
    keys
}

fn rules_effects(core: &Value, planned: &Value) -> BTreeSet<String> {
    let ours: HashSet<&str> = core["workspaces"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| w["id"].as_str())
        .collect();
    let inbox: HashSet<u64> = core["inbox"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pr| pr["number"].as_u64())
        .collect();
    let mut keys = BTreeSet::new();
    for action in planned.as_array().into_iter().flatten() {
        let workspace = action["workspace_id"].as_str().unwrap_or_default();
        let key = match action["action"].as_str().unwrap_or_default() {
            "create_workspace_from_pr" => match action["pr"]["number"].as_u64() {
                Some(number) if inbox.contains(&number) => format!("review:#{number}"),
                _ => continue,
            },
            _ if !ours.contains(workspace) => continue,
            "set_column" => format!(
                "status:{workspace}={}",
                action["status"].as_str().unwrap_or_default()
            ),
            "delete_workspace" => format!("delete:{workspace}"),
            "launch_agent" => format!("agent:{workspace}"),
            "sync_to_pr_head" => format!("sync:{workspace}"),
            "run_command" => format!("command:{workspace}"),
            _ => continue,
        };
        keys.insert(key);
    }
    keys
}

/// Where the script and the built-in rules would act differently on the same project (shadow mode).
fn shadow_diff(core: &Value, actions: &Value, planned: &Value) -> Value {
    let script = script_effects(core, actions);
    let rules = rules_effects(core, planned);
    json!({
        "agree": script == rules,
        "script_only": script.difference(&rules).collect::<Vec<_>>(),
        "rules_only": rules.difference(&script).collect::<Vec<_>>(),
    })
}

fn event(name: &str, workspace: &str, payload: Value) -> Value {
    json!({ "name": name, "workspace": workspace, "payload": payload })
}

fn pr_state(pr: &Value) -> Value {
    if pr["state"] == "OPEN" && pr["is_draft"] == true {
        json!("DRAFT")
    } else {
        pr["state"].clone()
    }
}

fn failed_checks(pr: &Value) -> Vec<Value> {
    const FAILED: &[&str] = &[
        "FAILURE",
        "TIMED_OUT",
        "CANCELLED",
        "ACTION_REQUIRED",
        "STARTUP_FAILURE",
        "ERROR",
    ];
    pr["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| {
            let outcome = c["kind"]["conclusion"]
                .as_str()
                .or(c["kind"]["state"].as_str())
                .unwrap_or_default()
                .to_ascii_uppercase();
            FAILED.contains(&outcome.as_str())
        })
        .map(|c| c["name"].clone())
        .collect()
}

fn added<'a>(before: &'a Value, after: &'a Value) -> Vec<Value> {
    let old = before.as_array().cloned().unwrap_or_default();
    after
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| !old.contains(v))
        .cloned()
        .collect()
}

/// What changed between two event checks, by workspace, in the spec's event names.
fn diff_events(before: &HashMap<String, Value>, after: &HashMap<String, Value>) -> Vec<Value> {
    let mut ids: Vec<&String> = after.keys().collect();
    ids.sort();
    let mut events = Vec::new();
    for id in ids {
        let n = &after[id];
        let Some(o) = before.get(id) else {
            events.push(event("workspace.created", id, json!({})));
            continue;
        };
        if o["status"] != n["status"] {
            let change = json!({ "from": o["status"], "to": n["status"] });
            events.push(event("workspace.status_changed", id, change));
        }
        if o["issue"] != n["issue"] && !n["issue"].is_null() {
            events.push(event("linear.changed", id, json!({ "issue": n["issue"] })));
        }
        let (op, np) = (&o["pr"], &n["pr"]);
        if np.is_object() {
            if !op.is_object() || op["number"] != np["number"] {
                events.push(event("pr.opened", id, json!({ "number": np["number"] })));
            } else {
                if pr_state(op) != pr_state(np) {
                    let change = json!({ "from": pr_state(op), "to": pr_state(np) });
                    events.push(event("pr.state_changed", id, change));
                }
                if op["head_ref_oid"] != np["head_ref_oid"] {
                    let change = json!({ "from": op["head_ref_oid"], "to": np["head_ref_oid"] });
                    events.push(event("pr.head_changed", id, change));
                }
                if op["base_ref_name"] != np["base_ref_name"] {
                    let change = json!({ "from": op["base_ref_name"], "to": np["base_ref_name"] });
                    events.push(event("pr.base_changed", id, change));
                }
                if op["checks_rollup"] != np["checks_rollup"] {
                    let change = json!({ "from": op["checks_rollup"], "to": np["checks_rollup"] });
                    events.push(event("checks.changed", id, change));
                    if np["checks_rollup"] == "failure" {
                        let failed = json!({ "failed": failed_checks(np) });
                        events.push(event("checks.failed", id, failed));
                    }
                }
                let requested = added(&op["requested_reviewers"], &np["requested_reviewers"]);
                if !requested.is_empty() {
                    let payload = json!({ "reviewers": requested });
                    events.push(event("review.requested", id, payload));
                }
                let reviews = added(&op["latest_reviews"], &np["latest_reviews"]);
                if !reviews.is_empty() {
                    events.push(event("review.submitted", id, json!({ "reviews": reviews })));
                }
            }
        }
        let (og, ng) = (&o["git"], &n["git"]);
        if ng["upstream"].is_string()
            && og["head"] == ng["head"]
            && og["ahead"].as_u64().unwrap_or(0) > 0
            && ng["ahead"] == 0
        {
            events.push(event("git.pushed", id, json!({ "head": ng["head"] })));
        }
        let old_agents: HashMap<&str, &Value> = o["agents"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| Some((a["id"].as_str()?, &a["state"])))
            .collect();
        for agent in n["agents"].as_array().into_iter().flatten() {
            let Some(was) = agent["id"].as_str().and_then(|a| old_agents.get(a)) else {
                continue;
            };
            let name = match agent["state"].as_str() {
                Some("idle") if *was == "working" => "agent.finished",
                Some("waiting") if *was != "waiting" => "agent.waiting",
                _ => continue,
            };
            events.push(event(name, id, json!({ "agent": agent["id"] })));
        }
    }
    events
}

fn full_scope(
    core: &Value,
    trigger: &Value,
    now: u64,
    memory: &Value,
    previous: &Value,
    dry_run: bool,
    run_id: &str,
) -> Value {
    let mut scope = core.clone();
    scope["trigger"] = trigger.clone();
    scope["now"] = json!(now);
    scope["memory"] = memory.clone();
    scope["previous"] = previous.clone();
    scope["dry_run"] = json!(dry_run);
    scope["run_id"] = json!(run_id);
    scope
}

/// What the title-bar button shows for a script's button, in the shape of a built-in step.
pub(crate) fn button_action(button: &Value) -> Value {
    json!({
        "kind": format!("script:{}", button.get("id").and_then(Value::as_str).unwrap_or_default()),
        "label": button.get("label").and_then(Value::as_str).unwrap_or("Run"),
        "agent_step": false,
        "icon": button.get("icon"),
        "tooltip": button.get("tooltip"),
    })
}

fn quote_union(ids: &[impl AsRef<str>]) -> String {
    if ids.is_empty() {
        return "never".into();
    }
    ids.iter()
        .map(|id| serde_json::to_string(id.as_ref()).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" | ")
}

pub(crate) fn generated_types(statuses: &[String], prompts: &[String]) -> String {
    format!(
        "// Generated by Icarus {}; regenerated when statuses or prompts change. Do not edit.\n\nexport type StatusId = {};\nexport type PromptId = {};\nexport type Method = {};\n\n{}",
        crate::updates::VERSION,
        quote_union(statuses),
        quote_union(prompts),
        quote_union(METHODS),
        BASE_TYPES
    )
}

fn prune(db: &Connection, now: u64) -> rusqlite::Result<()> {
    db.execute(
        "DELETE FROM runs WHERE started_ms < ?1
           AND NOT ((failed = 1 OR acted = 1 OR json_extract(record, '$.shadow') IS NOT NULL) AND started_ms >= ?2)
           AND id NOT IN (SELECT r.id FROM runs r WHERE r.project = runs.project ORDER BY r.started_ms DESC LIMIT ?3)",
        params![
            now.saturating_sub(KEEP_RECENT_MS) as i64,
            now.saturating_sub(KEEP_NOTABLE_MS) as i64,
            KEEP_RECENT_RUNS
        ],
    )?;
    db.execute(
        "DELETE FROM scopes WHERE hash NOT IN (SELECT scope FROM runs)",
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_mode_compares_effects_not_wording() {
        let core = json!({
            "workspaces": [
                { "id": "w1", "name": "one", "status": "todo" },
                { "id": "w2", "name": "two", "status": "done" },
            ],
            "inbox": [{ "number": 12 }],
        });
        let actions = json!([
            { "method": "workspace.set", "params": { "id": "one", "status": "in-review" } },
            { "method": "workspace.set", "params": { "id": "w2", "status": "done" } },
            { "method": "review.workspace", "params": { "project": "p", "number": 12 } },
        ]);
        let planned = json!([
            { "action": "set_column", "workspace_id": "w1", "status": "in-review" },
            { "action": "create_workspace_from_pr", "pr": { "number": 12 } },
            { "action": "create_workspace_from_pr", "pr": { "number": 99 } },
            { "action": "delete_workspace", "workspace_id": "elsewhere" },
        ]);
        assert_eq!(shadow_diff(&core, &actions, &planned)["agree"], true);
        let planned = json!([{ "action": "sync_to_pr_head", "workspace_id": "w2" }]);
        let diff = shadow_diff(&core, &actions, &planned);
        assert_eq!(diff["rules_only"], json!(["sync:w2"]));
        assert_eq!(
            diff["script_only"],
            json!(["review:#12", "status:w1=in-review"])
        );
    }

    #[test]
    fn events_name_what_changed_between_checks() {
        let pr = json!({
            "number": 7, "state": "OPEN", "is_draft": true, "head_ref_oid": "a",
            "base_ref_name": "main", "checks_rollup": "pending", "checks": [],
            "requested_reviewers": [], "latest_reviews": [],
        });
        let ws = json!({
            "id": "w", "status": "todo", "issue": null, "pr": pr,
            "git": { "upstream": "origin/x", "head": "h", "ahead": 2 },
            "agents": [{ "id": "a1", "state": "working" }],
        });
        let mut next = ws.clone();
        next["status"] = json!("in-review");
        next["pr"]["is_draft"] = json!(false);
        next["pr"]["checks_rollup"] = json!("failure");
        next["pr"]["checks"] = json!([
            { "name": "test", "kind": { "type": "run", "status": "COMPLETED", "conclusion": "FAILURE" } },
            { "name": "lint", "kind": { "type": "run", "status": "COMPLETED", "conclusion": "SUCCESS" } },
        ]);
        next["pr"]["requested_reviewers"] = json!([{ "kind": "user", "login": "bo" }]);
        next["git"]["ahead"] = json!(0);
        next["agents"][0]["state"] = json!("idle");
        let before = HashMap::from([("w".to_owned(), ws)]);
        let after = HashMap::from([
            ("w".to_owned(), next),
            ("v".to_owned(), json!({ "id": "v" })),
        ]);
        let events = diff_events(&before, &after);
        let names: Vec<&str> = events.iter().filter_map(|e| e["name"].as_str()).collect();
        assert_eq!(
            names,
            [
                "workspace.created",
                "workspace.status_changed",
                "pr.state_changed",
                "checks.changed",
                "checks.failed",
                "review.requested",
                "git.pushed",
                "agent.finished",
            ]
        );
        assert_eq!(
            events[2]["payload"],
            json!({ "from": "DRAFT", "to": "OPEN" })
        );
        assert_eq!(events[4]["payload"]["failed"], json!(["test"]));
        assert!(diff_events(&after, &after).is_empty());
    }

    #[test]
    fn templates_substitute_paths_and_leave_missing_ones_empty() {
        let context =
            json!({ "pr": { "number": 12, "title": "Fix" }, "workspace": { "status": "review" } });
        assert_eq!(
            render_template(
                "#{{pr.number}} {{ pr.title }} in {{workspace.status}}{{missing.x}}!",
                &context
            ),
            "#12 Fix in review!"
        );
        assert_eq!(
            render_template("open {{ never closed", &context),
            "open {{ never closed"
        );
    }

    #[test]
    fn a_decision_is_an_action_list_or_an_object() {
        let list = parse_decision(r#"[{"method":"workspace.set","params":{"id":"w"}}]"#)
            .expect("an action list");
        assert_eq!(list.actions.len(), 1);
        let object = parse_decision(
            "log line\n{\"actions\":[],\"memory\":{\"a\":1},\"buttons\":{\"w\":[]}}",
        )
        .expect("an object after a log line");
        assert_eq!(object.memory, Some(json!({ "a": 1 })));
        assert!(object.buttons.expect("buttons").contains_key("w"));
        assert!(parse_decision("").expect("empty").actions.is_empty());
        assert!(parse_decision("nope").is_err());
        assert!(parse_decision(r#"{"actions":3}"#).is_err());
    }

    #[test]
    fn scripts_cannot_drive_the_engine_or_the_server() {
        assert!(denied("automation.run"));
        assert!(denied("server.shutdown"));
        assert!(denied("agent.attach"));
        assert!(!denied("workspace.set"));
        assert!(METHODS.iter().all(|m| !denied(m)));
    }

    #[test]
    fn workspace_actions_are_found_by_workspace_or_id() {
        let action = |method: &str, params: Value| Action {
            method: method.into(),
            params,
            force: false,
        };
        assert_eq!(
            action_workspace(&action("workspace.set", json!({ "id": "w1" }))),
            Some("w1".into())
        );
        assert_eq!(
            action_workspace(&action("pr.operate", json!({ "workspace": "w2" }))),
            Some("w2".into())
        );
        assert_eq!(
            action_workspace(&action("agent.send", json!({ "id": "a1" }))),
            None
        );
    }

    #[test]
    fn generated_types_list_settings_ids_as_literals() {
        let types = generated_types(&["review".into(), "done".into()], &[]);
        assert!(types.contains("export type StatusId = \"review\" | \"done\";"));
        assert!(types.contains("export type PromptId = never;"));
        assert!(types.contains("\"pr.operate\""));
        assert!(types.contains("export interface Scope"));
    }

    #[test]
    fn script_paths_resolve_by_name_then_id() {
        let mut config = config(&json!({}));
        config
            .scripts
            .insert("icarus".into(), "/abs/icarus.ts".into());
        config.scripts.insert("p-2".into(), "/abs/two.ts".into());
        let project = |id: &str, name: &str| Project {
            id: id.into(),
            name: name.into(),
            root: PathBuf::from("/r"),
            remote: None,
            base_ref: None,
            created_at_ms: 0,
        };
        assert_eq!(
            script_path(&config, &project("p-1", "icarus")),
            Some(PathBuf::from("/abs/icarus.ts"))
        );
        assert_eq!(
            script_path(&config, &project("p-2", "other")),
            Some(PathBuf::from("/abs/two.ts"))
        );
        assert_eq!(script_path(&config, &project("p-3", "none")), None);
    }

    #[test]
    fn pruning_keeps_recent_notable_and_the_last_runs() {
        let db = Connection::open_in_memory().expect("db");
        db.execute_batch(
            "CREATE TABLE runs (id TEXT PRIMARY KEY, project TEXT, started_ms INTEGER, duration_ms INTEGER,
               trigger TEXT, scope TEXT, dry_run INTEGER, failed INTEGER, acted INTEGER, record TEXT);
             CREATE TABLE scopes (hash TEXT PRIMARY KEY, scope TEXT);",
        )
        .expect("schema");
        let day = KEEP_RECENT_MS;
        let now = 30 * day;
        let insert = |id: &str, started: u64, failed: bool, acted: bool| {
            db.execute(
                "INSERT INTO runs VALUES (?1, 'p', ?2, 0, '{}', ?1, 0, ?3, ?4, '{}')",
                params![id, started as i64, failed, acted],
            )
            .expect("run");
            db.execute("INSERT INTO scopes VALUES (?1, '{}')", params![id])
                .expect("scope");
        };
        for i in 0..(KEEP_RECENT_RUNS as u64 + 10) {
            insert(&format!("old-{i}"), now - 2 * day - i, false, false);
        }
        insert("recent", now - 1000, false, false);
        insert("acted", now - 3 * day - 1000, false, true);
        insert("ancient-failure", now - 8 * day - 10_000, true, false);
        prune(&db, now).expect("prune");
        let ids: HashSet<String> = db
            .prepare("SELECT id FROM runs")
            .expect("query")
            .query_map([], |r| r.get(0))
            .expect("rows")
            .collect::<rusqlite::Result<_>>()
            .expect("ids");
        assert!(ids.contains("recent"));
        assert!(ids.contains("acted"));
        assert!(!ids.contains("ancient-failure"));
        assert_eq!(ids.len(), KEEP_RECENT_RUNS as usize + 1);
        let scopes: i64 = db
            .query_row("SELECT COUNT(*) FROM scopes", [], |r| r.get(0))
            .expect("count");
        assert_eq!(scopes as usize, ids.len());
    }
}
