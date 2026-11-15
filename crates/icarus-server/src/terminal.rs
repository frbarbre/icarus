use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::emulator::Emulator;

/// Raw output kept in memory so a reconnecting client can resume without a full snapshot.
const RING_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum TerminalEvent {
    Output { seq: u64, data: Arc<Vec<u8>> },
    Title(Option<String>),
    Bell,
    Exit { code: Option<u32> },
}

#[derive(Debug, Clone)]
pub struct SpawnOptions {
    pub cwd: PathBuf,
    pub command: Option<Vec<String>>,
    pub env: BTreeMap<String, String>,
    pub columns: u16,
    pub rows: u16,
    pub label: Option<String>,
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TerminalInfo {
    pub id: String,
    pub label: Option<String>,
    pub workspace: Option<String>,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    pub title: Option<String>,
    pub columns: u16,
    pub rows: u16,
    pub pid: Option<u32>,
    pub running: bool,
    pub exit_code: Option<u32>,
    pub created_at_ms: u64,
    pub next_seq: u64,
}

/// What a client gets when it (re)attaches: either the missing output or a full screen snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct Attachment {
    pub snapshot: Option<String>,
    pub chunks: Vec<(u64, Vec<u8>)>,
    pub next_seq: u64,
    pub columns: u16,
    pub rows: u16,
}

struct State {
    emulator: Emulator,
    ring: VecDeque<(u64, Vec<u8>)>,
    ring_bytes: usize,
    next_seq: u64,
    exit_code: Option<u32>,
    running: bool,
}

pub struct TerminalSession {
    id: String,
    label: Option<String>,
    workspace: Option<String>,
    cwd: PathBuf,
    command: Vec<String>,
    created_at_ms: u64,
    pid: Option<u32>,
    state: Mutex<State>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    events: broadcast::Sender<TerminalEvent>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

pub fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string())
}

impl TerminalSession {
    pub fn spawn(id: String, options: SpawnOptions) -> Result<Arc<Self>> {
        let size = PtySize {
            rows: options.rows.max(1),
            cols: options.columns.max(2),
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|e| anyhow!("openpty: {e}"))?;
        let command = options
            .command
            .clone()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| vec![default_shell(), "-l".to_string()]);
        let mut builder = CommandBuilder::new(&command[0]);
        builder.args(&command[1..]);
        builder.cwd(&options.cwd);
        builder.env("TERM", "xterm-256color");
        builder.env("COLORTERM", "truecolor");
        builder.env("ICARUS_TERMINAL_ID", &id);
        if let Some(workspace) = &options.workspace {
            builder.env("ICARUS_WORKSPACE_ID", workspace);
        }
        for (key, value) in &options.env {
            builder.env(key, value);
        }
        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|e| anyhow!("spawn {}: {e}", command[0]))?;
        drop(pair.slave);
        let pid = child.process_id();
        let killer = child.clone_killer();
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow!("pty reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow!("pty writer: {e}"))?;
        let (events, _) = broadcast::channel(4096);

        let session = Arc::new(Self {
            id,
            label: options.label,
            workspace: options.workspace,
            cwd: options.cwd,
            command,
            created_at_ms: now_ms(),
            pid,
            state: Mutex::new(State {
                emulator: Emulator::new(size.cols, size.rows),
                ring: VecDeque::new(),
                ring_bytes: 0,
                next_seq: 0,
                exit_code: None,
                running: true,
            }),
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
            events,
        });

        let reading = Arc::clone(&session);
        std::thread::Builder::new()
            .name(format!("pty-{}", reading.id))
            .spawn(move || reading.read_loop(reader, child))
            .context("spawn pty reader thread")?;
        Ok(session)
    }

    fn read_loop(
        &self,
        mut reader: Box<dyn Read + Send>,
        mut child: Box<dyn portable_pty::Child + Send + Sync>,
    ) {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => self.ingest(&buffer[..n]),
            }
        }
        let code = child.wait().ok().map(|status| status.exit_code());
        {
            let mut state = lock(&self.state);
            state.running = false;
            state.exit_code = code;
        }
        let _ = self.events.send(TerminalEvent::Exit { code });
    }

    fn ingest(&self, bytes: &[u8]) {
        let data = Arc::new(bytes.to_vec());
        let effects;
        let seq;
        {
            let mut state = lock(&self.state);
            effects = state.emulator.feed(bytes);
            seq = state.next_seq;
            state.next_seq += 1;
            state.ring_bytes += bytes.len();
            state.ring.push_back((seq, bytes.to_vec()));
            while state.ring_bytes > RING_BYTES {
                match state.ring.pop_front() {
                    Some((_, dropped)) => state.ring_bytes -= dropped.len(),
                    None => break,
                }
            }
            // Sent under the lock so an attach can never observe a seq without its event.
            let _ = self.events.send(TerminalEvent::Output { seq, data });
        }
        if !effects.pty_replies.is_empty() {
            let _ = lock(&self.writer).write_all(&effects.pty_replies);
        }
        if let Some(title) = effects.title {
            let _ = self.events.send(TerminalEvent::Title(title));
        }
        if effects.bell {
            let _ = self.events.send(TerminalEvent::Bell);
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn info(&self) -> TerminalInfo {
        let state = lock(&self.state);
        let (columns, rows) = state.emulator.size();
        TerminalInfo {
            id: self.id.clone(),
            label: self.label.clone(),
            workspace: self.workspace.clone(),
            cwd: self.cwd.clone(),
            command: self.command.clone(),
            title: state.emulator.title().map(str::to_string),
            columns,
            rows,
            pid: self.pid,
            running: state.running,
            exit_code: state.exit_code,
            created_at_ms: self.created_at_ms,
            next_seq: state.next_seq,
        }
    }

    /// Returns what the client is missing since `since_seq` plus a live subscription, atomically.
    pub fn attach(
        &self,
        since_seq: Option<u64>,
    ) -> (Attachment, broadcast::Receiver<TerminalEvent>) {
        let state = lock(&self.state);
        let receiver = self.events.subscribe();
        let (columns, rows) = state.emulator.size();
        let oldest = state.ring.front().map(|(seq, _)| *seq);
        let resumable = match (since_seq, oldest) {
            (Some(since), Some(oldest)) => since >= oldest && since <= state.next_seq,
            (Some(since), None) => since == state.next_seq,
            (None, _) => false,
        };
        let attachment = if resumable {
            let since = since_seq.unwrap_or_default();
            Attachment {
                snapshot: None,
                chunks: state
                    .ring
                    .iter()
                    .filter(|(seq, _)| *seq >= since)
                    .cloned()
                    .collect(),
                next_seq: state.next_seq,
                columns,
                rows,
            }
        } else {
            Attachment {
                snapshot: Some(state.emulator.snapshot_vt()),
                chunks: Vec::new(),
                next_seq: state.next_seq,
                columns,
                rows,
            }
        };
        (attachment, receiver)
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = lock(&self.writer);
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    pub fn resize(&self, columns: u16, rows: u16) -> Result<()> {
        let size = PtySize {
            rows: rows.max(1),
            cols: columns.max(2),
            pixel_width: 0,
            pixel_height: 0,
        };
        lock(&self.master)
            .resize(size)
            .map_err(|e| anyhow!("resize: {e}"))?;
        lock(&self.state).emulator.resize(size.cols, size.rows);
        Ok(())
    }

    pub fn screen_text(&self) -> String {
        lock(&self.state).emulator.screen_text()
    }

    pub fn full_text(&self) -> String {
        lock(&self.state).emulator.full_text()
    }

    pub fn kill(&self) -> Result<()> {
        lock(&self.killer).kill().context("kill terminal process")
    }

    pub fn is_running(&self) -> bool {
        lock(&self.state).running
    }
}

#[derive(Default)]
pub struct TerminalManager {
    sessions: Mutex<BTreeMap<String, Arc<TerminalSession>>>,
}

impl TerminalManager {
    pub fn create(&self, options: SpawnOptions) -> Result<Arc<TerminalSession>> {
        let id = format!("term-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let session = TerminalSession::spawn(id.clone(), options)?;
        lock(&self.sessions).insert(id, Arc::clone(&session));
        Ok(session)
    }

    pub fn get(&self, id: &str) -> Option<Arc<TerminalSession>> {
        lock(&self.sessions).get(id).cloned()
    }

    pub fn list(&self) -> Vec<TerminalInfo> {
        lock(&self.sessions).values().map(|s| s.info()).collect()
    }

    pub fn remove(&self, id: &str) -> Option<Arc<TerminalSession>> {
        lock(&self.sessions).remove(id)
    }

    pub fn kill_all(&self) {
        for session in lock(&self.sessions).values() {
            let _ = session.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn options(command: &[&str]) -> SpawnOptions {
        SpawnOptions {
            cwd: std::env::temp_dir(),
            command: Some(command.iter().map(|s| s.to_string()).collect()),
            env: BTreeMap::new(),
            columns: 40,
            rows: 10,
            label: None,
            workspace: None,
        }
    }

    fn wait_until(mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if check() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("condition not met in time");
    }

    #[test]
    fn runs_a_command_and_reports_exit() {
        let manager = TerminalManager::default();
        let session = manager
            .create(options(&["/bin/sh", "-c", "printf 'hi there'; exit 3"]))
            .expect("spawn");
        wait_until(|| !session.is_running());
        assert!(session.screen_text().contains("hi there"));
        assert_eq!(session.info().exit_code, Some(3));
    }

    #[test]
    fn interactive_input_reaches_the_program() {
        let manager = TerminalManager::default();
        let session = manager.create(options(&["/bin/cat"])).expect("spawn");
        session.write(b"echo-me\n").expect("write");
        wait_until(|| session.screen_text().matches("echo-me").count() >= 2);
        session.kill().expect("kill");
        wait_until(|| !session.is_running());
    }

    #[test]
    fn reattach_resumes_from_sequence_or_falls_back_to_snapshot() {
        let manager = TerminalManager::default();
        let session = manager.create(options(&["/bin/cat"])).expect("spawn");
        session.write(b"first\n").expect("write");
        wait_until(|| session.screen_text().contains("first"));
        let (fresh, _) = session.attach(None);
        assert!(fresh.snapshot.as_deref().unwrap_or("").contains("first"));
        let resume_from = fresh.next_seq;

        session.write(b"second\n").expect("write");
        wait_until(|| session.screen_text().contains("second"));
        let (resumed, _) = session.attach(Some(resume_from));
        assert!(resumed.snapshot.is_none());
        let replay: Vec<u8> = resumed.chunks.iter().flat_map(|(_, d)| d.clone()).collect();
        let text = String::from_utf8_lossy(&replay);
        assert!(text.contains("second") && !text.contains("first"), "{text}");

        let (stale, _) = session.attach(Some(resumed.next_seq + 100));
        assert!(stale.snapshot.is_some());
        session.kill().expect("kill");
    }

    #[test]
    fn resize_changes_the_emulator_size() {
        let manager = TerminalManager::default();
        let session = manager.create(options(&["/bin/cat"])).expect("spawn");
        session.resize(100, 30).expect("resize");
        let info = session.info();
        assert_eq!((info.columns, info.rows), (100, 30));
        session.kill().expect("kill");
    }
}
