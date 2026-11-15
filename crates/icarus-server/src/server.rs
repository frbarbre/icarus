use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use icarus_protocol::{
    ClientMessage, PROTOCOL_VERSION, RpcError, ServerMessage, decode_frames, encode_frame,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use crate::agent::{AgentManager, AgentOptions, AgentState};
use crate::next_action;
use crate::paths::ServerPaths;
use crate::state::Store;
use crate::terminal::{SpawnOptions, TerminalEvent, TerminalManager};

pub struct Server {
    pub(crate) paths: ServerPaths,
    token: String,
    started_at_ms: u64,
    pub(crate) terminals: TerminalManager,
    pub(crate) agents: AgentManager,
    pub(crate) store: Store,
    pub(crate) workspace_root: PathBuf,
    pub(crate) github: crate::github::GitHubService,
    pub(crate) automation: crate::automation::Automation,
    pub(crate) ports: crate::ports::Ports,
    /// Connected clients, for requests the server forwards to editors (e.g. open a file).
    clients: std::sync::Mutex<Vec<(String, String, Outbound)>>,
    shutdown: Notify,
}

type Outbound = mpsc::UnboundedSender<ServerMessage>;

pub(crate) fn rpc(code: &str, message: impl std::fmt::Display) -> RpcError {
    RpcError::new(code, message.to_string())
}

pub(crate) fn params<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, RpcError> {
    let value = if value.is_null() { json!({}) } else { value };
    serde_json::from_value(value).map_err(|e| rpc("invalid_params", e))
}

impl Server {
    /// Binds the socket and writes the token, refusing to start if another server is alive.
    pub async fn bind(paths: ServerPaths) -> Result<(Arc<Self>, UnixListener)> {
        std::fs::create_dir_all(&paths.home)
            .with_context(|| format!("create {}", paths.home.display()))?;
        std::fs::set_permissions(&paths.home, std::fs::Permissions::from_mode(0o700))?;
        let socket = paths.socket();
        if socket.exists() {
            if UnixStream::connect(&socket).await.is_ok() {
                bail!("a server is already running at {}", socket.display());
            }
            std::fs::remove_file(&socket)?;
        }
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        std::fs::write(paths.token(), &token)?;
        std::fs::set_permissions(paths.token(), std::fs::Permissions::from_mode(0o600))?;
        std::fs::write(paths.pid(), std::process::id().to_string())?;
        let agents = AgentManager::new(Some(paths.home.join("threads")))
            .with_turn_hook(Arc::new(crate::turns::TurnRecorder::default()));
        let restored = agents.restore_all();
        if restored > 0 {
            tracing::info!("restored {restored} sleeping agent session(s)");
        }
        let store = Store::open(&paths.state_db())?;
        let workspace_root = match std::env::var_os("ICARUS_WORKSPACE_ROOT") {
            Some(root) => PathBuf::from(root),
            None => dirs::home_dir()
                .context("no home directory")?
                .join("icarus")
                .join("workspaces"),
        };
        let server = Arc::new(Self {
            agents,
            store,
            workspace_root,
            github: crate::github::GitHubService::from_env(),
            automation: crate::automation::Automation::default(),
            ports: crate::ports::Ports::default(),
            paths,
            token,
            started_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or_default(),
            terminals: TerminalManager::default(),
            clients: std::sync::Mutex::new(Vec::new()),
            shutdown: Notify::new(),
        });
        Ok((server, listener))
    }

    pub async fn run(self: Arc<Self>, listener: UnixListener) -> Result<()> {
        crate::automation::spawn(Arc::clone(&self));
        crate::ports::spawn(Arc::clone(&self));
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let server = Arc::clone(&self);
                    tokio::spawn(async move {
                        if let Err(error) = server.serve_connection(stream).await {
                            tracing::debug!("connection ended: {error:#}");
                        }
                    });
                }
                () = self.shutdown.notified() => break,
            }
        }
        self.terminals.kill_all();
        // Agents sleep rather than die, so the next server resumes them.
        for agent in self.agents.all() {
            let _ = agent.sleep().await;
        }
        let _ = std::fs::remove_file(self.paths.socket());
        let _ = std::fs::remove_file(self.paths.pid());
        Ok(())
    }

    async fn serve_connection(self: Arc<Self>, stream: UnixStream) -> Result<()> {
        let (mut reader, mut writer) = stream.into_split();
        let (outbound, mut outbox) = mpsc::unbounded_channel::<ServerMessage>();
        let writer_task = tokio::spawn(async move {
            while let Some(message) = outbox.recv().await {
                let Ok(frame) = encode_frame(&message) else {
                    continue;
                };
                if writer.write_all(&frame).await.is_err() {
                    break;
                }
            }
        });

        let mut buffer = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        let mut authenticated = false;
        let mut client_id: Option<String> = None;
        let mut subscriptions: HashMap<String, JoinHandle<()>> = HashMap::new();
        let result = async {
            loop {
                let n = reader.read(&mut chunk).await?;
                if n == 0 {
                    return Ok(());
                }
                buffer.extend_from_slice(&chunk[..n]);
                for message in decode_frames::<ClientMessage>(&mut buffer)? {
                    match message {
                        ClientMessage::Hello {
                            version,
                            token,
                            client_name,
                        } => {
                            if version != PROTOCOL_VERSION || token != self.token {
                                let _ = outbound.send(ServerMessage::Response {
                                    id: 0,
                                    result: None,
                                    error: Some(rpc(
                                        "unauthorized",
                                        "bad token or protocol version",
                                    )),
                                });
                                bail!("rejected client {client_name}");
                            }
                            authenticated = true;
                            let id = uuid::Uuid::new_v4().simple().to_string();
                            self.clients
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .push((id.clone(), client_name.clone(), outbound.clone()));
                            client_id = Some(id.clone());
                            let _ = outbound.send(ServerMessage::Welcome {
                                version: PROTOCOL_VERSION,
                                server_version: env!("CARGO_PKG_VERSION").to_string(),
                                client_id: id,
                            });
                        }
                        ClientMessage::Request { id, method, params } => {
                            let reply = if authenticated {
                                self.dispatch(&method, params, &outbound, &mut subscriptions)
                                    .await
                            } else {
                                Err(rpc("unauthorized", "send hello first"))
                            };
                            let (result, error) = match reply {
                                Ok(value) => (Some(value), None),
                                Err(error) => (None, Some(error)),
                            };
                            let _ = outbound.send(ServerMessage::Response { id, result, error });
                        }
                    }
                }
            }
        }
        .await;
        if let Some(id) = client_id {
            self.clients
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|(client, _, _)| *client != id);
        }
        for (_, task) in subscriptions {
            task.abort();
        }
        drop(outbound);
        let _ = writer_task.await;
        result
    }

    async fn dispatch(
        self: &Arc<Self>,
        method: &str,
        params_value: Value,
        outbound: &Outbound,
        subscriptions: &mut HashMap<String, JoinHandle<()>>,
    ) -> Result<Value, RpcError> {
        match method {
            "server.info" => Ok(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "protocol": PROTOCOL_VERSION,
                "pid": std::process::id(),
                "home": self.paths.home,
                "started_at_ms": self.started_at_ms,
                "terminals": self.terminals.list().len(),
                "agents": self.agents.list().len(),
            })),
            "agent.create" => {
                #[derive(Deserialize)]
                struct P {
                    provider: Option<String>,
                    cwd: Option<PathBuf>,
                    workspace: Option<String>,
                    title: Option<String>,
                    model: Option<String>,
                    #[serde(default = "default_true")]
                    bypass_permissions: bool,
                    resume: Option<String>,
                    command: Option<Vec<String>>,
                    #[serde(default)]
                    env: BTreeMap<String, String>,
                    /// Sent as the first message once the agent is up.
                    prompt: Option<String>,
                }
                let p: P = params(params_value)?;
                let cwd = match p.cwd {
                    Some(cwd) => cwd,
                    None => dirs::home_dir().ok_or_else(|| rpc("invalid_params", "no cwd"))?,
                };
                if !cwd.is_dir() {
                    return Err(rpc(
                        "invalid_params",
                        format!("{} is not a directory", cwd.display()),
                    ));
                }
                let settings = self.resolved_settings(None);
                let provider = p
                    .provider
                    .unwrap_or_else(|| next_action::default_provider(&settings));
                let command = p
                    .command
                    .or_else(|| next_action::configured_command(&settings, &provider));
                let thread = self
                    .agents
                    .create(AgentOptions {
                        provider,
                        cwd,
                        workspace: p.workspace,
                        title: p.title,
                        model: p.model,
                        bypass_permissions: p.bypass_permissions,
                        resume: p.resume,
                        command,
                        env: p.env,
                    })
                    .await
                    .map_err(|e| rpc("spawn_failed", format!("{e:#}")))?;
                if let Some(prompt) = p.prompt {
                    thread
                        .send(&prompt, &[])
                        .await
                        .map_err(|e| rpc("send_failed", format!("{e:#}")))?;
                }
                to_value(thread.info())
            }
            "agent.list" => to_value(self.agents.list()),
            "agent.info" => to_value(self.agent(params_value)?.info()),
            "agent.send" => {
                #[derive(Deserialize)]
                struct Image {
                    media_type: String,
                    data: String,
                }
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    text: String,
                    #[serde(default)]
                    images: Vec<Image>,
                }
                let p: P = params(params_value)?;
                let thread = self
                    .agents
                    .get(&p.id)
                    .ok_or_else(|| rpc("not_found", &p.id))?;
                let images: Vec<(String, String)> = p
                    .images
                    .into_iter()
                    .map(|i| (i.media_type, i.data))
                    .collect();
                thread
                    .send(&p.text, &images)
                    .await
                    .map_err(|e| rpc("send_failed", format!("{e:#}")))?;
                to_value(thread.info())
            }
            "agent.sleep" => {
                let thread = self.agent(params_value)?;
                thread
                    .sleep()
                    .await
                    .map_err(|e| rpc("sleep_failed", format!("{e:#}")))?;
                Ok(json!({ "sleeping": true }))
            }
            "agent.interrupt" => {
                let thread = self.agent(params_value)?;
                thread
                    .interrupt()
                    .await
                    .map_err(|e| rpc("interrupt_failed", format!("{e:#}")))?;
                Ok(json!({ "interrupted": true }))
            }
            "agent.kill" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    #[serde(default)]
                    remove: bool,
                }
                let p: P = params(params_value)?;
                let thread = self
                    .agents
                    .get(&p.id)
                    .ok_or_else(|| rpc("not_found", &p.id))?;
                thread
                    .kill()
                    .await
                    .map_err(|e| rpc("kill_failed", format!("{e:#}")))?;
                if p.remove {
                    self.agents.remove(&p.id);
                }
                Ok(json!({ "killed": true }))
            }
            "agent.read" => {
                let thread = self.agent(params_value)?;
                Ok(json!({ "text": thread.transcript(), "info": thread.info() }))
            }
            "agent.events" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    #[serde(default)]
                    since: u64,
                }
                let p: P = params(params_value)?;
                let thread = self
                    .agents
                    .get(&p.id)
                    .ok_or_else(|| rpc("not_found", &p.id))?;
                to_value(thread.events_since(p.since))
            }
            "agent.attach" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    #[serde(default)]
                    since: u64,
                }
                let p: P = params(params_value)?;
                let thread = self
                    .agents
                    .get(&p.id)
                    .ok_or_else(|| rpc("not_found", &p.id))?;
                let (backlog, mut receiver) = thread.attach(p.since);
                let next = backlog.last().map(|e| e.seq + 1).unwrap_or(p.since);
                let id = p.id.clone();
                let out = outbound.clone();
                let task = tokio::spawn(async move {
                    loop {
                        match receiver.recv().await {
                            Ok(event) if event.seq >= next => {
                                let payload = json!({ "id": id, "event": event });
                                if out
                                    .send(ServerMessage::Event {
                                        topic: "agent.event".into(),
                                        payload,
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Ok(_) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                let _ = out.send(ServerMessage::Event {
                                    topic: "agent.lagged".into(),
                                    payload: json!({ "id": id }),
                                });
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                });
                if let Some(previous) = subscriptions.insert(p.id, task) {
                    previous.abort();
                }
                Ok(json!({ "events": backlog, "next_seq": next, "info": thread.info() }))
            }
            "agent.detach" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                }
                let p: P = params(params_value)?;
                if let Some(task) = subscriptions.remove(&p.id) {
                    task.abort();
                }
                Ok(json!({ "detached": true }))
            }
            "agent.wait" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    /// Wait for the current turn to finish.
                    #[serde(default = "default_true")]
                    idle: bool,
                    timeout_ms: Option<u64>,
                }
                let p: P = params(params_value)?;
                let thread = self
                    .agents
                    .get(&p.id)
                    .ok_or_else(|| rpc("not_found", &p.id))?;
                let deadline =
                    Instant::now() + Duration::from_millis(p.timeout_ms.unwrap_or(600_000));
                loop {
                    let state = thread.state();
                    if state == AgentState::Exited || (p.idle && state == AgentState::Idle) {
                        return to_value(thread.info());
                    }
                    if Instant::now() >= deadline {
                        return Err(rpc("timeout", "agent still working"));
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
            "client.list" => {
                let clients = self.clients.lock().unwrap_or_else(|p| p.into_inner());
                Ok(json!(
                    clients
                        .iter()
                        .map(|(id, name, _)| json!({ "id": id, "name": name }))
                        .collect::<Vec<_>>()
                ))
            }
            "client.open_file" => {
                #[derive(Deserialize)]
                struct P {
                    path: PathBuf,
                    line: Option<u32>,
                    column: Option<u32>,
                    #[serde(default = "default_true")]
                    focus: bool,
                }
                let p: P = params(params_value)?;
                // Editors subscribe by name; CLIs and bridges never open files.
                let payload =
                    json!({ "path": p.path, "line": p.line, "column": p.column, "focus": p.focus });
                let clients = self.clients.lock().unwrap_or_else(|p| p.into_inner());
                let editors: Vec<&(String, String, Outbound)> = clients
                    .iter()
                    .filter(|(_, name, _)| name == "icarus-app")
                    .collect();
                if editors.is_empty() {
                    return Err(rpc(
                        "no_client",
                        "no Icarus window is connected; open the app first",
                    ));
                }
                let mut delivered = 0;
                for (_, _, outbound) in editors {
                    if outbound
                        .send(ServerMessage::Event {
                            topic: "client.open_file".into(),
                            payload: payload.clone(),
                        })
                        .is_ok()
                    {
                        delivered += 1;
                    }
                }
                Ok(json!({ "delivered": delivered }))
            }
            "client.capture" => {
                #[derive(Deserialize)]
                struct P {
                    dir: PathBuf,
                }
                let p: P = params(params_value)?;
                if !p.dir.is_absolute() {
                    return Err(rpc("invalid_params", "dir must be an absolute path"));
                }
                let delivered = self.send_to_apps("client.capture", json!({ "dir": p.dir }));
                if delivered == 0 {
                    return Err(rpc(
                        "no_client",
                        "no Icarus window is connected; open the app first",
                    ));
                }
                Ok(json!({ "delivered": delivered }))
            }
            "server.shutdown" => {
                self.shutdown.notify_one();
                Ok(json!({ "stopping": true }))
            }
            "terminal.create" => {
                #[derive(Deserialize)]
                struct P {
                    cwd: Option<PathBuf>,
                    command: Option<Vec<String>>,
                    #[serde(default)]
                    env: BTreeMap<String, String>,
                    columns: Option<u16>,
                    rows: Option<u16>,
                    label: Option<String>,
                    workspace: Option<String>,
                }
                let p: P = params(params_value)?;
                let cwd = match p.cwd {
                    Some(cwd) => cwd,
                    None => dirs::home_dir().ok_or_else(|| rpc("invalid_params", "no cwd"))?,
                };
                if !cwd.is_dir() {
                    return Err(rpc(
                        "invalid_params",
                        format!("{} is not a directory", cwd.display()),
                    ));
                }
                let session = self
                    .terminals
                    .create(SpawnOptions {
                        cwd,
                        command: p.command,
                        env: p.env,
                        columns: p.columns.unwrap_or(120),
                        rows: p.rows.unwrap_or(32),
                        label: p.label,
                        workspace: p.workspace,
                    })
                    .map_err(|e| rpc("spawn_failed", format!("{e:#}")))?;
                to_value(session.info())
            }
            "terminal.list" => to_value(self.terminals.list()),
            "terminal.info" => {
                let session = self.terminal(params_value)?;
                to_value(session.info())
            }
            "terminal.attach" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    since_seq: Option<u64>,
                }
                let p: P = params(params_value)?;
                let session = self
                    .terminals
                    .get(&p.id)
                    .ok_or_else(|| rpc("not_found", &p.id))?;
                let (attachment, mut receiver) = session.attach(p.since_seq);
                let id = p.id.clone();
                let out = outbound.clone();
                let task = tokio::spawn(async move {
                    loop {
                        let event = match receiver.recv().await {
                            Ok(event) => event,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                let _ = out.send(ServerMessage::Event {
                                    topic: "terminal.lagged".into(),
                                    payload: json!({ "id": id }),
                                });
                                break;
                            }
                            Err(_) => break,
                        };
                        let (topic, payload) = match event {
                            TerminalEvent::Output { seq, data } => (
                                "terminal.output",
                                json!({ "id": id, "seq": seq, "data": B64.encode(data.as_slice()) }),
                            ),
                            TerminalEvent::Title(title) => {
                                ("terminal.title", json!({ "id": id, "title": title }))
                            }
                            TerminalEvent::Bell => ("terminal.bell", json!({ "id": id })),
                            TerminalEvent::Exit { code } => {
                                ("terminal.exit", json!({ "id": id, "code": code }))
                            }
                        };
                        if out
                            .send(ServerMessage::Event {
                                topic: topic.into(),
                                payload,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                if let Some(previous) = subscriptions.insert(p.id, task) {
                    previous.abort();
                }
                Ok(json!({
                    "snapshot": attachment.snapshot,
                    "chunks": attachment
                        .chunks
                        .iter()
                        .map(|(seq, data)| json!({ "seq": seq, "data": B64.encode(data) }))
                        .collect::<Vec<_>>(),
                    "next_seq": attachment.next_seq,
                    "columns": attachment.columns,
                    "rows": attachment.rows,
                }))
            }
            "terminal.detach" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                }
                let p: P = params(params_value)?;
                if let Some(task) = subscriptions.remove(&p.id) {
                    task.abort();
                }
                Ok(json!({ "detached": true }))
            }
            "terminal.write" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    text: Option<String>,
                    data: Option<String>,
                }
                let p: P = params(params_value)?;
                let session = self
                    .terminals
                    .get(&p.id)
                    .ok_or_else(|| rpc("not_found", &p.id))?;
                let bytes = match (p.text, p.data) {
                    (Some(text), _) => text.into_bytes(),
                    (None, Some(data)) => B64.decode(data).map_err(|e| rpc("invalid_params", e))?,
                    (None, None) => return Err(rpc("invalid_params", "text or data required")),
                };
                session
                    .write(&bytes)
                    .map_err(|e| rpc("write_failed", format!("{e:#}")))?;
                Ok(json!({ "written": bytes.len() }))
            }
            "terminal.resize" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    columns: u16,
                    rows: u16,
                }
                let p: P = params(params_value)?;
                let session = self
                    .terminals
                    .get(&p.id)
                    .ok_or_else(|| rpc("not_found", &p.id))?;
                session
                    .resize(p.columns, p.rows)
                    .map_err(|e| rpc("resize_failed", format!("{e:#}")))?;
                to_value(session.info())
            }
            "terminal.read" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    #[serde(default)]
                    scrollback: bool,
                }
                let p: P = params(params_value)?;
                let session = self
                    .terminals
                    .get(&p.id)
                    .ok_or_else(|| rpc("not_found", &p.id))?;
                let text = if p.scrollback {
                    session.full_text()
                } else {
                    session.screen_text()
                };
                Ok(json!({ "text": text }))
            }
            "terminal.wait" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    contains: Option<String>,
                    #[serde(default)]
                    exit: bool,
                    timeout_ms: Option<u64>,
                }
                let p: P = params(params_value)?;
                let session = self
                    .terminals
                    .get(&p.id)
                    .ok_or_else(|| rpc("not_found", &p.id))?;
                let deadline =
                    Instant::now() + Duration::from_millis(p.timeout_ms.unwrap_or(30_000));
                loop {
                    let matched = match &p.contains {
                        Some(needle) => session.full_text().contains(needle.as_str()),
                        None => false,
                    };
                    if matched || (p.exit && !session.is_running()) {
                        return to_value(session.info());
                    }
                    if Instant::now() >= deadline {
                        return Err(rpc("timeout", "condition not met in time"));
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
            "terminal.kill" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    #[serde(default)]
                    remove: bool,
                }
                let p: P = params(params_value)?;
                let session = self
                    .terminals
                    .get(&p.id)
                    .ok_or_else(|| rpc("not_found", &p.id))?;
                if session.is_running() {
                    session
                        .kill()
                        .map_err(|e| rpc("kill_failed", format!("{e:#}")))?;
                }
                if p.remove {
                    self.terminals.remove(&p.id);
                }
                Ok(json!({ "killed": true }))
            }
            "workspace.next_action" | "workspace.run_next_action" => {
                self.next_action_dispatch(method, params_value).await
            }
            other if other.starts_with("pr.") => self.github_dispatch(other, params_value).await,
            other if other.starts_with("ports.") => self.ports_dispatch(other, params_value).await,
            other if other.starts_with("attachment.") || other.starts_with("attach.") => {
                self.attachments_dispatch(other, params_value).await
            }
            other if other.starts_with("rules.") => self.rules_dispatch(other, params_value).await,
            "workspace.changes"
            | "workspace.changes.explain"
            | "workspace.changes.filter"
            | "workspace.turns" => self.changes_dispatch(method, params_value).await,
            "workspace.layout" | "workspace.take_layout" | "workspace.pending_layouts" => {
                self.layouts_dispatch(method, params_value).await
            }
            other if other.starts_with("layout.") => {
                self.layouts_dispatch(other, params_value).await
            }
            other if other.starts_with("project.") || other.starts_with("workspace.") => {
                self.workspace_dispatch(other, params_value).await
            }
            other if other.starts_with("config.") || other.starts_with("settings.") => {
                self.settings_dispatch(other, params_value).await
            }
            other => Err(rpc("unknown_method", other)),
        }
    }

    fn terminal(
        &self,
        params_value: Value,
    ) -> Result<Arc<crate::terminal::TerminalSession>, RpcError> {
        #[derive(Deserialize)]
        struct P {
            id: String,
        }
        let p: P = params(params_value)?;
        self.terminals
            .get(&p.id)
            .ok_or_else(|| rpc("not_found", &p.id))
    }
}

fn default_true() -> bool {
    true
}

impl Server {
    pub(crate) fn client_count(&self) -> usize {
        self.clients.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// Sends an event to every connected client; returns how many received it.
    pub(crate) fn broadcast(&self, topic: &str, payload: Value) -> usize {
        let clients = self.clients.lock().unwrap_or_else(|p| p.into_inner());
        clients
            .iter()
            .filter(|(_, _, outbound)| {
                outbound
                    .send(ServerMessage::Event {
                        topic: topic.into(),
                        payload: payload.clone(),
                    })
                    .is_ok()
            })
            .count()
    }

    /// Sends an event to the connected Icarus windows only; returns how many received it.
    pub(crate) fn send_to_apps(&self, topic: &str, payload: Value) -> usize {
        let clients = self.clients.lock().unwrap_or_else(|p| p.into_inner());
        clients
            .iter()
            .filter(|(_, name, _)| name == "icarus-app")
            .filter(|(_, _, outbound)| {
                outbound
                    .send(ServerMessage::Event {
                        topic: topic.into(),
                        payload: payload.clone(),
                    })
                    .is_ok()
            })
            .count()
    }

    fn agent(&self, params_value: Value) -> Result<Arc<crate::agent::AgentThread>, RpcError> {
        #[derive(Deserialize)]
        struct P {
            id: String,
        }
        let p: P = params(params_value)?;
        self.agents
            .get(&p.id)
            .ok_or_else(|| rpc("not_found", &p.id))
    }
}

pub(crate) fn to_value<T: serde::Serialize>(value: T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| rpc("internal", e))
}
