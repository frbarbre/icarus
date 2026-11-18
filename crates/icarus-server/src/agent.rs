//! Agent threads: one long-lived agent process per thread, owned by the server.
//!
//! Claude Code runs as the user's own, unmodified `claude` binary in stream-JSON mode (plan D19);
//! Codex runs as the user's own `codex app-server` (see `codex`). Output is normalized into
//! `AgentEvent`s, kept in memory and on disk, so clients can attach, detach and resume by
//! sequence number while the agent keeps working.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::codex;

/// Bounds Codex handshakes and requests so a stuck app-server cannot hang a client.
const CODEX_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Bounds Claude Code control requests; `initialize` waits for the session's hooks to start.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// The provider's own default model; no `--model` / per-turn model is sent for it.
pub const DEFAULT_MODEL: &str = "default";

const BYPASS_PERMISSIONS: &str = "bypassPermissions";

/// Claude Code's permission modes (`set_permission_mode`) as `(id, name, description)`. Nobody
/// answers its permission prompts, so in the asking modes a tool that needs approval is denied.
pub const CLAUDE_MODES: &[(&str, &str, &str)] = &[
    (
        "default",
        "Default",
        "Asks before edits and commands; with nobody to ask they are denied",
    ),
    (
        "acceptEdits",
        "Accept Edits",
        "Edits files without asking; other prompts are denied",
    ),
    (
        "plan",
        "Plan",
        "Explores and plans without changing anything",
    ),
    (
        "auto",
        "Auto",
        "Claude Code decides which actions are safe to run",
    ),
    (
        "dontAsk",
        "Don't Ask",
        "Runs only what is allowed already; denies the rest",
    ),
    (
        BYPASS_PERMISSIONS,
        "Bypass Permissions",
        "Runs everything without asking",
    ),
];

pub fn provider_modes(provider: &str) -> &'static [(&'static str, &'static str, &'static str)] {
    match provider {
        "codex" => codex::MODES,
        _ => CLAUDE_MODES,
    }
}

/// The mode an agent starts in when none was picked: its bypass setting, in the provider's terms.
pub fn default_mode(options: &AgentOptions) -> &'static str {
    match (options.provider.as_str(), options.bypass_permissions) {
        ("codex", bypass) => codex::default_mode(bypass),
        (_, true) => BYPASS_PERMISSIONS,
        (_, false) => "default",
    }
}

/// Models a provider offers before it has said: Claude Code's aliases, Codex's default.
fn fallback_models(provider: &str) -> Vec<(String, String)> {
    let models: &[(&str, &str)] = match provider {
        "claude" => &[
            (DEFAULT_MODEL, "Default"),
            ("opus", "Opus"),
            ("sonnet", "Sonnet"),
            ("haiku", "Haiku"),
        ],
        _ => &[(DEFAULT_MODEL, "Default")],
    };
    models
        .iter()
        .map(|(id, name)| (id.to_string(), name.to_string()))
        .collect()
}

/// Codex app-server's `model/list` answer as `{ id, name }`, the provider default first.
fn codex_models(listed: &Value) -> Vec<Value> {
    let items: Vec<&Value> = listed["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["hidden"] != json!(true))
        .collect();
    let id = |item: &Value| {
        item["model"]
            .as_str()
            .or_else(|| item["id"].as_str())
            .map(str::to_string)
    };
    // A per-turn model sticks, so going back to the default has to name it.
    let default = items
        .iter()
        .find(|item| item["isDefault"] == json!(true))
        .and_then(|item| id(item));
    let mut models = vec![json!({ "id": DEFAULT_MODEL, "name": "Default", "resolves": default })];
    for item in items {
        let Some(id) = id(item) else { continue };
        let name = item["displayName"].as_str().unwrap_or(&id).to_string();
        models.push(json!({ "id": id, "name": name }));
    }
    models
}

/// Claude Code's `initialize` answer: its slash commands, models and permission mode.
fn claude_capabilities(response: &Value) -> Value {
    let commands: Vec<Value> = response["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|command| {
            let name = command["name"].as_str()?;
            // Claude Code's own `__` commands are internal and never typed.
            (!name.is_empty() && !name.starts_with("__")).then(|| {
                json!({
                    "name": name,
                    "description": command["description"].as_str().unwrap_or_default(),
                    "hint": command["argumentHint"].as_str().filter(|hint| !hint.is_empty()),
                })
            })
        })
        .collect();
    let models: Vec<Value> = response["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let id = model["value"].as_str()?;
            Some(json!({ "id": id, "name": model["displayName"].as_str().unwrap_or(id) }))
        })
        .collect();
    json!({
        "commands": commands,
        "models": models,
        "permission_mode": response["current_permission_mode"],
    })
}

type Reply = std::result::Result<Value, String>;

async fn settle(what: &str, reply: oneshot::Receiver<Reply>) -> Result<Value> {
    match tokio::time::timeout(CONTROL_TIMEOUT, reply).await {
        Err(_) => Err(anyhow!("{what} timed out")),
        Ok(Err(_)) => Err(anyhow!("the agent exited during {what}")),
        Ok(Ok(Err(message))) => Err(anyhow!("{what}: {message}")),
        Ok(Ok(Ok(result))) => Ok(result),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Starting,
    Idle,
    Working,
    /// Mid-turn and blocked on the user: a tool permission or a question.
    Waiting,
    /// No process right now; the next message resumes the provider session.
    Sleeping,
    Exited,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub seq: u64,
    pub at_ms: u64,
    pub kind: String,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOptions {
    pub provider: String,
    pub cwd: PathBuf,
    pub workspace: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    /// Bypass permission prompts (plan D22 default).
    pub bypass_permissions: bool,
    /// Provider session to resume, e.g. a Claude Code session id.
    pub resume: Option<String>,
    /// Overrides the provider's command line; used by tests and custom agents.
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Managed account (plan D13); its config dir is already in `env`.
    #[serde(default)]
    pub account: Option<String>,
    /// Start Claude Code with its own Remote Control (plan D21); switchable later per agent.
    #[serde(default)]
    pub remote_control: bool,
    /// The mode to start in (see `provider_modes`); unset follows `bypass_permissions`.
    #[serde(default)]
    pub permission_mode: Option<String>,
}

/// Agent events the server reacts to (notifications), forwarded with the agent's id.
pub type AgentSignal = (String, AgentEvent);

const SIGNAL_KINDS: &[&str] = &[
    "turn_end",
    "exit",
    "notice",
    "tool_call",
    "permission_request",
];

/// The signal sent whenever an agent's shown state changes; it is never logged.
pub const STATE_SIGNAL: &str = "state";

#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    pub id: String,
    pub provider: String,
    pub title: Option<String>,
    pub workspace: Option<String>,
    pub cwd: PathBuf,
    pub state: AgentState,
    pub provider_session: Option<String>,
    pub model: Option<String>,
    /// The permission mode (Claude Code) or approval preset (Codex) the agent works in.
    pub mode: String,
    pub turns: u64,
    pub created_at_ms: u64,
    pub last_activity_ms: u64,
    pub next_seq: u64,
    pub rate_limits: Option<Value>,
    pub rate_limits_at_ms: Option<u64>,
    pub account: Option<String>,
    pub remote_control: bool,
    /// The claude.ai page of the Remote Control session, once Claude Code has registered it.
    pub remote_control_url: Option<String>,
    /// Background tasks (shells, monitors, subagents) still running after the turn that started them.
    pub background_tasks: usize,
    #[serde(flatten)]
    pub live: crate::subagents::Activity,
    /// Permission and question requests waiting for the user's answer.
    pub permission_requests: Vec<Value>,
    /// Bumped on every change of the shown state, so a client keeps the newest of a poll and a push.
    pub revision: u64,
}

impl AgentInfo {
    /// What workspace cards and agent tabs need to draw the agent's state.
    pub fn summary(&self) -> Value {
        json!({
            "id": self.id, "title": self.title, "state": self.state, "provider": self.provider,
            "workspace": self.workspace, "turns": self.turns,
            "last_activity_ms": self.last_activity_ms, "background_tasks": self.background_tasks,
            "revision": self.revision,
            "activity": self.live.activity, "subagents": self.live.subagents,
            "waiting_reason": self.permission_requests.first().map(|request| request["reason"].clone()),
        })
    }
}

// Hooks run on the blocking pool because snapshots shell out to git.
pub trait TurnHook: Send + Sync {
    fn turn_started(&self, agent: &AgentInfo, prompt: &str);
    fn turn_ended(&self, agent: &AgentInfo);
}

struct State {
    state: AgentState,
    events: Vec<AgentEvent>,
    provider_session: Option<String>,
    model: Option<String>,
    turns: u64,
    /// What the first named prompt asked, for agents nobody titled.
    prompt_title: Option<String>,
    last_activity_ms: u64,
    rate_limits: Option<Value>,
    rate_limits_at_ms: Option<u64>,
    killed: bool,
    /// Set by `sleep`, so the process ending is not mistaken for a crash.
    parking: bool,
    remote_control: bool,
    /// Picked with `set_mode` or reported by the agent; restarts start in it.
    mode: Option<String>,
    /// Picked with `set_model`; wins over the creation-time model for later turns and restarts.
    chosen_model: Option<String>,
    /// What the provider said it offers: `{ commands, models }`.
    capabilities: Value,
    /// Open `can_use_tool` requests by request id, in arrival order.
    permissions: Vec<(String, Value)>,
    /// Live background task ids, from Claude Code's task frames.
    background: BTreeSet<String>,
    live: crate::subagents::Activity,
    /// The turn ended and its end snapshot is still being taken.
    finishing: bool,
    revision: u64,
}

impl State {
    fn shown(&self) -> (AgentState, usize, crate::subagents::Activity) {
        (self.state, self.background.len(), self.live.clone())
    }

    fn track_background(&mut self, frame: &Value) {
        let id = frame["task_id"].as_str().map(str::to_string);
        match frame["subtype"].as_str().unwrap_or_default() {
            "background_tasks_changed" => {
                self.background = frame["tasks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|task| task["task_id"].as_str().map(str::to_string))
                    .collect();
            }
            "task_started" => {
                let backgrounded = frame["is_backgrounded"] == json!(true)
                    || matches!(frame["task_type"].as_str(), Some("monitor" | "workflow"));
                let ambient = frame["ambient"] == json!(true);
                if let (Some(id), true, false) = (id, backgrounded, ambient) {
                    self.background.insert(id);
                }
            }
            "task_notification" => {
                if let Some(id) = id {
                    self.background.remove(&id);
                }
            }
            "task_updated" => {
                let settled = matches!(
                    frame["patch"]["status"].as_str(),
                    Some("completed" | "failed" | "killed" | "stopped" | "cancelled")
                );
                if let (Some(id), true) = (id, settled) {
                    self.background.remove(&id);
                }
            }
            _ => {}
        }
    }
}

fn note_prompt_title(state: &mut State, data: &Value) {
    if state.prompt_title.is_none() {
        state.prompt_title =
            crate::history::title_from_prompt(data["text"].as_str().unwrap_or_default());
    }
}

/// Applies the events that carry the agent's mode, model choice and capabilities.
fn track_choices(state: &mut State, kind: &str, data: &Value) {
    match kind {
        "mode_changed" | "session" | "status" | "capabilities" => {
            if let Some(mode) = data["permission_mode"].as_str() {
                state.mode = Some(mode.to_string());
            }
        }
        "model_changed" => state.chosen_model = data["model"].as_str().map(str::to_string),
        _ => {}
    }
    if kind == "capabilities" {
        for key in ["commands", "models"] {
            if data[key].is_array() {
                state.capabilities[key] = data[key].clone();
            }
        }
    }
}

pub struct AgentThread {
    id: String,
    options: AgentOptions,
    created_at_ms: u64,
    log_path: Option<PathBuf>,
    state: Mutex<State>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    child: tokio::sync::Mutex<Option<Child>>,
    events: broadcast::Sender<AgentEvent>,
    turn_hook: std::sync::OnceLock<Arc<dyn TurnHook>>,
    signals: std::sync::OnceLock<mpsc::UnboundedSender<AgentSignal>>,
    codex: codex::Rpc,
    /// Claude Code control requests waiting for their response, by request id.
    control: Mutex<HashMap<String, oneshot::Sender<Reply>>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Claude Code's own flag (`claude --help`: "Start an interactive session with Remote Control enabled").
pub const REMOTE_CONTROL_FLAG: &str = "--remote-control";

fn claude_config_dir(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    env.get("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from))
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
}

/// The claude.ai URL of a session's Remote Control bridge, from Claude Code's `sessions/<pid>.json`.
/// Only those JSON records are read (never the `.key` files beside them); the newest match wins.
pub fn remote_control_url(config_dir: &Path, session_id: &str) -> Option<String> {
    let entries = std::fs::read_dir(config_dir.join("sessions")).ok()?;
    let mut best: Option<(u64, String)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(record) = std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(serde_json::from_slice::<Value>(&bytes)?))
        else {
            continue;
        };
        if record["sessionId"].as_str() != Some(session_id) {
            continue;
        }
        let Some(bridge) = record["bridgeSessionId"].as_str().filter(|b| {
            b.strip_prefix("session_").is_some_and(|rest| {
                !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric())
            })
        }) else {
            continue;
        };
        let updated = record["updatedAt"].as_u64().unwrap_or_default();
        if best.as_ref().is_none_or(|(at, _)| updated > *at) {
            best = Some((updated, format!("https://claude.ai/code/{bridge}")));
        }
    }
    best.map(|(_, url)| url)
}

fn permission_flags(options: &AgentOptions) -> Vec<String> {
    // Lets the mode switch to bypassPermissions later, whatever it starts in.
    let mut flags = vec!["--allow-dangerously-skip-permissions".to_string()];
    match options.permission_mode.as_deref() {
        Some(BYPASS_PERMISSIONS) => flags.push("--dangerously-skip-permissions".into()),
        Some(mode) => flags.extend(["--permission-mode".to_string(), mode.to_string()]),
        None if options.bypass_permissions => flags.push("--dangerously-skip-permissions".into()),
        None => {}
    }
    flags
}

fn spawn_error(program: &str, error: std::io::Error) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::NotFound && !program.contains('/') {
        let path = std::env::var("PATH").unwrap_or_default();
        return anyhow!(
            "start agent `{program}`: not found on the server's PATH ({path}); \
             if it is installed, run `icarus server restart` so the server picks up your shell's PATH"
        );
    }
    anyhow::Error::new(error).context(format!("start agent `{program}`"))
}

/// The command line for a provider, built from the user's installed agent binary.
pub fn provider_command(options: &AgentOptions) -> Result<Vec<String>> {
    if options.command.as_ref().is_some_and(Vec::is_empty) {
        return Err(anyhow!("empty agent command"));
    }
    if options.provider == "codex" {
        return Ok(codex::command(options));
    }
    // Last on the line, because `--remote-control [name]` would take a following value as its name.
    let remote_control = (options.provider == "claude" && options.remote_control)
        .then(|| REMOTE_CONTROL_FLAG.to_string());
    if let Some(command) = &options.command {
        let mut command = command.clone();
        if let Some(session) = &options.resume {
            command.extend(["--resume".to_string(), session.clone()]);
        }
        if options.permission_mode.is_some() {
            command.extend(permission_flags(options));
        }
        if let Some(model) = &options.model {
            command.extend(["--model".to_string(), model.clone()]);
        }
        command.extend(remote_control);
        return Ok(command);
    }
    match options.provider.as_str() {
        "claude" => {
            let mut command = vec![
                "claude".to_string(),
                "-p".into(),
                "--input-format".into(),
                "stream-json".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
                "--include-partial-messages".into(),
                // Permission and question prompts come to Icarus as `can_use_tool` control requests.
                "--permission-prompt-tool".into(),
                "stdio".into(),
            ];
            command.extend(permission_flags(options));
            if let Some(model) = &options.model {
                command.extend(["--model".to_string(), model.clone()]);
            }
            if let Some(session) = &options.resume {
                command.extend(["--resume".to_string(), session.clone()]);
            }
            command.extend(remote_control);
            Ok(command)
        }
        other => Err(anyhow!("unknown agent provider {other}")),
    }
}

/// The `can_use_tool` reply for a decision, plus the permission mode an approved plan switches to.
fn permission_response(
    request: &Value,
    decision: &str,
    answers: Option<Value>,
) -> Result<(Value, Option<&'static str>)> {
    let tool_use_id = request["tool_use_id"].clone();
    let plan = request["reason"] == "plan" || request["tool_name"] == "ExitPlanMode";
    let mode = match (decision, plan) {
        ("allow_accept_edits", true) => Some("acceptEdits"),
        ("allow" | "allow_always", true) => Some("default"),
        ("allow" | "allow_always" | "deny", false) | ("deny", true) => None,
        (other, _) => return Err(anyhow!("unknown decision {other}")),
    };
    if decision == "deny" {
        let message = if plan {
            "The user asked you to keep planning. Revise the plan and call ExitPlanMode again."
        } else {
            "User denied this action."
        };
        return Ok((
            json!({ "behavior": "deny", "message": message, "toolUseID": tool_use_id }),
            None,
        ));
    }
    let mut input = request["input"].clone();
    if let Some(answers) = answers {
        input["answers"] = answers;
    }
    let mut allow = json!({ "behavior": "allow", "updatedInput": input, "toolUseID": tool_use_id });
    if let Some(mode) = mode {
        allow["updatedPermissions"] =
            json!([{ "type": "setMode", "mode": mode, "destination": "session" }]);
    } else if decision == "allow_always" && request["suggestions"].is_array() {
        allow["updatedPermissions"] = request["suggestions"].clone();
    }
    Ok((allow, mode))
}

/// Maps one stream-JSON line from the agent to zero or more normalized events.
pub fn normalize(line: &Value) -> Vec<(String, Value)> {
    let kind = line["type"].as_str().unwrap_or_default();
    match kind {
        "system" => match line["subtype"].as_str().unwrap_or_default() {
            "init" => vec![(
                "session".into(),
                json!({ "session_id": line["session_id"], "model": line["model"], "cwd": line["cwd"],
                    "permission_mode": line["permissionMode"] }),
            )],
            "status" => vec![(
                "status".into(),
                json!({ "status": line["status"], "permission_mode": line["permissionMode"] }),
            )],
            "api_retry" | "permission_denied" => vec![("notice".into(), line.clone())],
            "task_started" | "task_notification" | "task_updated" | "background_tasks_changed" => {
                vec![("task".into(), line.clone())]
            }
            _ => Vec::new(),
        },
        "control_request" if line["request"]["subtype"] == "can_use_tool" => {
            let request = &line["request"];
            let reason = match request["tool_name"].as_str() {
                Some("ExitPlanMode") => "plan",
                Some("AskUserQuestion") => "question",
                _ => "permission",
            };
            let mut data = json!({
                "request_id": line["request_id"],
                "tool_name": request["tool_name"],
                "tool_use_id": request["tool_use_id"],
                "input": request["input"],
                "suggestions": request["permission_suggestions"],
                "question": reason == "question",
                "reason": reason,
            });
            if reason == "plan" {
                data["plan"] = request["input"]["plan"].clone();
            }
            vec![("permission_request".into(), data)]
        }
        "control_cancel_request" => vec![(
            "permission_resolved".into(),
            json!({ "request_id": line["request_id"], "decision": "cancelled" }),
        )],
        "stream_event" => {
            let event = &line["event"];
            match event["type"].as_str().unwrap_or_default() {
                "content_block_delta" => {
                    match event["delta"]["type"].as_str().unwrap_or_default() {
                        "text_delta" => vec![(
                            "text_delta".into(),
                            json!({ "index": event["index"], "text": event["delta"]["text"], "parent": line["parent_tool_use_id"] }),
                        )],
                        "thinking_delta" => vec![(
                            "thinking_delta".into(),
                            json!({ "index": event["index"], "text": event["delta"]["thinking"], "parent": line["parent_tool_use_id"] }),
                        )],
                        _ => Vec::new(),
                    }
                }
                "message_start" => vec![(
                    "message_start".into(),
                    json!({ "message_id": event["message"]["id"], "parent": line["parent_tool_use_id"] }),
                )],
                _ => Vec::new(),
            }
        }
        "assistant" => {
            let message = &line["message"];
            let mut out = vec![(
                "message".into(),
                json!({ "message_id": message["id"], "content": message["content"], "parent": line["parent_tool_use_id"] }),
            )];
            for block in message["content"].as_array().into_iter().flatten() {
                if block["type"] == "tool_use" {
                    out.push((
                        "tool_call".into(),
                        json!({ "id": block["id"], "name": block["name"], "input": block["input"], "parent": line["parent_tool_use_id"] }),
                    ));
                    if block["name"] == "TodoWrite" {
                        out.push(("plan".into(), json!({ "todos": block["input"]["todos"] })));
                    }
                }
            }
            out
        }
        "user" => line["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"] == "tool_result")
            .map(|block| {
                (
                    "tool_result".into(),
                    json!({
                        "tool_use_id": block["tool_use_id"],
                        "content": block["content"],
                        "is_error": block["is_error"].as_bool().unwrap_or(false),
                        "detail": line["tool_use_result"],
                        "parent": line["parent_tool_use_id"],
                    }),
                )
            })
            .collect(),
        "rate_limit_event" => vec![("rate_limit".into(), line["rate_limit_info"].clone())],
        "result" => vec![(
            "turn_end".into(),
            json!({
                "subtype": line["subtype"],
                "is_error": line["is_error"],
                "result": line["result"],
                "stop_reason": line["stop_reason"],
                "duration_ms": line["duration_ms"],
                "cost_usd": line["total_cost_usd"],
                "num_turns": line["num_turns"],
            }),
        )],
        _ => Vec::new(),
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Meta {
    pub(crate) id: String,
    pub(crate) options: AgentOptions,
    pub(crate) created_at_ms: u64,
}

impl AgentThread {
    fn new(
        id: String,
        options: AgentOptions,
        created_at_ms: u64,
        log_dir: Option<&Path>,
        events: Vec<AgentEvent>,
    ) -> Arc<Self> {
        let (sender, _) = broadcast::channel(8192);
        let mut state = State {
            state: AgentState::Starting,
            events: Vec::new(),
            provider_session: None,
            model: None,
            turns: 0,
            prompt_title: None,
            last_activity_ms: created_at_ms,
            rate_limits: None,
            rate_limits_at_ms: None,
            killed: false,
            parking: false,
            remote_control: options.remote_control,
            mode: None,
            chosen_model: None,
            capabilities: json!({}),
            permissions: Vec::new(),
            background: BTreeSet::new(),
            live: Default::default(),
            finishing: false,
            // Starts at the clock so revisions keep rising across server restarts.
            revision: now_ms(),
        };
        for event in events {
            track_choices(&mut state, &event.kind, &event.data);
            match event.kind.as_str() {
                "remote_control" => {
                    state.remote_control = event.data["enabled"].as_bool().unwrap_or_default();
                }
                "session" => {
                    state.provider_session = event.data["session_id"].as_str().map(str::to_string);
                    state.model = event.data["model"].as_str().map(str::to_string);
                }
                "user_message" => {
                    state.turns += 1;
                    note_prompt_title(&mut state, &event.data);
                }
                "rate_limit" => {
                    state.rate_limits = Some(event.data.clone());
                    state.rate_limits_at_ms = Some(event.at_ms);
                }
                "kill" => state.killed = true,
                "wake" => state.killed = false,
                _ => {}
            }
            state.last_activity_ms = event.at_ms;
            state.events.push(event);
        }
        if state.killed {
            state.state = AgentState::Exited;
        }
        Arc::new(Self {
            log_path: log_dir.map(|dir| dir.join(format!("{id}.jsonl"))),
            id,
            options,
            created_at_ms,
            state: Mutex::new(state),
            stdin: tokio::sync::Mutex::new(None),
            child: tokio::sync::Mutex::new(None),
            events: sender,
            turn_hook: std::sync::OnceLock::new(),
            signals: std::sync::OnceLock::new(),
            codex: codex::Rpc::default(),
            control: Mutex::default(),
        })
    }

    fn is_codex(&self) -> bool {
        self.options.provider == "codex"
    }

    pub fn set_turn_hook(&self, hook: Arc<dyn TurnHook>) {
        let _ = self.turn_hook.set(hook);
    }

    pub fn set_signals(&self, signals: mpsc::UnboundedSender<AgentSignal>) {
        let _ = self.signals.set(signals);
    }

    pub fn options(&self) -> &AgentOptions {
        &self.options
    }

    async fn run_turn_hook(&self, prompt: Option<&str>) {
        let Some(hook) = self.turn_hook.get().cloned() else {
            return;
        };
        let info = self.info();
        let prompt = prompt.map(str::to_string);
        let started = std::time::Instant::now();
        let starting = prompt.is_some();
        let _ = tokio::task::spawn_blocking(move || match prompt {
            Some(prompt) => hook.turn_started(&info, &prompt),
            None => hook.turn_ended(&info),
        })
        .await;
        let took = started.elapsed();
        // The start snapshot runs before the prompt reaches the agent, so its time is the user's wait.
        if took > std::time::Duration::from_millis(100) {
            tracing::info!(
                "agent {}: turn {} snapshot took {took:.1?}",
                self.id,
                if starting { "start" } else { "end" }
            );
        }
    }

    pub async fn spawn(
        id: String,
        options: AgentOptions,
        log_dir: Option<&Path>,
    ) -> Result<Arc<Self>> {
        if let Some(dir) = log_dir {
            std::fs::create_dir_all(dir)?;
            let meta = Meta {
                id: id.clone(),
                options: options.clone(),
                created_at_ms: now_ms(),
            };
            std::fs::write(
                dir.join(format!("{id}.meta.json")),
                serde_json::to_vec_pretty(&meta)?,
            )?;
        }
        let thread = Self::new(id, options, now_ms(), log_dir, Vec::new());
        thread.record(
            "created",
            json!({ "provider": thread.options.provider, "title": thread.options.title }),
        );
        let resume = thread.options.resume.clone();
        thread.start_process(resume).await?;
        Ok(thread)
    }

    /// Loads a session saved by an earlier server run; it sleeps until the next message wakes it.
    /// A killed session stays exited unless `revive` is set.
    pub fn restore(meta_path: &Path, log_dir: &Path, revive: bool) -> Result<Arc<Self>> {
        let meta: Meta = serde_json::from_slice(&std::fs::read(meta_path)?)?;
        let log = log_dir.join(format!("{}.jsonl", meta.id));
        let events: Vec<AgentEvent> = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let thread = Self::new(
            meta.id,
            meta.options,
            meta.created_at_ms,
            Some(log_dir),
            events,
        );
        let (resumable, killed) = {
            let mut state = lock(&thread.state);
            if revive {
                state.killed = false;
            }
            (state.provider_session.is_some(), state.killed)
        };
        if !killed {
            thread.record("sleep", json!({ "resumable": resumable }));
        }
        Ok(thread)
    }

    async fn start_process(self: &Arc<Self>, resume: Option<String>) -> Result<()> {
        let mut options = self.options.clone();
        options.resume = resume;
        {
            let state = lock(&self.state);
            options.remote_control = state.remote_control;
            if let Some(mode) = &state.mode {
                options.permission_mode = Some(mode.clone());
            }
            if let Some(model) = &state.chosen_model {
                options.model = (model != DEFAULT_MODEL).then(|| model.clone());
            }
        }
        let command = provider_command(&options)?;
        let mut process = Command::new(&command[0]);
        process
            .args(&command[1..])
            .current_dir(&options.cwd)
            .env("ICARUS_AGENT_ID", &self.id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(workspace) = &options.workspace {
            process.env("ICARUS_WORKSPACE_ID", workspace);
        }
        if options.account.is_some() {
            for key in crate::accounts::auth_env_vars(&options.provider) {
                process.env_remove(key);
            }
        }
        for (key, value) in &options.env {
            process.env(key, value);
        }
        let mut child = process
            .spawn()
            .map_err(|error| spawn_error(&command[0], error))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("agent stdout")?;
        let stderr = child.stderr.take().context("agent stderr")?;
        *self.stdin.lock().await = stdin;
        *self.child.lock().await = Some(child);
        let woke = {
            let mut state = lock(&self.state);
            state.parking = false;
            let asleep = matches!(state.state, AgentState::Exited | AgentState::Sleeping);
            if asleep {
                state.state = AgentState::Starting;
            }
            asleep
        };
        if woke {
            self.state_changed();
        }

        let reading = Arc::clone(self);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let events = reading.decode_line(&line).await;
                // The end snapshot lands before `turn_end` is published, so waiters see the turn.
                if events.iter().any(|(kind, _)| kind == "turn_end") {
                    reading.turn_finishing();
                    reading.run_turn_hook(None).await;
                }
                for (kind, data) in events {
                    reading.record(&kind, data);
                }
            }
            reading.finish().await;
        });
        let errors = Arc::clone(self);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    errors.record("stderr", json!({ "line": line }));
                }
            }
        });
        if !self.is_codex() {
            // Written before any message, so the answer describes the session from its start.
            let reply = self
                .control_request(json!({ "subtype": "initialize" }))
                .await?;
            let asking = Arc::clone(self);
            tokio::spawn(async move {
                if let Ok(response) = settle("initialize", reply).await {
                    // Claude Code only reports its session once it gets a message; answering means it waits for one.
                    let started = {
                        let mut state = lock(&asking.state);
                        let starting = state.state == AgentState::Starting;
                        if starting {
                            state.state = AgentState::Idle;
                        }
                        starting
                    };
                    if started {
                        asking.state_changed();
                    }
                    asking.record_capabilities(claude_capabilities(&response));
                }
            });
            return Ok(());
        }
        if let Err(error) = self.open_codex_thread(&options).await {
            self.stdin.lock().await.take();
            if let Some(child) = self.child.lock().await.as_mut() {
                let _ = child.start_kill();
            }
            return Err(error);
        }
        let asking = Arc::clone(self);
        tokio::spawn(async move {
            if let Ok(listed) = asking.codex_request("model/list", json!({})).await {
                asking.record_capabilities(json!({ "models": codex_models(&listed) }));
            }
        });
        Ok(())
    }

    /// Records what the provider offers, unless it said the same before.
    fn record_capabilities(&self, capabilities: Value) {
        {
            let state = lock(&self.state);
            let unchanged = ["commands", "models"].iter().all(|key| {
                capabilities[key].is_null() || capabilities[key] == state.capabilities[key]
            });
            let mode = capabilities["permission_mode"].as_str();
            if unchanged && (mode.is_none() || mode == state.mode.as_deref()) {
                return;
            }
        }
        self.record("capabilities", capabilities);
    }

    /// Writes a Claude Code control request; the receiver gets its response.
    async fn control_request(&self, request: Value) -> Result<oneshot::Receiver<Reply>> {
        let id = uuid::Uuid::new_v4().to_string();
        let (sender, reply) = oneshot::channel();
        lock(&self.control).insert(id.clone(), sender);
        let line = json!({ "type": "control_request", "request_id": id, "request": request });
        if let Err(error) = self.write_line(&line).await {
            lock(&self.control).remove(&id);
            return Err(error);
        }
        Ok(reply)
    }

    fn control_response(&self, response: &Value) {
        let Some(sender) = response["request_id"]
            .as_str()
            .and_then(|id| lock(&self.control).remove(id))
        else {
            return;
        };
        let reply = if response["subtype"] == "error" {
            Err(response["error"]
                .as_str()
                .map_or_else(|| response["error"].to_string(), str::to_string))
        } else {
            Ok(response["response"].clone())
        };
        let _ = sender.send(reply);
    }

    async fn write_line(&self, message: &Value) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        let pipe = stdin.as_mut().context("agent input is closed")?;
        pipe.write_all(format!("{message}\n").as_bytes()).await?;
        pipe.flush().await?;
        Ok(())
    }

    async fn codex_request(&self, method: &str, params: Value) -> Result<Value> {
        let (request, reply) = self.codex.request(method, params);
        self.write_line(&request).await?;
        match tokio::time::timeout(CODEX_REQUEST_TIMEOUT, reply).await {
            Err(_) => Err(anyhow!("codex {method} timed out")),
            Ok(Err(_)) => Err(anyhow!("codex app-server exited during {method}")),
            Ok(Ok(Err(message))) => Err(anyhow!("codex {method}: {message}")),
            Ok(Ok(Ok(result))) => Ok(result),
        }
    }

    async fn open_codex_thread(&self, options: &AgentOptions) -> Result<()> {
        self.codex_request("initialize", codex::initialize_params())
            .await?;
        self.write_line(&json!({ "method": "initialized" })).await?;
        let opened = match options.resume.as_deref() {
            Some(thread_id) => match self
                .codex_request("thread/resume", codex::resume_params(options, thread_id))
                .await
            {
                Err(error) if codex::is_no_rollout(&format!("{error}"), thread_id) => {
                    self.record(
                        "notice",
                        json!({ "message": format!("codex thread {thread_id} was never saved; started a new one") }),
                    );
                    self.codex_request("thread/start", codex::start_params(options))
                        .await?
                }
                other => other?,
            },
            None => {
                self.codex_request("thread/start", codex::start_params(options))
                    .await?
            }
        };
        let thread_id = opened["thread"]["id"]
            .as_str()
            .context("codex app-server did not name the thread it opened")?;
        self.record(
            "session",
            json!({ "session_id": thread_id, "model": opened["model"], "cwd": self.options.cwd }),
        );
        Ok(())
    }

    /// Turns one stdout line into normalized events, answering Codex protocol traffic on the way.
    async fn decode_line(&self, line: &str) -> Vec<(String, Value)> {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return vec![("stdout".into(), json!({ "line": line }))];
        };
        if !self.is_codex() {
            if value["type"] == "control_response" {
                self.control_response(&value["response"]);
                return Vec::new();
            }
            return normalize(&value);
        }
        match self.codex.incoming(value) {
            codex::Incoming::Notification { method, params } => codex::normalize(&method, &params),
            codex::Incoming::Request { id, method } => {
                let approve = self.current_mode() == codex::FULL_ACCESS;
                let reply = match codex::reply_to_request(&method, approve) {
                    Ok(result) => json!({ "id": id, "result": result }),
                    Err((code, message)) => {
                        json!({ "id": id, "error": { "code": code, "message": message } })
                    }
                };
                let _ = self.write_line(&reply).await;
                vec![(
                    "notice".into(),
                    json!({ "request": method, "reply": reply.get("result").unwrap_or(&reply["error"]) }),
                )]
            }
            codex::Incoming::Response | codex::Incoming::Other => Vec::new(),
        }
    }

    /// Puts the agent to sleep: its process ends, the session stays resumable.
    pub async fn sleep(&self) -> Result<()> {
        lock(&self.state).parking = true;
        self.stdin.lock().await.take();
        if let Some(child) = self.child.lock().await.as_mut() {
            let _ = child.start_kill();
        }
        Ok(())
    }

    fn record(&self, kind: &str, data: Value) -> AgentEvent {
        let mut state = lock(&self.state);
        let event = AgentEvent {
            seq: state.events.len() as u64,
            at_ms: now_ms(),
            kind: kind.to_string(),
            data,
        };
        state.last_activity_ms = event.at_ms;
        let shown = state.shown();
        track_choices(&mut state, kind, &event.data);
        match kind {
            "permission_request" => {
                if let Some(id) = event.data["request_id"].as_str() {
                    state.permissions.push((id.to_string(), event.data.clone()));
                    state.state = AgentState::Waiting;
                }
            }
            "permission_resolved" => {
                let id = event.data["request_id"].as_str().unwrap_or_default();
                state.permissions.retain(|(open, _)| open != id);
                if state.permissions.is_empty() && state.state == AgentState::Waiting {
                    state.state = AgentState::Working;
                }
            }
            "task" => state.track_background(&event.data),
            "remote_control" => {
                state.remote_control = event.data["enabled"].as_bool().unwrap_or_default();
            }
            "session" => {
                state.provider_session = event.data["session_id"].as_str().map(str::to_string);
                state.model = event.data["model"].as_str().map(str::to_string);
                if state.state == AgentState::Starting {
                    state.state = AgentState::Idle;
                }
            }
            "user_message" => {
                state.state = AgentState::Working;
                state.turns += 1;
                note_prompt_title(&mut state, &event.data);
            }
            "turn_end" => {
                state.state = AgentState::Idle;
                state.finishing = false;
                state.permissions.clear();
            }
            "rate_limit" => {
                state.rate_limits = Some(event.data.clone());
                state.rate_limits_at_ms = Some(event.at_ms);
            }
            "sleep" => {
                // An agent that never got a message has nothing to resume; waking it just starts fresh.
                state.state = if state.killed {
                    AgentState::Exited
                } else {
                    AgentState::Sleeping
                };
            }
            "exit" => {
                state.state = if state.killed || state.provider_session.is_none() {
                    AgentState::Exited
                } else {
                    AgentState::Sleeping
                };
                state.permissions.clear();
                state.background.clear();
                state.finishing = false;
            }
            _ => {}
        }
        let background = std::mem::take(&mut state.background);
        state
            .live
            .observe(kind, &event.data, event.at_ms, &background);
        state.background = background;
        let changed = state.shown() != shown;
        state.events.push(event.clone());
        if let Some(path) = &self.log_path
            && let Ok(line) = serde_json::to_string(&event)
        {
            use std::io::Write as _;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "{line}");
            }
        }
        // Sent under the lock so an attach never sees a seq without its live event.
        let _ = self.events.send(event.clone());
        drop(state);
        if SIGNAL_KINDS.contains(&kind)
            && let Some(signals) = self.signals.get()
        {
            let _ = signals.send((self.id.clone(), event.clone()));
        }
        if changed {
            self.state_changed();
        }
        event
    }

    /// Tells the server the agent's shown state moved, so clients redraw it without polling.
    fn state_changed(&self) {
        lock(&self.state).revision += 1;
        if let Some(signals) = self.signals.get() {
            let event = AgentEvent {
                seq: u64::MAX,
                at_ms: now_ms(),
                kind: STATE_SIGNAL.into(),
                data: self.info_without_url().summary(),
            };
            let _ = signals.send((self.id.clone(), event));
        }
    }

    /// Shows the turn as done while its end snapshot is taken; `agent.wait` still waits for `turn_end`.
    fn turn_finishing(&self) {
        {
            let mut state = lock(&self.state);
            if !matches!(state.state, AgentState::Working | AgentState::Waiting) {
                return;
            }
            state.state = AgentState::Idle;
            state.finishing = true;
            state.permissions.clear();
            let background = std::mem::take(&mut state.background);
            state
                .live
                .observe("turn_end", &Value::Null, now_ms(), &background);
            state.background = background;
        }
        self.state_changed();
    }

    /// Idle with the last turn fully recorded.
    pub fn settled(&self) -> bool {
        let state = lock(&self.state);
        state.state == AgentState::Idle && !state.finishing
    }

    async fn finish(&self) {
        self.codex.reset();
        lock(&self.control).clear();
        let code = match self.child.lock().await.as_mut() {
            Some(child) => child.wait().await.ok().and_then(|status| status.code()),
            None => None,
        };
        let (killed, parking) = {
            let state = lock(&self.state);
            (state.killed, state.parking)
        };
        self.record(
            "exit",
            json!({ "code": code, "killed": killed, "unexpected": !killed && !parking }),
        );
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn info(&self) -> AgentInfo {
        let mut info = self.info_without_url();
        let running = matches!(info.state, AgentState::Idle | AgentState::Working);
        if info.remote_control
            && running
            && let (Some(session), Some(dir)) = (
                info.provider_session.as_deref(),
                claude_config_dir(&self.options.env),
            )
        {
            info.remote_control_url = remote_control_url(&dir, session);
        }
        info
    }

    /// Switches Claude Code's Remote Control for this agent. A running, idle agent restarts on its
    /// session right away; a busy one picks it up the next time its process starts.
    pub async fn set_remote_control(self: &Arc<Self>, enabled: bool) -> Result<bool> {
        if self.options.provider != "claude" {
            return Err(anyhow!(
                "Remote Control is a Claude Code feature; agent {} runs {}",
                self.id,
                self.options.provider
            ));
        }
        if lock(&self.state).remote_control == enabled {
            return Ok(false);
        }
        self.record("remote_control", json!({ "enabled": enabled }));
        if !matches!(self.state(), AgentState::Idle | AgentState::Starting) {
            return Ok(false);
        }
        self.sleep().await?;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while matches!(self.state(), AgentState::Idle | AgentState::Starting) {
            if std::time::Instant::now() > deadline {
                return Err(anyhow!("agent {} did not stop to restart", self.id));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let session = lock(&self.state).provider_session.clone();
        self.record("wake", json!({ "resume": session }));
        self.start_process(session).await?;
        Ok(true)
    }

    fn info_without_url(&self) -> AgentInfo {
        let state = lock(&self.state);
        AgentInfo {
            id: self.id.clone(),
            provider: self.options.provider.clone(),
            title: crate::history::session_title(
                self.options.title.as_deref(),
                &self.options.provider,
                state.prompt_title.clone(),
            ),
            workspace: self.options.workspace.clone(),
            cwd: self.options.cwd.clone(),
            state: state.state,
            provider_session: state.provider_session.clone(),
            model: state
                .chosen_model
                .clone()
                .filter(|model| model != DEFAULT_MODEL)
                .or_else(|| state.model.clone())
                .or_else(|| self.options.model.clone()),
            mode: self.mode_of(&state),
            turns: state.turns,
            created_at_ms: self.created_at_ms,
            last_activity_ms: state.last_activity_ms,
            next_seq: state.events.len() as u64,
            rate_limits: state.rate_limits.clone(),
            rate_limits_at_ms: state.rate_limits_at_ms,
            account: self.options.account.clone(),
            remote_control: state.remote_control,
            remote_control_url: None,
            background_tasks: state.background.len(),
            live: state.live.clone(),
            permission_requests: state.permissions.iter().map(|(_, r)| r.clone()).collect(),
            revision: state.revision,
        }
    }

    pub fn state(&self) -> AgentState {
        lock(&self.state).state
    }

    /// Events from `since` plus a live subscription, taken atomically.
    pub fn attach(&self, since: u64) -> (Vec<AgentEvent>, broadcast::Receiver<AgentEvent>) {
        let state = lock(&self.state);
        let receiver = self.events.subscribe();
        let backlog = state
            .events
            .iter()
            .skip(usize::try_from(since).unwrap_or(usize::MAX))
            .cloned()
            .collect();
        (backlog, receiver)
    }

    pub fn events_since(&self, since: u64) -> Vec<AgentEvent> {
        self.attach(since).0
    }

    /// Sends a user message; images are `(media_type, base64)` pairs.
    pub async fn send(self: &Arc<Self>, text: &str, images: &[(String, String)]) -> Result<()> {
        if self.state() == AgentState::Exited {
            return Err(anyhow!("agent {} has exited", self.id));
        }
        // The next turn's start snapshot must not race the last turn's end snapshot.
        while lock(&self.state).finishing {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.wake().await?;
        if self.is_codex() {
            return self.send_codex(text, images).await;
        }
        let mut content = Vec::new();
        for (media_type, data) in images {
            content.push(json!({ "type": "image", "source": { "type": "base64", "media_type": media_type, "data": data } }));
        }
        content.push(json!({ "type": "text", "text": text }));
        self.run_turn_hook(Some(text)).await;
        let line = json!({ "type": "user", "message": { "role": "user", "content": content } });
        let mut stdin = self.stdin.lock().await;
        let pipe = stdin.as_mut().context("agent input is closed")?;
        // Recorded before the write: a fast agent's reply must not land ahead of the message in the log.
        self.record(
            "user_message",
            json!({ "text": text, "images": images.len() }),
        );
        pipe.write_all(format!("{line}\n").as_bytes()).await?;
        pipe.flush().await?;
        drop(stdin);
        Ok(())
    }

    /// Starts a sleeping agent's process again, resuming its provider session; false if it wasn't asleep.
    pub async fn wake(self: &Arc<Self>) -> Result<bool> {
        if self.state() != AgentState::Sleeping {
            return Ok(false);
        }
        let session = lock(&self.state).provider_session.clone();
        self.record("wake", json!({ "resume": session }));
        self.start_process(session).await?;
        Ok(true)
    }

    async fn send_codex(&self, text: &str, images: &[(String, String)]) -> Result<()> {
        let thread_id = lock(&self.state)
            .provider_session
            .clone()
            .context("codex thread is not open")?;
        self.run_turn_hook(Some(text)).await;
        let mut params = codex::turn_params(&thread_id, text, images, &self.current_mode());
        if let Some(model) = self.codex_turn_model() {
            params["model"] = json!(model);
        }
        let (request, reply) = self.codex.request("turn/start", params);
        self.record(
            "user_message",
            json!({ "text": text, "images": images.len() }),
        );
        self.write_line(&request).await?;
        let error = match tokio::time::timeout(CODEX_REQUEST_TIMEOUT, reply).await {
            Ok(Ok(Ok(_))) => return Ok(()),
            Ok(Ok(Err(message))) => anyhow!("codex turn/start: {message}"),
            Ok(Err(_)) => anyhow!("codex app-server exited during turn/start"),
            Err(_) => anyhow!("codex turn/start timed out"),
        };
        if self.state() == AgentState::Working {
            self.run_turn_hook(None).await;
            self.record(
                "turn_end",
                json!({ "subtype": "failed", "is_error": true, "result": format!("{error:#}") }),
            );
        }
        Err(error)
    }

    fn codex_turn_model(&self) -> Option<String> {
        let state = lock(&self.state);
        let chosen = state.chosen_model.clone()?;
        if chosen != DEFAULT_MODEL {
            return Some(chosen);
        }
        state.capabilities["models"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|model| model["id"] == DEFAULT_MODEL)
            .and_then(|model| model["resolves"].as_str().map(str::to_string))
    }

    /// The models the provider offers as `(id, name)`, the provider default first.
    pub fn models(&self) -> Vec<(String, String)> {
        let state = lock(&self.state);
        let reported: Vec<(String, String)> = state.capabilities["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|model| {
                Some((
                    model["id"].as_str()?.to_string(),
                    model["name"].as_str()?.to_string(),
                ))
            })
            .collect();
        if reported.is_empty() {
            fallback_models(&self.options.provider)
        } else {
            reported
        }
    }

    /// The model the next turn uses: the picked one, else the one it was started with.
    pub fn current_model(&self) -> String {
        lock(&self.state)
            .chosen_model
            .clone()
            .or_else(|| self.options.model.clone())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string())
    }

    /// Whether a process is up to take control requests now; otherwise the next start applies the choice.
    async fn running(&self) -> bool {
        self.stdin.lock().await.is_some()
    }

    /// Switches the model: Claude Code switches its running session, Codex takes it with the next
    /// turn, and restarts keep it.
    pub async fn set_model(&self, model: &str) -> Result<()> {
        if model.trim().is_empty() {
            return Err(anyhow!("no model given"));
        }
        if !self.is_codex() && self.running().await {
            let reply = self
                .control_request(json!({ "subtype": "set_model", "model": model }))
                .await?;
            settle("set_model", reply).await?;
        }
        self.record("model_changed", json!({ "model": model }));
        Ok(())
    }

    fn mode_of(&self, state: &State) -> String {
        state
            .mode
            .clone()
            .or_else(|| self.options.permission_mode.clone())
            .unwrap_or_else(|| default_mode(&self.options).to_string())
    }

    pub fn current_mode(&self) -> String {
        self.mode_of(&lock(&self.state))
    }

    /// Switches the permission mode: Claude Code switches its running session, Codex applies it
    /// from the next turn, and restarts keep it.
    pub async fn set_mode(&self, mode: &str) -> Result<()> {
        let modes = provider_modes(&self.options.provider);
        if !modes.iter().any(|(id, _, _)| *id == mode) {
            let known: Vec<&str> = modes.iter().map(|(id, _, _)| *id).collect();
            return Err(anyhow!(
                "{} has no mode {mode}; pick one of {}",
                self.options.provider,
                known.join(", ")
            ));
        }
        if !self.is_codex() && self.running().await {
            let reply = self
                .control_request(json!({ "subtype": "set_permission_mode", "mode": mode }))
                .await?;
            settle("set_permission_mode", reply).await?;
        }
        self.record("mode_changed", json!({ "permission_mode": mode }));
        Ok(())
    }

    /// The provider's slash commands as `{ name, description, hint }`; empty until it has said.
    pub fn commands(&self) -> Vec<Value> {
        lock(&self.state).capabilities["commands"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// Answers an open permission or question request: `allow`, `allow_always` or `deny`, and
    /// for a plan also `allow_accept_edits`.
    /// A question's answers (question text to chosen label) go in `answers`.
    pub async fn answer(
        &self,
        request_id: &str,
        decision: &str,
        answers: Option<Value>,
    ) -> Result<()> {
        let request = lock(&self.state)
            .permissions
            .iter()
            .find(|(id, _)| id == request_id)
            .map(|(_, request)| request.clone())
            .with_context(|| format!("agent {} has no open request {request_id}", self.id))?;
        let (response, mode) = permission_response(&request, decision, answers)?;
        self.write_line(&json!({
            "type": "control_response",
            "response": { "subtype": "success", "request_id": request_id, "response": response },
        }))
        .await?;
        if let Some(mode) = mode {
            self.record("mode_changed", json!({ "permission_mode": mode }));
        }
        self.record(
            "permission_resolved",
            json!({ "request_id": request_id, "decision": decision }),
        );
        Ok(())
    }

    /// Asks the agent to stop the current turn.
    pub async fn interrupt(&self) -> Result<()> {
        if self.is_codex() {
            let thread_id = lock(&self.state)
                .provider_session
                .clone()
                .context("codex thread is not open")?;
            let turn_id = self
                .codex
                .running_turn()
                .context("no codex turn is running")?;
            self.codex_request(
                "turn/interrupt",
                json!({ "threadId": thread_id, "turnId": turn_id }),
            )
            .await?;
            return Ok(());
        }
        let request = json!({
            "type": "control_request",
            "request_id": uuid::Uuid::new_v4().to_string(),
            "request": { "subtype": "interrupt" },
        });
        let mut stdin = self.stdin.lock().await;
        let pipe = stdin.as_mut().context("agent input is closed")?;
        pipe.write_all(format!("{request}\n").as_bytes()).await?;
        pipe.flush().await?;
        Ok(())
    }

    pub async fn kill(&self) -> Result<()> {
        lock(&self.state).killed = true;
        // Logged at once, so a restart before the process is reaped still knows it was killed.
        self.record("kill", json!({}));
        if self.child.lock().await.is_none() || self.state() == AgentState::Sleeping {
            self.record("exit", json!({ "code": null, "killed": true }));
        }
        self.stdin.lock().await.take();
        if let Some(child) = self.child.lock().await.as_mut() {
            let _ = child.start_kill();
        }
        Ok(())
    }

    /// Plain-text transcript for the CLI and for agents reading other agents.
    pub fn transcript(&self) -> String {
        let state = lock(&self.state);
        let mut out = String::new();
        for event in &state.events {
            match event.kind.as_str() {
                "user_message" => {
                    out.push_str("\n> ");
                    out.push_str(event.data["text"].as_str().unwrap_or_default());
                    out.push('\n');
                }
                "message" => {
                    if !event.data["parent"].is_null() {
                        continue;
                    }
                    for block in event.data["content"].as_array().into_iter().flatten() {
                        if block["type"] == "text" {
                            out.push('\n');
                            out.push_str(block["text"].as_str().unwrap_or_default());
                            out.push('\n');
                        }
                    }
                }
                "tool_call" if event.data["parent"].is_null() => {
                    out.push_str(&format!(
                        "\n[{}]\n",
                        event.data["name"].as_str().unwrap_or("tool")
                    ));
                }
                "turn_end" if event.data["is_error"] == json!(true) => {
                    out.push_str("\n[turn failed]\n");
                }
                "exit" => out.push_str("\n[agent exited]\n"),
                _ => {}
            }
        }
        out
    }
}

pub struct AgentManager {
    threads: Mutex<BTreeMap<String, Arc<AgentThread>>>,
    log_dir: Option<PathBuf>,
    turn_hook: Option<Arc<dyn TurnHook>>,
    signals: mpsc::UnboundedSender<AgentSignal>,
    signal_receiver: Mutex<Option<mpsc::UnboundedReceiver<AgentSignal>>>,
}

impl Default for AgentManager {
    fn default() -> Self {
        Self::new(None)
    }
}

impl AgentManager {
    pub fn new(log_dir: Option<PathBuf>) -> Self {
        let (signals, receiver) = mpsc::unbounded_channel();
        Self {
            threads: Mutex::default(),
            log_dir,
            turn_hook: None,
            signals,
            signal_receiver: Mutex::new(Some(receiver)),
        }
    }

    pub fn with_turn_hook(mut self, hook: Arc<dyn TurnHook>) -> Self {
        self.turn_hook = Some(hook);
        self
    }

    /// The turn ends, exits and notices of every agent; only the first caller gets them.
    pub fn take_signals(&self) -> Option<mpsc::UnboundedReceiver<AgentSignal>> {
        lock(&self.signal_receiver).take()
    }

    pub fn log_dir(&self) -> Option<&Path> {
        self.log_dir.as_deref()
    }

    fn adopt(&self, thread: &AgentThread) {
        if let Some(hook) = &self.turn_hook {
            thread.set_turn_hook(Arc::clone(hook));
        }
        thread.set_signals(self.signals.clone());
    }

    /// Reloads one saved session from disk into the live set, replacing a stale entry.
    pub fn restore_one(&self, id: &str) -> Result<Arc<AgentThread>> {
        let dir = self.log_dir.as_deref().context("agents are not saved")?;
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(anyhow!("bad agent id {id}"));
        }
        let meta = dir.join(format!("{id}.meta.json"));
        if !meta.is_file() {
            return Err(anyhow!("no saved agent session {id}"));
        }
        let thread = AgentThread::restore(&meta, dir, true)?;
        self.adopt(&thread);
        lock(&self.threads).insert(thread.id().to_string(), Arc::clone(&thread));
        Ok(thread)
    }

    /// Reloads sessions saved by earlier server runs as sleeping, resumable agents.
    pub fn restore_all(&self) -> usize {
        let Some(dir) = &self.log_dir else { return 0 };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        let mut candidates = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.to_string_lossy().ends_with(".meta.json") {
                continue;
            }
            match AgentThread::restore(&path, dir, false) {
                Ok(thread) => {
                    // Killed sessions stay in history (and `agent.resume`) only.
                    let killed = lock(&thread.state).killed;
                    if !killed && (thread.state() != AgentState::Exited || thread.info().turns > 0)
                    {
                        candidates.push(thread);
                    }
                }
                Err(error) => tracing::warn!("cannot restore {}: {error:#}", path.display()),
            }
        }
        // Unused agents hold nothing, so only the newest per workspace is kept as its warm agent.
        let mut newest_unused: HashMap<Option<String>, (u64, String)> = HashMap::new();
        for thread in &candidates {
            let info = thread.info();
            if info.turns == 0 {
                let entry = newest_unused
                    .entry(info.workspace.clone())
                    .or_insert((info.created_at_ms, info.id.clone()));
                if info.created_at_ms > entry.0 {
                    *entry = (info.created_at_ms, info.id.clone());
                }
            }
        }
        let keep: HashSet<String> = newest_unused.into_values().map(|(_, id)| id).collect();
        let mut restored = 0;
        for thread in candidates {
            let info = thread.info();
            if info.turns == 0 && !keep.contains(&info.id) {
                for file in [
                    dir.join(format!("{}.meta.json", info.id)),
                    dir.join(format!("{}.jsonl", info.id)),
                ] {
                    if let Err(error) = std::fs::remove_file(&file) {
                        tracing::warn!("cannot remove {}: {error}", file.display());
                    }
                }
                continue;
            }
            self.adopt(&thread);
            lock(&self.threads).insert(thread.id().to_string(), thread);
            restored += 1;
        }
        restored
    }

    pub async fn create(&self, options: AgentOptions) -> Result<Arc<AgentThread>> {
        let id = format!("agent-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let thread = AgentThread::spawn(id.clone(), options, self.log_dir.as_deref()).await?;
        self.adopt(&thread);
        lock(&self.threads).insert(id, Arc::clone(&thread));
        Ok(thread)
    }

    pub fn get(&self, id: &str) -> Option<Arc<AgentThread>> {
        lock(&self.threads).get(id).cloned()
    }

    pub fn list(&self) -> Vec<AgentInfo> {
        lock(&self.threads).values().map(|t| t.info()).collect()
    }

    pub fn remove(&self, id: &str) -> Option<Arc<AgentThread>> {
        lock(&self.threads).remove(id)
    }

    pub fn all(&self) -> Vec<Arc<AgentThread>> {
        lock(&self.threads).values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_agent_binary_blames_the_server_path_and_suggests_a_restart() {
        let missing = || std::io::Error::from(std::io::ErrorKind::NotFound);
        let message = format!("{:#}", spawn_error("claude", missing()));
        assert!(message.contains("server's PATH"), "{message}");
        assert!(message.contains("icarus server restart"), "{message}");
        let message = format!("{:#}", spawn_error("/opt/claude", missing()));
        assert!(!message.contains("icarus server restart"), "{message}");
    }

    #[test]
    fn claude_command_uses_the_unmodified_binary_in_stream_json_mode() {
        let options = AgentOptions {
            provider: "claude".into(),
            cwd: PathBuf::from("/tmp"),
            workspace: None,
            title: None,
            model: Some("opus".into()),
            bypass_permissions: true,
            resume: Some("abc".into()),
            command: None,
            env: BTreeMap::new(),
            account: None,
            remote_control: false,
            permission_mode: None,
        };
        let command = provider_command(&options).expect("command");
        assert_eq!(command[0], "claude");
        for flag in [
            "-p",
            "--dangerously-skip-permissions",
            "--include-partial-messages",
        ] {
            assert!(command.contains(&flag.to_string()), "{flag} missing");
        }
        assert!(command.windows(2).any(|w| w == ["--resume", "abc"]));
        assert!(command.windows(2).any(|w| w == ["--model", "opus"]));
        assert!(!command.contains(&REMOTE_CONTROL_FLAG.to_string()));

        let remote = AgentOptions {
            remote_control: true,
            ..options.clone()
        };
        let command = provider_command(&remote).expect("command");
        assert_eq!(
            command.last().map(String::as_str),
            Some(REMOTE_CONTROL_FLAG)
        );
        let custom = AgentOptions {
            command: Some(vec!["my-claude".into()]),
            ..remote
        };
        assert_eq!(
            provider_command(&custom).expect("command"),
            [
                "my-claude",
                "--resume",
                "abc",
                "--model",
                "opus",
                REMOTE_CONTROL_FLAG
            ]
        );
    }

    #[test]
    fn a_picked_permission_mode_starts_claude_in_it_and_bypass_stays_reachable() {
        let options = AgentOptions {
            provider: "claude".into(),
            cwd: PathBuf::from("/tmp"),
            workspace: None,
            title: None,
            model: None,
            bypass_permissions: true,
            resume: None,
            command: None,
            env: BTreeMap::new(),
            account: None,
            remote_control: false,
            permission_mode: Some("plan".into()),
        };
        let command = provider_command(&options).expect("command");
        assert!(
            command
                .windows(2)
                .any(|w| w == ["--permission-mode", "plan"])
        );
        assert!(!command.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(command.contains(&"--allow-dangerously-skip-permissions".to_string()));
        let ask = AgentOptions {
            permission_mode: None,
            bypass_permissions: false,
            ..options
        };
        assert_eq!(default_mode(&ask), "default");
        assert!(
            !provider_command(&ask)
                .expect("command")
                .iter()
                .any(|arg| arg == "--permission-mode" || arg == "--dangerously-skip-permissions")
        );
    }

    #[test]
    fn claude_initialize_answers_become_commands_models_and_a_mode() {
        let capabilities = claude_capabilities(&json!({
            "commands": [
                { "name": "compact", "description": "Summarize", "argumentHint": "<focus>" },
                { "name": "__internal", "description": "", "argumentHint": "" },
                { "name": "review", "description": "Review a PR", "argumentHint": "" },
            ],
            "models": [{ "value": "default", "displayName": "Default (recommended)" }, { "value": "opus", "displayName": "Opus" }],
            "current_permission_mode": "acceptEdits",
        }));
        let names: Vec<&str> = capabilities["commands"]
            .as_array()
            .expect("commands")
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert_eq!(names, ["compact", "review"]);
        assert_eq!(capabilities["commands"][0]["hint"], "<focus>");
        assert!(capabilities["commands"][1]["hint"].is_null());
        assert_eq!(
            capabilities["models"][1],
            json!({ "id": "opus", "name": "Opus" })
        );
        assert_eq!(capabilities["permission_mode"], "acceptEdits");

        let codex = codex_models(&json!({ "data": [
            { "model": "gpt-a", "displayName": "A", "isDefault": true },
            { "model": "gpt-hidden", "hidden": true },
        ] }));
        assert_eq!(codex.len(), 2);
        assert_eq!(codex[0]["resolves"], "gpt-a");
    }

    #[test]
    fn remote_control_url_comes_from_the_newest_matching_session_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        let write = |name: &str, value: Value| {
            std::fs::write(sessions.join(name), value.to_string()).expect("write");
        };
        write(
            "1.json",
            json!({ "sessionId": "s1", "bridgeSessionId": "session_old", "updatedAt": 1 }),
        );
        write(
            "2.json",
            json!({ "sessionId": "s1", "bridgeSessionId": "session_new", "updatedAt": 2 }),
        );
        write(
            "3.json",
            json!({ "sessionId": "s2", "bridgeSessionId": "session_other", "updatedAt": 9 }),
        );
        write(
            "4.json",
            json!({ "sessionId": "s3", "bridgeSessionId": "../evil", "updatedAt": 9 }),
        );
        write(
            "2.abc.key",
            json!({ "sessionId": "s1", "bridgeSessionId": "session_key", "updatedAt": 99 }),
        );
        assert_eq!(
            remote_control_url(dir.path(), "s1").as_deref(),
            Some("https://claude.ai/code/session_new")
        );
        assert_eq!(remote_control_url(dir.path(), "s3"), None);
        assert_eq!(remote_control_url(dir.path(), "nope"), None);
    }

    #[test]
    fn normalizes_tool_calls_results_text_and_rate_limits() {
        let tool = normalize(
            &json!({ "type": "assistant", "message": { "id": "m1", "content": [
            { "type": "tool_use", "id": "t1", "name": "TodoWrite", "input": { "todos": [{ "content": "x" }] } }
        ] }, "parent_tool_use_id": null }),
        );
        let kinds: Vec<&str> = tool.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds, ["message", "tool_call", "plan"]);

        let result = normalize(&json!({ "type": "user", "message": { "content": [
            { "type": "tool_result", "tool_use_id": "t1", "content": "ok" }
        ] }, "tool_use_result": { "type": "create" } }));
        assert_eq!(result[0].0, "tool_result");
        assert_eq!(result[0].1["detail"]["type"], "create");

        let delta = normalize(&json!({ "type": "stream_event", "event": {
            "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "hi" } } }));
        assert_eq!(delta[0].1["text"], "hi");

        let limits = normalize(
            &json!({ "type": "rate_limit_event", "rate_limit_info": { "status": "allowed" } }),
        );
        assert_eq!(limits[0].0, "rate_limit");
        assert!(normalize(&json!({ "type": "system", "subtype": "hook_started" })).is_empty());
    }

    #[test]
    fn an_untitled_agent_is_named_by_its_first_prompt() {
        let options = |title: Option<&str>| AgentOptions {
            provider: "claude".into(),
            cwd: PathBuf::from("/tmp"),
            workspace: None,
            title: title.map(str::to_owned),
            model: None,
            bypass_permissions: true,
            resume: None,
            command: None,
            env: BTreeMap::new(),
            account: None,
            remote_control: false,
            permission_mode: None,
        };
        let prompt = |seq, text: &str| AgentEvent {
            seq,
            at_ms: 1,
            kind: "user_message".into(),
            data: json!({ "text": text }),
        };
        let fresh = AgentThread::new("a".into(), options(Some("Agent")), 0, None, Vec::new());
        assert_eq!(fresh.info().title, None, "the UI names it by its harness");

        let asked = AgentThread::new(
            "b".into(),
            options(Some("Agent")),
            0,
            None,
            vec![
                prompt(0, "/clear"),
                prompt(
                    1,
                    "<context kind=\"file\">\nx\n</context>\nFix the login bug\nmore",
                ),
            ],
        );
        assert_eq!(asked.info().title.as_deref(), Some("Fix the login bug"));
        asked.record("user_message", json!({ "text": "Something else" }));
        assert_eq!(asked.info().title.as_deref(), Some("Fix the login bug"));

        let live = AgentThread::new("c".into(), options(None), 0, None, Vec::new());
        live.record("user_message", json!({ "text": "Write the docs" }));
        assert_eq!(live.info().title.as_deref(), Some("Write the docs"));

        let chosen = AgentThread::new(
            "d".into(),
            options(Some("Review #7")),
            0,
            None,
            vec![prompt(0, "Look at the PR")],
        );
        assert_eq!(chosen.info().title.as_deref(), Some("Review #7"));
    }

    struct SlowTurnEnd;

    impl TurnHook for SlowTurnEnd {
        fn turn_started(&self, _agent: &AgentInfo, _prompt: &str) {}
        fn turn_ended(&self, _agent: &AgentInfo) {
            std::thread::sleep(Duration::from_millis(800));
        }
    }

    #[tokio::test]
    async fn a_finished_turn_shows_done_before_its_end_snapshot_is_taken() {
        let manager = AgentManager::new(None).with_turn_hook(Arc::new(SlowTurnEnd));
        let mut signals = manager.take_signals().expect("signals");
        let script = r#"while read l; do case "$l" in *'"user"'*) echo '{"type":"result","subtype":"success","is_error":false,"result":"ok"}';; esac; done"#;
        let agent = manager
            .create(AgentOptions {
                provider: "claude".into(),
                cwd: std::env::temp_dir(),
                workspace: None,
                title: None,
                model: None,
                bypass_permissions: true,
                resume: None,
                command: Some(vec!["sh".into(), "-c".into(), script.into()]),
                env: BTreeMap::new(),
                account: None,
                remote_control: false,
                permission_mode: None,
            })
            .await
            .expect("agent");
        agent.send("hi", &[]).await.expect("send");
        let started = std::time::Instant::now();
        let idle = loop {
            let (_, event) = tokio::time::timeout(Duration::from_millis(500), signals.recv())
                .await
                .expect("a state push before the snapshot ends")
                .expect("signals open");
            if event.kind == STATE_SIGNAL
                && event.data["state"] == "idle"
                && event.data["turns"] == 1
            {
                break event;
            }
        };
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(idle.data["id"], agent.id());
        assert!(
            !agent.settled(),
            "agent.wait still waits for the end snapshot"
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !agent.settled() {
            assert!(
                std::time::Instant::now() < deadline,
                "the turn never settled"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(agent.events_since(0).iter().any(|e| e.kind == "turn_end"));
    }

    #[test]
    fn permission_requests_and_background_tasks_are_normalized() {
        let ask = json!({ "type": "control_request", "request_id": "r1", "request": {
            "subtype": "can_use_tool", "tool_name": "AskUserQuestion", "tool_use_id": "t1",
            "input": { "questions": [] }, "permission_suggestions": [] } });
        let [(kind, data)] = normalize(&ask).try_into().expect("one event");
        assert_eq!(kind, "permission_request");
        assert_eq!(data["request_id"], "r1");
        assert_eq!(data["question"], true);
        assert_eq!(data["reason"], "question");
        let cancel = json!({ "type": "control_cancel_request", "request_id": "r1" });
        assert_eq!(normalize(&cancel)[0].0, "permission_resolved");

        let mut state = State {
            state: AgentState::Idle,
            events: Vec::new(),
            provider_session: None,
            model: None,
            turns: 1,
            prompt_title: None,
            last_activity_ms: 0,
            rate_limits: None,
            rate_limits_at_ms: None,
            killed: false,
            parking: false,
            remote_control: false,
            mode: None,
            chosen_model: None,
            capabilities: json!({}),
            permissions: Vec::new(),
            background: BTreeSet::new(),
            live: Default::default(),
            finishing: false,
            revision: 0,
        };
        let frame = |subtype: &str, extra: Value| {
            let mut frame = json!({ "type": "system", "subtype": subtype, "task_id": "b1" });
            for (key, value) in extra.as_object().into_iter().flatten() {
                frame[key] = value.clone();
            }
            frame
        };
        state.track_background(&frame("task_started", json!({ "is_backgrounded": false })));
        assert_eq!(
            state.background.len(),
            0,
            "foreground tasks end with their turn"
        );
        state.track_background(&frame("task_started", json!({ "task_type": "monitor" })));
        assert_eq!(state.background.len(), 1);
        state.track_background(&frame(
            "task_updated",
            json!({ "patch": { "status": "completed" } }),
        ));
        assert_eq!(state.background.len(), 0);
        state.track_background(&json!({ "subtype": "background_tasks_changed",
            "tasks": [{ "task_id": "a" }, { "task_id": "b" }] }));
        assert_eq!(state.background.len(), 2);
        state.track_background(&frame("task_notification", json!({})));
        assert_eq!(state.background.len(), 2);
        assert_eq!(normalize(&frame("task_started", json!({})))[0].0, "task");
    }

    #[test]
    fn a_plan_request_waits_with_reason_plan_and_approval_switches_the_mode() {
        let exit_plan = json!({ "type": "control_request", "request_id": "r2", "request": {
            "subtype": "can_use_tool", "tool_name": "ExitPlanMode", "tool_use_id": "t2",
            "input": { "plan": "# Plan\n- step" },
            "permission_suggestions": [{ "type": "addRules" }] } });
        let [(kind, data)] = normalize(&exit_plan).try_into().expect("one event");
        assert_eq!(kind, "permission_request");
        assert_eq!(data["reason"], "plan");
        assert_eq!(data["plan"], "# Plan\n- step");
        assert_eq!(data["question"], false);

        let agent = AgentThread::new(
            "p".into(),
            AgentOptions {
                provider: "claude".into(),
                cwd: PathBuf::from("/tmp"),
                workspace: None,
                title: None,
                model: None,
                bypass_permissions: false,
                resume: None,
                command: None,
                env: BTreeMap::new(),
                account: None,
                remote_control: false,
                permission_mode: Some("plan".into()),
            },
            0,
            None,
            Vec::new(),
        );
        assert_eq!(agent.info().summary()["waiting_reason"], Value::Null);
        agent.record(&kind, data.clone());
        let info = agent.info();
        assert_eq!(info.state, AgentState::Waiting);
        assert_eq!(info.summary()["waiting_reason"], "plan");

        let (response, mode) =
            permission_response(&data, "allow_accept_edits", None).expect("response");
        assert_eq!(mode, Some("acceptEdits"));
        assert_eq!(response["behavior"], "allow");
        assert_eq!(response["updatedInput"], data["input"]);
        assert_eq!(
            response["updatedPermissions"],
            json!([{ "type": "setMode", "mode": "acceptEdits", "destination": "session" }])
        );
        let (response, mode) = permission_response(&data, "allow", None).expect("response");
        assert_eq!(mode, Some("default"));
        assert_eq!(response["updatedPermissions"][0]["mode"], "default");
        let (response, mode) = permission_response(&data, "deny", None).expect("response");
        assert_eq!(mode, None);
        assert!(
            response["message"]
                .as_str()
                .unwrap_or_default()
                .contains("keep planning")
        );

        agent.record("mode_changed", json!({ "permission_mode": "acceptEdits" }));
        assert_eq!(agent.current_mode(), "acceptEdits");

        let (_, data) = normalize(&json!({ "type": "control_request", "request_id": "r3",
            "request": { "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "t3",
                "input": {}, "permission_suggestions": [{ "type": "addRules" }] } }))
        .remove(0);
        assert_eq!(data["reason"], "permission");
        let (response, mode) = permission_response(&data, "allow_always", None).expect("response");
        assert_eq!(mode, None);
        assert_eq!(response["updatedPermissions"], data["suggestions"]);
        assert!(permission_response(&data, "allow_accept_edits", None).is_err());
    }

    #[test]
    fn spawned_subagents_reach_the_summary_and_push_state() {
        let agent = AgentThread::new(
            "s".into(),
            AgentOptions {
                provider: "claude".into(),
                cwd: PathBuf::from("/tmp"),
                workspace: None,
                title: None,
                model: None,
                bypass_permissions: true,
                resume: None,
                command: None,
                env: BTreeMap::new(),
                account: None,
                remote_control: false,
                permission_mode: None,
            },
            0,
            None,
            Vec::new(),
        );
        let (sender, mut signals) = mpsc::unbounded_channel();
        agent.set_signals(sender);
        let feed = |line: Value| {
            for (kind, data) in normalize(&line) {
                agent.record(&kind, data);
            }
        };
        agent.record("user_message", json!({ "text": "Review #4202" }));
        let tool_use = |id: &str, name: &str, input: Value, parent: Value| {
            json!({ "type": "assistant", "parent_tool_use_id": parent, "message": { "id": id,
                "content": [{ "type": "tool_use", "id": id, "name": name, "input": input }] } })
        };
        feed(tool_use(
            "toolu_agent",
            "Agent",
            json!({ "description": "Check the diff", "subagent_type": "general-purpose" }),
            Value::Null,
        ));
        feed(tool_use(
            "toolu_bash",
            "Bash",
            json!({ "command": "mise exec -- ./bin/flow" }),
            json!("toolu_agent"),
        ));

        let summary = agent.info().summary();
        assert_eq!(summary["activity"], "Agent: Check the diff");
        assert_eq!(summary["subagents"][0]["type"], "general-purpose");
        assert_eq!(summary["subagents"][0]["state"], "working");
        assert_eq!(
            summary["subagents"][0]["activity"],
            "Bash: mise exec -- ./bin/flow"
        );
        let mut pushed = Vec::new();
        while let Ok((_, event)) = signals.try_recv() {
            if event.kind == STATE_SIGNAL {
                pushed.push(event.data);
            }
        }
        assert_eq!(
            pushed.last().expect("a state push")["subagents"][0]["activity"],
            "Bash: mise exec -- ./bin/flow"
        );
        let revision = summary["revision"].as_u64().expect("revision");

        feed(json!({ "type": "user", "message": { "content": [
            { "type": "tool_result", "tool_use_id": "toolu_agent", "content": "done" }] } }));
        let summary = agent.info().summary();
        assert_eq!(summary["subagents"][0]["state"], "done");
        assert!(summary["revision"].as_u64().expect("revision") > revision);
        feed(json!({ "type": "result", "subtype": "success" }));
        assert_eq!(agent.info().summary()["subagents"], json!([]));
    }
}
