//! `icarus terminal attach`: mirrors a server terminal in this one, dtach-style, so the shell outlives the viewer.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use icarus_client::{Client, Event};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt as _;
use tokio::sync::mpsc;

const CLIENT_NAME: &str = "icarus-attach";
/// Long enough to ride out a server restart or upgrade handoff.
const RECONNECT_FOR: Duration = Duration::from_secs(60);

pub struct CreateSpec {
    pub cwd: PathBuf,
    pub workspace: Option<String>,
    pub label: Option<String>,
    pub command: Vec<String>,
}

/// Restores the terminal's original mode when dropped, so a panic or error never leaves it raw.
struct RawMode {
    original: Option<rustix::termios::Termios>,
}

impl RawMode {
    fn enable() -> Self {
        let stdin = std::io::stdin();
        if !rustix::termios::isatty(&stdin) {
            return Self { original: None };
        }
        let Ok(original) = rustix::termios::tcgetattr(&stdin) else {
            return Self { original: None };
        };
        let mut raw = original.clone();
        raw.make_raw();
        if rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &raw).is_err()
        {
            return Self { original: None };
        }
        Self {
            original: Some(original),
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(original) = &self.original
            && let Err(error) = rustix::termios::tcsetattr(
                std::io::stdin(),
                rustix::termios::OptionalActions::Now,
                original,
            )
        {
            eprintln!("icarus: could not restore the terminal: {error}");
        }
    }
}

fn local_size() -> Option<(u16, u16)> {
    let size = rustix::termios::tcgetwinsize(std::io::stdout()).ok()?;
    (size.ws_col > 0 && size.ws_row > 0).then_some((size.ws_col, size.ws_row))
}

fn emit(bytes: &[u8]) -> Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(bytes)?;
    out.flush()?;
    Ok(())
}

/// What the attach loop should do after handling something.
enum Next {
    Continue,
    Exit(i32),
}

struct Session {
    id: String,
    /// The sequence number of the next output chunk not yet shown.
    next_seq: Option<u64>,
}

impl Session {
    async fn attach(&mut self, client: &Client) -> Result<Next> {
        let response = client
            .request(
                "terminal.attach",
                json!({ "id": self.id, "since_seq": self.next_seq }),
            )
            .await
            .with_context(|| format!("terminal {} is not on the server", self.id))?;
        if let Some(snapshot) = response["snapshot"].as_str() {
            emit(snapshot.as_bytes())?;
        }
        for chunk in response["chunks"].as_array().into_iter().flatten() {
            self.output(chunk)?;
        }
        if let Some(next) = response["next_seq"].as_u64() {
            self.next_seq = Some(self.next_seq.map_or(next, |seen| seen.max(next)));
        }
        if let Some((columns, rows)) = local_size() {
            client
                .request(
                    "terminal.resize",
                    json!({ "id": self.id, "columns": columns, "rows": rows }),
                )
                .await?;
        }
        let info = client
            .request("terminal.info", json!({ "id": self.id }))
            .await?;
        if info["lost"].as_bool() == Some(true) {
            eprintln!(
                "\r\nicarus: terminal {} was lost when its server stopped\r",
                self.id
            );
            return Ok(Next::Exit(1));
        }
        if info["running"].as_bool() == Some(false) {
            return Ok(Next::Exit(exit_code(&info["exit_code"])));
        }
        Ok(Next::Continue)
    }

    fn output(&mut self, chunk: &Value) -> Result<()> {
        let Some(seq) = chunk["seq"].as_u64() else {
            return Ok(());
        };
        if self.next_seq.is_some_and(|next| seq < next) {
            return Ok(());
        }
        let data = B64
            .decode(chunk["data"].as_str().unwrap_or_default())
            .context("bad output chunk")?;
        emit(&data)?;
        self.next_seq = Some(seq + 1);
        Ok(())
    }

    async fn event(&mut self, client: &Client, event: Event) -> Result<Next> {
        if event.payload["id"].as_str() != Some(self.id.as_str()) {
            return Ok(Next::Continue);
        }
        match event.topic.as_str() {
            "terminal.output" => {
                let seq = event.payload["seq"].as_u64();
                if let (Some(seq), Some(next)) = (seq, self.next_seq)
                    && seq > next
                {
                    // A gap means this subscription missed output; resubscribing replays it.
                    return self.attach(client).await;
                }
                self.output(&event.payload)?;
                Ok(Next::Continue)
            }
            "terminal.lagged" => self.attach(client).await,
            "terminal.exit" => Ok(Next::Exit(exit_code(&event.payload["code"]))),
            _ => Ok(Next::Continue),
        }
    }
}

fn exit_code(code: &Value) -> i32 {
    code.as_u64()
        .and_then(|code| i32::try_from(code).ok())
        .unwrap_or(0)
}

async fn reconnect(cli: &crate::Cli) -> Result<(Arc<Client>, mpsc::UnboundedReceiver<Event>)> {
    let deadline = Instant::now() + RECONNECT_FOR;
    let mut delay = Duration::from_millis(50);
    loop {
        match crate::open(cli, CLIENT_NAME).await {
            Ok(connection) => return Ok(connection),
            Err(error) if Instant::now() >= deadline => {
                return Err(error.context("lost the Icarus server"));
            }
            Err(_) => {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(1));
            }
        }
    }
}

/// Attaches to (or creates, then attaches to) a server terminal; returns the code to exit with.
pub async fn run(cli: &crate::Cli, id: Option<String>, create: Option<CreateSpec>) -> Result<i32> {
    let (mut client, mut events) = crate::open(cli, CLIENT_NAME).await?;
    let id = match (id, create) {
        (Some(id), _) => id,
        (None, Some(spec)) => {
            let (columns, rows) = local_size().unzip();
            let created = client
                .request(
                    "terminal.create",
                    json!({
                        "cwd": spec.cwd,
                        "workspace": spec.workspace,
                        "label": spec.label,
                        "command": if spec.command.is_empty() { Value::Null } else { json!(spec.command) },
                        "columns": columns,
                        "rows": rows,
                    }),
                )
                .await?;
            created["id"]
                .as_str()
                .context("the server returned no terminal id")?
                .to_owned()
        }
        (None, None) => bail!("give a terminal id, or --create"),
    };

    let raw = RawMode::enable();
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buffer = vec![0u8; 4096];
        while let Ok(read) = stdin.read(&mut buffer).await {
            if read == 0 || input_tx.send(buffer[..read].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut resized =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;

    let mut session = Session { id, next_seq: None };
    let mut pending: Vec<u8> = Vec::new();
    let code = 'outer: {
        if let Next::Exit(code) = session.attach(&client).await? {
            break 'outer code;
        }
        loop {
            tokio::select! {
                event = events.recv() => {
                    let next = match event {
                        Some(event) => session.event(&client, event).await?,
                        None => {
                            (client, events) = reconnect(cli).await?;
                            let next = session.attach(&client).await?;
                            if !pending.is_empty() {
                                let data = B64.encode(std::mem::take(&mut pending));
                                client
                                    .request("terminal.write", json!({ "id": session.id, "data": data }))
                                    .await?;
                            }
                            next
                        }
                    };
                    if let Next::Exit(code) = next {
                        break 'outer code;
                    }
                }
                Some(bytes) = input_rx.recv() => {
                    let data = B64.encode(&bytes);
                    if client
                        .request("terminal.write", json!({ "id": session.id, "data": data }))
                        .await
                        .is_err()
                    {
                        // The connection is going away; the reconnect above sends this once it's back.
                        pending.extend_from_slice(&bytes);
                    }
                }
                _ = resized.recv() => {
                    if let Some((columns, rows)) = local_size() {
                        client
                            .request(
                                "terminal.resize",
                                json!({ "id": session.id, "columns": columns, "rows": rows }),
                            )
                            .await
                            .ok();
                    }
                }
            }
        }
    };
    drop(raw);
    Ok(code)
}
