//! Agent threads: one long-lived agent process per thread, owned by the server.
//!
//! Claude Code runs as the user's own, unmodified `claude` binary in stream-JSON mode (plan D19);
//! Codex runs as the user's own `codex app-server` (see `codex`). Output is normalized into
//! `AgentEvent`s, kept in memory and on disk, so clients can attach, detach and resume by
//! sequence number while the agent keeps working.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, mpsc};

use crate::codex;

/// Bounds Codex handshakes and requests so a stuck app-server cannot hang a client.
const CODEX_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Starting,
    Idle,
    Working,
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
}

/// Agent events the server reacts to (notifications), forwarded with the agent's id.
pub type AgentSignal = (String, AgentEvent);

const SIGNAL_KINDS: &[&str] = &["turn_end", "exit", "notice", "tool_call"];

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
    last_activity_ms: u64,
    rate_limits: Option<Value>,
    rate_limits_at_ms: Option<u64>,
    killed: bool,
    /// Set by `sleep`, so the process ending is not mistaken for a crash.
    parking: bool,
    remote_control: bool,
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
            ];
            if options.bypass_permissions {
                command.push("--dangerously-skip-permissions".into());
            }
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

/// Maps one stream-JSON line from the agent to zero or more normalized events.
pub fn normalize(line: &Value) -> Vec<(String, Value)> {
    let kind = line["type"].as_str().unwrap_or_default();
    match kind {
        "system" => match line["subtype"].as_str().unwrap_or_default() {
            "init" => vec![(
                "session".into(),
                json!({ "session_id": line["session_id"], "model": line["model"], "cwd": line["cwd"] }),
            )],
            "status" => vec![("status".into(), json!({ "status": line["status"] }))],
            "api_retry" | "permission_denied" => vec![("notice".into(), line.clone())],
            _ => Vec::new(),
        },
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
            last_activity_ms: created_at_ms,
            rate_limits: None,
            rate_limits_at_ms: None,
            killed: false,
            parking: false,
            remote_control: options.remote_control,
        };
        for event in events {
            match event.kind.as_str() {
                "remote_control" => {
                    state.remote_control = event.data["enabled"].as_bool().unwrap_or_default();
                }
                "session" => {
                    state.provider_session = event.data["session_id"].as_str().map(str::to_string);
                    state.model = event.data["model"].as_str().map(str::to_string);
                }
                "user_message" => state.turns += 1,
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
        let _ = tokio::task::spawn_blocking(move || match prompt {
            Some(prompt) => hook.turn_started(&info, &prompt),
            None => hook.turn_ended(&info),
        })
        .await;
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
        options.remote_control = lock(&self.state).remote_control;
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
            .with_context(|| format!("start agent `{}`", command[0]))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("agent stdout")?;
        let stderr = child.stderr.take().context("agent stderr")?;
        *self.stdin.lock().await = stdin;
        *self.child.lock().await = Some(child);
        {
            let mut state = lock(&self.state);
            state.parking = false;
            if matches!(state.state, AgentState::Exited | AgentState::Sleeping) {
                state.state = AgentState::Starting;
            }
        }

        let reading = Arc::clone(self);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let events = reading.decode_line(&line).await;
                // The end snapshot lands before `turn_end` is published, so waiters see the turn.
                if events.iter().any(|(kind, _)| kind == "turn_end") {
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
        if self.is_codex()
            && let Err(error) = self.open_codex_thread(options.resume.as_deref()).await
        {
            self.stdin.lock().await.take();
            if let Some(child) = self.child.lock().await.as_mut() {
                let _ = child.start_kill();
            }
            return Err(error);
        }
        Ok(())
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

    async fn open_codex_thread(&self, resume: Option<&str>) -> Result<()> {
        self.codex_request("initialize", codex::initialize_params())
            .await?;
        self.write_line(&json!({ "method": "initialized" })).await?;
        let opened = match resume {
            Some(thread_id) => match self
                .codex_request(
                    "thread/resume",
                    codex::resume_params(&self.options, thread_id),
                )
                .await
            {
                Err(error) if codex::is_no_rollout(&format!("{error}"), thread_id) => {
                    self.record(
                        "notice",
                        json!({ "message": format!("codex thread {thread_id} was never saved; started a new one") }),
                    );
                    self.codex_request("thread/start", codex::start_params(&self.options))
                        .await?
                }
                other => other?,
            },
            None => {
                self.codex_request("thread/start", codex::start_params(&self.options))
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
            return normalize(&value);
        }
        match self.codex.incoming(value) {
            codex::Incoming::Notification { method, params } => codex::normalize(&method, &params),
            codex::Incoming::Request { id, method } => {
                let bypass = self.options.bypass_permissions;
                let reply = match codex::reply_to_request(&method, bypass) {
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
        match kind {
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
            }
            "turn_end" => state.state = AgentState::Idle,
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
            }
            _ => {}
        }
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
        event
    }

    async fn finish(&self) {
        self.codex.reset();
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
            title: self.options.title.clone(),
            workspace: self.options.workspace.clone(),
            cwd: self.options.cwd.clone(),
            state: state.state,
            provider_session: state.provider_session.clone(),
            model: state.model.clone().or_else(|| self.options.model.clone()),
            turns: state.turns,
            created_at_ms: self.created_at_ms,
            last_activity_ms: state.last_activity_ms,
            next_seq: state.events.len() as u64,
            rate_limits: state.rate_limits.clone(),
            rate_limits_at_ms: state.rate_limits_at_ms,
            account: self.options.account.clone(),
            remote_control: state.remote_control,
            remote_control_url: None,
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
        let (request, reply) = self.codex.request(
            "turn/start",
            codex::turn_params(&thread_id, text, images, self.options.bypass_permissions),
        );
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
            ["my-claude", "--resume", "abc", REMOTE_CONTROL_FLAG]
        );
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
}
