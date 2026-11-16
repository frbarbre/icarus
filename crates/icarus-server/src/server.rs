use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    pub(crate) linear: crate::linear::LinearService,
    pub(crate) updates: crate::updates::Updates,
    pub(crate) automation: crate::automation::Automation,
    pub(crate) ports: crate::ports::Ports,
    pub(crate) history: Arc<crate::history::HistoryIndex>,
    /// Connected clients, for requests the server forwards to editors (e.g. open a file).
    clients: std::sync::Mutex<Vec<(String, String, Outbound)>>,
    shutdown: Notify,
    /// Set by `server.shutdown { keep_terminals }`: park terminals for the next server.
    keep_terminals: AtomicBool,
    /// The previous server's network listeners, when terminals were handed over.
    inherited_listen: Option<(Vec<String>, bool)>,
    pairing: crate::remote::Pairing,
    listening: std::sync::Mutex<Vec<SocketAddr>>,
    allow_public: AtomicBool,
    tls_fingerprint: std::sync::Mutex<Option<String>>,
    /// Live connections of paired clients, closed when the client is revoked.
    paired_connections: std::sync::Mutex<Vec<(String, Arc<Notify>)>>,
}

type Outbound = mpsc::UnboundedSender<ServerMessage>;

#[derive(Clone, Copy)]
enum Origin {
    Local,
    Network(SocketAddr),
}

const LOCAL_ONLY_METHODS: &[&str] = &["server.pair", "server.clients", "server.revoke"];

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
        let handed = {
            let paths = paths.clone();
            tokio::task::spawn_blocking(move || crate::handoff::take(&paths)).await?
        };
        let handed = handed.unwrap_or_else(|error| {
            tracing::warn!("could not take over the previous server's terminals: {error:#}");
            None
        });
        let (token, inherited_listen, parked) = match handed {
            Some((mut parcel, fds)) => {
                tracing::info!(
                    "took over {} terminal(s) from the previous server",
                    parcel.terminals.len()
                );
                let token = parcel.token.take();
                let listen = (std::mem::take(&mut parcel.listen), parcel.allow_public);
                (token, Some(listen), parcel.into_terminals(fds))
            }
            None => (None, None, Vec::new()),
        };
        let terminals = TerminalManager::restore(paths.terminals(), parked);
        let token = token.unwrap_or_else(|| {
            format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            )
        });
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
        let orphaned = store.fail_unfinished_note_replies(
            "the server restarted before the reply session answered",
        )?;
        for agent in orphaned.iter().flatten().filter_map(|id| agents.get(id)) {
            let _ = agent.kill().await;
            agents.remove(agent.id());
        }
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
            linear: crate::linear::LinearService::from_env(),
            updates: crate::updates::Updates::from_env(),
            automation: crate::automation::Automation::default(),
            ports: crate::ports::Ports::default(),
            history: Arc::default(),
            paths,
            token,
            started_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or_default(),
            terminals,
            clients: std::sync::Mutex::new(Vec::new()),
            shutdown: Notify::new(),
            keep_terminals: AtomicBool::new(false),
            inherited_listen,
            pairing: crate::remote::Pairing::default(),
            listening: std::sync::Mutex::new(Vec::new()),
            allow_public: AtomicBool::new(false),
            tls_fingerprint: std::sync::Mutex::new(None),
            paired_connections: std::sync::Mutex::new(Vec::new()),
        });
        Ok((server, listener))
    }

    /// Accepts TLS connections from paired clients on each address; returns the bound addresses.
    pub async fn listen_tcp(
        self: &Arc<Self>,
        addrs: &[SocketAddr],
        allow_public: bool,
    ) -> Result<Vec<SocketAddr>> {
        for addr in addrs {
            crate::remote::check_listen(*addr, allow_public)?;
        }
        if addrs.is_empty() {
            return Ok(Vec::new());
        }
        self.allow_public.store(allow_public, Ordering::Relaxed);
        let tls = crate::remote::load_or_create_tls(&self.paths)?;
        *self
            .tls_fingerprint
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(tls.fingerprint.clone());
        let mut bound = Vec::new();
        for addr in addrs {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("listen on {addr}"))?;
            bound.push(listener.local_addr()?);
            let server = Arc::clone(self);
            let acceptor = tls.acceptor.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, peer)) = listener.accept().await else {
                        // Running out of file descriptors fails every accept; don't spin on it.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    };
                    let server = Arc::clone(&server);
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        let handshake =
                            tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream))
                                .await;
                        let Ok(Ok(stream)) = handshake else {
                            tracing::debug!("TLS handshake with {peer} failed");
                            return;
                        };
                        let (reader, writer) = tokio::io::split(stream);
                        if let Err(error) = server
                            .serve_connection(reader, writer, Origin::Network(peer))
                            .await
                        {
                            tracing::debug!("connection from {peer} ended: {error:#}");
                        }
                    });
                }
            });
        }
        let lines: String = bound.iter().map(|a| format!("{a}\n")).collect();
        std::fs::write(self.paths.listen_addrs(), lines)?;
        self.listening
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend(&bound);
        Ok(bound)
    }

    pub async fn run(self: Arc<Self>, listener: UnixListener) -> Result<()> {
        crate::notifications::spawn(Arc::clone(&self));
        crate::automation::spawn(Arc::clone(&self));
        crate::ports::spawn(Arc::clone(&self));
        crate::linear::spawn(Arc::clone(&self));
        crate::updates::spawn(Arc::clone(&self));
        self.resume_setups();
        // launchd and systemd stop (and restart) the service with SIGTERM; nothing may die.
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let keep_terminals = loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let server = Arc::clone(&self);
                    tokio::spawn(async move {
                        let (reader, writer) = stream.into_split();
                        if let Err(error) = server.serve_connection(reader, writer, Origin::Local).await {
                            tracing::debug!("connection ended: {error:#}");
                        }
                    });
                }
                () = self.shutdown.notified() => break self.keep_terminals.load(Ordering::Relaxed),
                _ = terminate.recv() => break true,
            }
        };
        // Agents sleep rather than die, so the next server resumes them.
        for agent in self.agents.all() {
            let _ = agent.sleep().await;
        }
        if keep_terminals {
            // The socket stays bound until the holder has everything, so no new server starts early.
            match self.park_terminals().await {
                Ok(count) => tracing::info!("parked {count} terminal(s) for the next server"),
                Err(error) => {
                    tracing::warn!("could not keep terminals running: {error:#}");
                    self.terminals.kill_all();
                }
            }
        } else {
            self.terminals.kill_all();
            self.terminals.forget_all();
        }
        let _ = std::fs::remove_file(self.paths.socket());
        let _ = std::fs::remove_file(self.paths.pid());
        let _ = std::fs::remove_file(self.paths.listen_addrs());
        Ok(())
    }

    /// Checks a hello, returning the paired client id (if any) and a token to hand out (when pairing).
    fn authenticate(
        &self,
        origin: Origin,
        token: &str,
        pair_code: Option<&str>,
        label: &str,
    ) -> Result<(Option<String>, Option<String>), RpcError> {
        let peer = match origin {
            Origin::Local if token == self.token => return Ok((None, None)),
            Origin::Local => return Err(rpc("unauthorized", "bad token")),
            Origin::Network(peer) => peer.ip(),
        };
        if !self.pairing.allowed(peer) {
            return Err(rpc(
                "rate_limited",
                "too many failed attempts from this address; wait a minute",
            ));
        }
        let failed = |message: &str| {
            self.pairing.record_failure(peer);
            Err(rpc("unauthorized", message))
        };
        if let Some(code) = pair_code {
            if !self.pairing.redeem(code) {
                return failed("pairing code is wrong, expired or already used");
            }
            let token = crate::remote::new_token();
            let client = self
                .store
                .add_paired_client(label, &crate::remote::sha256_hex(token.as_bytes()))
                .map_err(|e| rpc("internal", e))?;
            tracing::info!("paired client {} ({label}) from {peer}", client.id);
            return Ok((Some(client.id), Some(token)));
        }
        match self
            .store
            .touch_paired_client(&crate::remote::sha256_hex(token.as_bytes()))
        {
            Ok(Some(client)) => Ok((Some(client.id), None)),
            Ok(None) => failed("unknown or revoked client token; pair again"),
            Err(error) => Err(rpc("internal", error)),
        }
    }

    async fn serve_connection<R, W>(
        self: Arc<Self>,
        mut reader: R,
        mut writer: W,
        origin: Origin,
    ) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let closed = Arc::new(Notify::new());
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
        let mut remote = false;
        let mut client_id: Option<String> = None;
        let mut subscriptions: HashMap<String, JoinHandle<()>> = HashMap::new();
        let result = async {
            loop {
                let n = tokio::select! {
                    read = reader.read(&mut chunk) => read?,
                    () = closed.notified() => return Ok(()),
                };
                if n == 0 {
                    return Ok(());
                }
                buffer.extend_from_slice(&chunk[..n]);
                for message in decode_frames::<ClientMessage>(&mut buffer)? {
                    match message {
                        ClientMessage::Hello { .. } if authenticated => {}
                        ClientMessage::Hello {
                            version,
                            token,
                            client_name,
                            pair_code,
                            client_label,
                        } => {
                            let label = client_label.as_deref().unwrap_or(&client_name);
                            let verdict = if version == PROTOCOL_VERSION {
                                self.authenticate(origin, &token, pair_code.as_deref(), label)
                            } else {
                                Err(rpc(
                                    "unauthorized",
                                    format!("protocol version {version} is not {PROTOCOL_VERSION}"),
                                ))
                            };
                            let (paired_client_id, issued_token) = match verdict {
                                Ok(granted) => granted,
                                Err(error) => {
                                    let _ = outbound.send(ServerMessage::Response {
                                        id: 0,
                                        result: None,
                                        error: Some(error),
                                    });
                                    bail!("rejected client {client_name}");
                                }
                            };
                            authenticated = true;
                            remote = matches!(origin, Origin::Network(_));
                            if let Some(paired) = &paired_client_id {
                                self.paired_connections
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .push((paired.clone(), Arc::clone(&closed)));
                            }
                            let id = uuid::Uuid::new_v4().simple().to_string();
                            self.clients
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .push((id.clone(), client_name.clone(), outbound.clone()));
                            client_id = Some(id.clone());
                            let _ = outbound.send(ServerMessage::Welcome {
                                version: PROTOCOL_VERSION,
                                server_version: crate::updates::VERSION.to_string(),
                                client_id: id,
                                token: issued_token,
                                paired_client_id,
                            });
                        }
                        ClientMessage::Request { id, method, params } => {
                            let reply = if !authenticated {
                                Err(rpc("unauthorized", "send hello first"))
                            } else if remote && LOCAL_ONLY_METHODS.contains(&method.as_str()) {
                                Err(rpc(
                                    "forbidden",
                                    format!("{method} only works on the server's own machine"),
                                ))
                            } else {
                                self.dispatch(&method, params, &outbound, &mut subscriptions)
                                    .await
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
        self.paired_connections
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(_, signal)| !Arc::ptr_eq(signal, &closed));
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
                "version": crate::updates::VERSION,
                "protocol": PROTOCOL_VERSION,
                "pid": std::process::id(),
                "home": self.paths.home,
                "started_at_ms": self.started_at_ms,
                "terminals": self.terminals.list().len(),
                "agents": self.agents.list().len(),
                "listen": self.listen_addresses(),
                "tls_fingerprint": self.tls_fingerprint(),
            })),
            "server.pair" => {
                #[derive(Deserialize)]
                struct P {
                    ttl_secs: Option<u64>,
                }
                let p: P = params(params_value)?;
                let (code, ttl) = self
                    .pairing
                    .issue(p.ttl_secs.map(|s| Duration::from_secs(s.max(1))));
                Ok(json!({
                    "code": crate::remote::format_code(&code),
                    "expires_in_secs": ttl.as_secs(),
                    "listen": self.listen_addresses(),
                    "tls_fingerprint": self.tls_fingerprint(),
                }))
            }
            "server.clients" => {
                let live = self
                    .paired_connections
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .iter()
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                let clients = self
                    .store
                    .paired_clients()
                    .map_err(|e| rpc("internal", e))?;
                Ok(json!(
                    clients
                        .into_iter()
                        .map(|c| {
                            let connections = live.iter().filter(|id| **id == c.id).count();
                            let mut value = json!(c);
                            value["connections"] = json!(connections);
                            value
                        })
                        .collect::<Vec<_>>()
                ))
            }
            "server.revoke" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                }
                let p: P = params(params_value)?;
                if !self
                    .store
                    .remove_paired_client(&p.id)
                    .map_err(|e| rpc("internal", e))?
                {
                    return Err(rpc("not_found", format!("paired client {}", p.id)));
                }
                let mut closed = 0;
                self.paired_connections
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .retain(|(id, signal)| {
                        if *id == p.id {
                            signal.notify_one();
                            closed += 1;
                        }
                        *id != p.id
                    });
                Ok(json!({ "revoked": true, "closed_connections": closed }))
            }
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
                    /// Account id or label; `system` for the provider's own login.
                    account: Option<String>,
                    /// Claude Code's Remote Control; default `agents.remote_control`.
                    remote_control: Option<bool>,
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
                let mut options = AgentOptions {
                    provider,
                    cwd,
                    workspace: p.workspace,
                    title: p.title,
                    model: p.model,
                    bypass_permissions: p.bypass_permissions,
                    resume: p.resume,
                    command,
                    env: p.env,
                    account: None,
                    remote_control: p
                        .remote_control
                        .unwrap_or_else(|| next_action::remote_control_default(&settings)),
                };
                self.apply_account(&mut options, p.account.as_deref())?;
                let thread = self
                    .agents
                    .create(options)
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
            "agent.remote_control" => {
                #[derive(Deserialize)]
                struct P {
                    id: Option<String>,
                    /// Every Claude Code agent of this workspace instead of one agent.
                    workspace: Option<String>,
                    enabled: bool,
                }
                let p: P = params(params_value)?;
                let threads = match (&p.id, &p.workspace) {
                    (Some(id), None) => {
                        vec![self.agents.get(id).ok_or_else(|| rpc("not_found", id))?]
                    }
                    (None, Some(workspace)) => self
                        .agents
                        .all()
                        .into_iter()
                        .filter(|a| {
                            a.options().provider == "claude"
                                && a.options().workspace.as_deref() == Some(workspace.as_str())
                        })
                        .collect(),
                    _ => return Err(rpc("invalid_params", "give an agent id or a workspace")),
                };
                let mut agents = Vec::new();
                for thread in threads {
                    let restarted = thread
                        .set_remote_control(p.enabled)
                        .await
                        .map_err(|e| rpc("remote_control_failed", format!("{e:#}")))?;
                    let mut info = to_value(thread.info())?;
                    info["restarted"] = json!(restarted);
                    agents.push(info);
                }
                Ok(json!({ "enabled": p.enabled, "agents": agents }))
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
            "client.dispatch" => {
                #[derive(Deserialize)]
                struct P {
                    action: String,
                    #[serde(default)]
                    args: Option<Value>,
                    /// A file the app writes the outcome to, so callers can wait for it.
                    result: Option<PathBuf>,
                }
                let p: P = params(params_value)?;
                if p.result.as_ref().is_some_and(|path| !path.is_absolute()) {
                    return Err(rpc("invalid_params", "result must be an absolute path"));
                }
                let delivered = self.send_to_apps(
                    "client.dispatch",
                    json!({ "action": p.action, "args": p.args, "result": p.result }),
                );
                if delivered == 0 {
                    return Err(rpc(
                        "no_client",
                        "no Icarus window is connected; open the app first",
                    ));
                }
                Ok(json!({ "delivered": delivered }))
            }
            "server.shutdown" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(default)]
                    keep_terminals: bool,
                }
                let p: P = params(params_value)?;
                self.keep_terminals
                    .store(p.keep_terminals, Ordering::Relaxed);
                self.shutdown.notify_one();
                Ok(json!({ "stopping": true, "keep_terminals": p.keep_terminals }))
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
            "terminal.set_label" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    label: Option<String>,
                }
                let p: P = params(params_value)?;
                let session = self
                    .terminals
                    .get(&p.id)
                    .ok_or_else(|| rpc("not_found", &p.id))?;
                session.set_label(p.label.filter(|label| !label.trim().is_empty()));
                self.terminals.save_registry();
                to_value(session.info())
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
            "agent.history" | "agent.resume" => self.history_dispatch(method, params_value).await,
            other if other.starts_with("update.") => self.updates_dispatch(other).await,
            other if other.starts_with("notification.") => {
                self.notifications_dispatch(other, params_value).await
            }
            other if other.starts_with("account.") => {
                self.accounts_dispatch(other, params_value).await
            }
            other if other.starts_with("pr.") => self.github_dispatch(other, params_value).await,
            other if other.starts_with("review.") => {
                self.review_dispatch(other, params_value).await
            }
            other if other.starts_with("issue.") => self.linear_dispatch(other, params_value).await,
            other if other.starts_with("ports.") => self.ports_dispatch(other, params_value).await,
            other if other.starts_with("attachment.") || other.starts_with("attach.") => {
                self.attachments_dispatch(other, params_value).await
            }
            other if other.starts_with("rules.") => self.rules_dispatch(other, params_value).await,
            other if other.starts_with("note.") => self.notes_dispatch(other, params_value).await,
            other if other.starts_with("git.") => self.git_dispatch(other, params_value).await,
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
            "settings.import_orca_state" => self.orca_state_dispatch(params_value),
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
    /// The network listeners the previous server had, when this one took over its terminals.
    pub fn inherited_listen(&self) -> Option<&(Vec<String>, bool)> {
        self.inherited_listen.as_ref()
    }

    async fn park_terminals(&self) -> Result<usize> {
        if self.terminals.list().is_empty() {
            return Ok(0);
        }
        let paths = self.paths.clone();
        tokio::task::spawn_blocking(move || crate::handoff::spawn_holder(&paths)).await??;
        let (terminals, fds): (Vec<_>, Vec<_>) = self.terminals.park_all().into_iter().unzip();
        let parcel = crate::handoff::Parcel {
            version: crate::handoff::VERSION,
            token: Some(self.token.clone()),
            listen: self.listen_addresses(),
            allow_public: self.allow_public.load(Ordering::Relaxed),
            terminals,
        };
        let fds: Vec<_> = fds.into_iter().flatten().collect();
        let count = parcel.terminals.len();
        let paths = self.paths.clone();
        tokio::task::spawn_blocking(move || crate::handoff::put(&paths, &parcel, &fds)).await??;
        Ok(count)
    }

    fn listen_addresses(&self) -> Vec<String> {
        self.listening
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn tls_fingerprint(&self) -> Option<String> {
        self.tls_fingerprint
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

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
