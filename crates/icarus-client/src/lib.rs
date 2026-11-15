//! Async client for an Icarus server, shared by the CLI, tests and the GUI.

pub mod servers;
mod tls;

pub use tls::fingerprint;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use icarus_protocol::{
    ClientMessage, PROTOCOL_VERSION, RpcError, ServerMessage, decode_frames, encode_frame,
};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc, oneshot};

#[derive(Debug, Clone)]
pub struct Event {
    pub topic: String,
    pub payload: Value,
}

type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

pub struct Client {
    writer: Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    pending: Pending,
    next_id: AtomicU64,
    pub server_version: String,
    /// The per-client token a pairing code was exchanged for; save it, it isn't shown again.
    pub issued_token: Option<String>,
    pub paired_client_id: Option<String>,
    /// SHA-256 of the server's TLS certificate, for TCP connections.
    pub server_fingerprint: Option<String>,
}

/// How a TCP client proves who it is.
#[derive(Debug, Clone)]
pub enum Credentials {
    /// A token from an earlier pairing, with the certificate fingerprint pinned then.
    Token { token: String, fingerprint: String },
    /// A one-time code from `icarus server pair`. Without a fingerprint the certificate is trusted on first use.
    PairCode {
        code: String,
        fingerprint: Option<String>,
        label: Option<String>,
    },
}

struct Hello<'a> {
    token: &'a str,
    pair_code: Option<&'a str>,
    label: Option<&'a str>,
    server_fingerprint: Option<String>,
}

impl Hello<'_> {
    fn token(token: &str) -> Hello<'_> {
        Hello {
            token,
            pair_code: None,
            label: None,
            server_fingerprint: None,
        }
    }
}

/// Default state directory, matching the server's resolution.
pub fn default_home() -> Result<PathBuf> {
    match std::env::var_os("ICARUS_HOME") {
        Some(path) => Ok(PathBuf::from(path)),
        None => Ok(dirs::home_dir()
            .context("no home directory")?
            .join(".icarus")),
    }
}

impl Client {
    /// Connects to the server whose state lives in `home`, returning the client and its event stream.
    pub async fn connect(
        home: &Path,
        client_name: &str,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Event>)> {
        let socket = home.join("server.sock");
        let token = std::fs::read_to_string(home.join("server.token"))
            .with_context(|| format!("read token in {}", home.display()))?;
        let stream = UnixStream::connect(&socket)
            .await
            .with_context(|| format!("connect {}", socket.display()))?;
        let (reader, writer) = stream.into_split();
        Self::handshake(
            reader,
            Box::new(writer),
            Hello::token(token.trim()),
            client_name,
        )
        .await
    }

    /// Connects to a server on another machine by running `icarus-server proxy` there over SSH.
    /// SSH does the authentication; the remote proxy presents the remote server's token.
    pub async fn connect_ssh(
        destination: &str,
        remote_command: Option<&str>,
        client_name: &str,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Event>)> {
        let command = remote_command.unwrap_or("icarus-server proxy");
        let mut child = tokio::process::Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-T", destination, command])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("ssh {destination}"))?;
        let reader = child.stdout.take().context("ssh stdout")?;
        let writer = child.stdin.take().context("ssh stdin")?;
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        Self::handshake(reader, Box::new(writer), Hello::token(""), client_name).await
    }

    /// Runs a local command that speaks the server protocol on stdio (used by tests and custom transports).
    pub async fn connect_command(
        program: &str,
        args: &[&str],
        client_name: &str,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Event>)> {
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start {program}"))?;
        let reader = child.stdout.take().context("stdout")?;
        let writer = child.stdin.take().context("stdin")?;
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        Self::handshake(reader, Box::new(writer), Hello::token(""), client_name).await
    }

    /// Connects to a server's `--listen` address over TLS, pinning its certificate.
    pub async fn connect_tcp(
        addr: &str,
        credentials: &Credentials,
        client_name: &str,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Event>)> {
        let addr = addr.trim_start_matches("tcp://");
        let (expected, hello) = match credentials {
            Credentials::Token { token, fingerprint } => {
                (Some(fingerprint.as_str()), Hello::token(token))
            }
            Credentials::PairCode {
                code,
                fingerprint,
                label,
            } => (
                fingerprint.as_deref(),
                Hello {
                    token: "",
                    pair_code: Some(code),
                    label: label.as_deref(),
                    server_fingerprint: None,
                },
            ),
        };
        let stream = tokio::time::timeout(
            Duration::from_secs(15),
            tokio::net::TcpStream::connect(addr),
        )
        .await
        .map_err(|_| anyhow!("connect {addr} timed out"))?
        .with_context(|| format!("connect {addr}"))?;
        let (stream, fingerprint) = tls::connect(stream, expected).await?;
        let (reader, writer) = tokio::io::split(stream);
        let hello = Hello {
            server_fingerprint: Some(fingerprint),
            ..hello
        };
        Self::handshake(reader, Box::new(writer), hello, client_name).await
    }

    /// Connects to a saved server (see [`servers`]).
    pub async fn connect_saved(
        server: &servers::SavedServer,
        client_name: &str,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Event>)> {
        match server.target()? {
            servers::Target::Ssh(destination) => {
                Self::connect_ssh(&destination, server.command.as_deref(), client_name).await
            }
            servers::Target::Tcp(addr) => {
                let (Some(token), Some(fingerprint)) = (&server.token, &server.fingerprint) else {
                    bail!(
                        "server {} has no saved token; pair again with `icarus remote add`",
                        server.name
                    );
                };
                let credentials = Credentials::Token {
                    token: token.clone(),
                    fingerprint: fingerprint.clone(),
                };
                Self::connect_tcp(&addr, &credentials, client_name)
                    .await
                    .with_context(|| format!("connect to {} ({addr})", server.name))
            }
        }
    }

    async fn handshake<R>(
        mut reader: R,
        mut writer: Box<dyn AsyncWrite + Send + Unpin>,
        hello: Hello<'_>,
        client_name: &str,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Event>)>
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        writer
            .write_all(&encode_frame(&ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                token: hello.token.to_string(),
                client_name: client_name.to_string(),
                pair_code: hello.pair_code.map(str::to_string),
                client_label: hello.label.map(str::to_string),
            })?)
            .await?;

        let mut buffer = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        let (server_version, issued_token, paired_client_id) = loop {
            let n = reader.read(&mut chunk).await?;
            if n == 0 {
                bail!("server closed the connection during hello");
            }
            buffer.extend_from_slice(&chunk[..n]);
            let mut messages = decode_frames::<ServerMessage>(&mut buffer)?.into_iter();
            match messages.next() {
                Some(ServerMessage::Welcome {
                    server_version,
                    token,
                    paired_client_id,
                    ..
                }) => break (server_version, token, paired_client_id),
                Some(ServerMessage::Response {
                    error: Some(error), ..
                }) => {
                    bail!("server rejected hello: {error}")
                }
                Some(other) => bail!("unexpected first message: {other:?}"),
                None => continue,
            }
        };

        let pending: Pending = Arc::default();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let routing = Arc::clone(&pending);
        tokio::spawn(async move {
            loop {
                for message in decode_frames::<ServerMessage>(&mut buffer).unwrap_or_default() {
                    match message {
                        ServerMessage::Response { id, result, error } => {
                            let sender = routing.lock().ok().and_then(|mut p| p.remove(&id));
                            if let Some(sender) = sender {
                                let _ = sender.send(match error {
                                    Some(error) => Err(error),
                                    None => Ok(result.unwrap_or(Value::Null)),
                                });
                            }
                        }
                        ServerMessage::Event { topic, payload } => {
                            let _ = events_tx.send(Event { topic, payload });
                        }
                        ServerMessage::Welcome { .. } => {}
                    }
                }
                match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                }
            }
            if let Ok(mut pending) = routing.lock() {
                pending.clear();
            }
        });

        Ok((
            Arc::new(Self {
                writer: Mutex::new(writer),
                pending,
                next_id: AtomicU64::new(1),
                server_version,
                issued_token,
                paired_client_id,
                server_fingerprint: hello.server_fingerprint,
            }),
            events_rx,
        ))
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| anyhow!("client state poisoned"))?
            .insert(id, tx);
        let frame = encode_frame(&ClientMessage::Request {
            id,
            method: method.to_string(),
            params,
        })?;
        self.writer.lock().await.write_all(&frame).await?;
        match rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(anyhow!(ClientError::Rpc(error))),
            Err(_) => bail!("connection closed before {method} answered"),
        }
    }

    /// Like `request` but gives up after `timeout`.
    pub async fn request_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        tokio::time::timeout(timeout, self.request(method, params))
            .await
            .map_err(|_| anyhow!("{method} timed out"))?
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Rpc(RpcError),
}
