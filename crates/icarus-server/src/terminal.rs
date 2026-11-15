use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use filedescriptor::FileDescriptor;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::io::{Errno, FdFlags, fcntl_setfd};
use rustix::process::{Pid, Signal, kill_process};
use serde::{Deserialize, Serialize};
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
    /// The server stopped without handing this terminal on, so its process and output are gone.
    pub lost: bool,
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

/// What `terminals.json` keeps per terminal, so a server that died can still list what it lost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalRecord {
    pub id: String,
    pub label: Option<String>,
    pub workspace: Option<String>,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub created_at_ms: u64,
    pub columns: u16,
    pub rows: u16,
}

/// A terminal as handed to the next server: its record, live state and screen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParkedTerminal {
    #[serde(flatten)]
    pub record: TerminalRecord,
    pub title: Option<String>,
    pub pid: Option<u32>,
    pub running: bool,
    pub exit_code: Option<u32>,
    pub next_seq: u64,
    /// Scrollback and screen as replayable VT.
    pub snapshot: String,
    /// Recent raw output (base64), so clients keep resuming by sequence number.
    pub ring: Vec<(u64, String)>,
    /// Whether a PTY master travels with this terminal.
    pub has_pty: bool,
}

struct State {
    emulator: Emulator,
    ring: VecDeque<(u64, Vec<u8>)>,
    ring_bytes: usize,
    next_seq: u64,
    exit_code: Option<u32>,
    running: bool,
    pid: Option<u32>,
    /// Bumped by `respawn`, so the previous process's reader cannot mark the new one exited.
    generation: u64,
    lost: bool,
}

/// The PTY master and the thread reading it.
struct Io {
    master: File,
    stop: OwnedFd,
    reader: Option<JoinHandle<()>>,
}

pub struct TerminalSession {
    id: String,
    label: Option<String>,
    workspace: Option<String>,
    cwd: PathBuf,
    command: Vec<String>,
    env: BTreeMap<String, String>,
    created_at_ms: u64,
    state: Mutex<State>,
    io: Mutex<Option<Io>>,
    events: broadcast::Sender<TerminalEvent>,
}

type Child = Box<dyn portable_pty::Child + Send + Sync>;

/// A started process on a fresh PTY, before it is wired into a session.
struct Pty {
    child: Child,
    master: File,
    pid: Option<u32>,
}

fn open_pty(
    id: &str,
    command: &[String],
    cwd: &PathBuf,
    workspace: Option<&str>,
    env: &BTreeMap<String, String>,
    size: PtySize,
) -> Result<Pty> {
    let pair = native_pty_system()
        .openpty(size)
        .map_err(|e| anyhow!("openpty: {e}"))?;
    let mut builder = CommandBuilder::new(&command[0]);
    builder.args(&command[1..]);
    builder.cwd(cwd);
    builder.env("TERM", "xterm-256color");
    builder.env("COLORTERM", "truecolor");
    builder.env("ICARUS_TERMINAL_ID", id);
    if let Some(workspace) = workspace {
        builder.env("ICARUS_WORKSPACE_ID", workspace);
    }
    for (key, value) in env {
        builder.env(key, value);
    }
    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| anyhow!("spawn {}: {e}", command[0]))?;
    drop(pair.slave);
    let pid = child.process_id();
    // Our own copy of the master: portable-pty's writer sends EOF to the shell when dropped.
    let raw = pair
        .master
        .as_raw_fd()
        .context("the PTY has no master fd")?;
    let master = FileDescriptor::dup(&raw)
        .and_then(|fd| fd.as_file())
        .context("duplicate the PTY master")?;
    Ok(Pty { child, master, pid })
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

fn pty_size(columns: u16, rows: u16) -> PtySize {
    PtySize {
        rows: rows.max(1),
        cols: columns.max(2),
        pixel_width: 0,
        pixel_height: 0,
    }
}

pub fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string())
}

impl TerminalSession {
    fn new(record: TerminalRecord, state: State) -> Arc<Self> {
        let (events, _) = broadcast::channel(4096);
        Arc::new(Self {
            id: record.id,
            label: record.label,
            workspace: record.workspace,
            cwd: record.cwd,
            command: record.command,
            env: record.env,
            created_at_ms: record.created_at_ms,
            state: Mutex::new(state),
            io: Mutex::new(None),
            events,
        })
    }

    fn idle_state(emulator: Emulator) -> State {
        State {
            emulator,
            ring: VecDeque::new(),
            ring_bytes: 0,
            next_seq: 0,
            exit_code: None,
            running: false,
            pid: None,
            generation: 0,
            lost: false,
        }
    }

    pub fn spawn(id: String, options: SpawnOptions) -> Result<Arc<Self>> {
        let size = pty_size(options.columns, options.rows);
        let command = options
            .command
            .clone()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| vec![default_shell(), "-l".to_string()]);
        let pty = open_pty(
            &id,
            &command,
            &options.cwd,
            options.workspace.as_deref(),
            &options.env,
            size,
        )?;
        let mut state = Self::idle_state(Emulator::new(size.cols, size.rows));
        state.running = true;
        state.pid = pty.pid;
        let session = Self::new(
            TerminalRecord {
                id,
                label: options.label,
                workspace: options.workspace,
                cwd: options.cwd,
                command,
                env: options.env,
                created_at_ms: now_ms(),
                columns: size.cols,
                rows: size.rows,
            },
            state,
        );
        session.start_io(pty.master, Some(pty.child), 0)?;
        Ok(session)
    }

    /// Takes over a terminal another server parked; its process keeps running untouched.
    pub fn adopt(parked: ParkedTerminal, master: Option<OwnedFd>) -> Result<Arc<Self>> {
        let size = pty_size(parked.record.columns, parked.record.rows);
        let mut emulator = Emulator::new(size.cols, size.rows);
        emulator.restore(parked.snapshot.as_bytes(), parked.title.clone());
        let mut state = Self::idle_state(emulator);
        for (seq, data) in &parked.ring {
            let data = B64.decode(data).context("decode parked output")?;
            state.ring_bytes += data.len();
            state.ring.push_back((*seq, data));
        }
        state.next_seq = parked.next_seq;
        state.exit_code = parked.exit_code;
        state.pid = parked.pid;
        state.running = parked.running && master.is_some();
        let session = Self::new(parked.record, state);
        if let Some(master) = master.filter(|_| parked.running) {
            session.start_io(File::from(master), None, 0)?;
        }
        Ok(session)
    }

    /// A terminal the previous server listed but could not hand on (it crashed or was killed).
    pub fn lost(record: TerminalRecord) -> Arc<Self> {
        let size = pty_size(record.columns, record.rows);
        let mut state = Self::idle_state(Emulator::new(size.cols, size.rows));
        state.lost = true;
        Self::new(record, state)
    }

    fn start_io(
        self: &Arc<Self>,
        master: File,
        child: Option<Child>,
        generation: u64,
    ) -> Result<()> {
        let reader = master.try_clone().context("clone the PTY master")?;
        let (stop_reader, stop) = rustix::pipe::pipe().context("create a stop pipe")?;
        fcntl_setfd(&stop_reader, FdFlags::CLOEXEC)?;
        fcntl_setfd(&stop, FdFlags::CLOEXEC)?;
        let reading = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name(format!("pty-{}", reading.id))
            .spawn(move || reading.read_loop(reader, stop_reader, child, generation))
            .context("spawn pty reader thread")?;
        *lock(&self.io) = Some(Io {
            master,
            stop,
            reader: Some(handle),
        });
        Ok(())
    }

    /// Starts the command again on a fresh PTY after it ended; the screen and scrollback stay.
    pub fn respawn(self: &Arc<Self>) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }
        let (columns, rows) = lock(&self.state).emulator.size();
        let pty = open_pty(
            &self.id,
            &self.command,
            &self.cwd,
            self.workspace.as_deref(),
            &self.env,
            pty_size(columns, rows),
        )?;
        let generation = {
            let mut state = lock(&self.state);
            state.generation += 1;
            state.running = true;
            state.lost = false;
            state.exit_code = None;
            state.pid = pty.pid;
            state.generation
        };
        self.start_io(pty.master, Some(pty.child), generation)
    }

    fn read_loop(&self, reader: File, stop: OwnedFd, child: Option<Child>, generation: u64) {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let mut fds = [
                PollFd::new(&reader, PollFlags::IN),
                PollFd::new(&stop, PollFlags::IN),
            ];
            match poll(&mut fds, None) {
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(_) => break,
            }
            if !fds[1].revents().is_empty() {
                // Parked: the next server reads from here on, and the process lives on.
                return;
            }
            match (&reader).read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => self.ingest(&buffer[..n]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                    ) => {}
                Err(_) => break,
            }
        }
        // An adopted process is not our child, so its exit code is unknown.
        let code = child
            .and_then(|mut child| child.wait().ok())
            .map(|status| status.exit_code());
        {
            let mut state = lock(&self.state);
            if state.generation != generation {
                return;
            }
            state.running = false;
            state.exit_code = code;
        }
        let _ = self.events.send(TerminalEvent::Exit { code });
    }

    fn ingest(&self, bytes: &[u8]) {
        let data = Arc::new(bytes.to_vec());
        let effects;
        {
            let mut state = lock(&self.state);
            effects = state.emulator.feed(bytes);
            let seq = state.next_seq;
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
        if !effects.pty_replies.is_empty()
            && let Some(io) = lock(&self.io).as_ref()
        {
            let _ = (&io.master).write_all(&effects.pty_replies);
        }
        if let Some(title) = effects.title {
            let _ = self.events.send(TerminalEvent::Title(title));
        }
        if effects.bell {
            let _ = self.events.send(TerminalEvent::Bell);
        }
    }

    /// Stops reading and returns the terminal's state plus its PTY master (if the process runs).
    pub fn park(&self) -> (ParkedTerminal, Option<OwnedFd>) {
        let io = lock(&self.io).take();
        let master = io.map(|mut io| {
            let _ = rustix::io::write(&io.stop, b"x");
            if let Some(reader) = io.reader.take() {
                let _ = reader.join();
            }
            io.master
        });
        let state = lock(&self.state);
        let (columns, rows) = state.emulator.size();
        let master = master.filter(|_| state.running).map(OwnedFd::from);
        let parked = ParkedTerminal {
            record: self.record_sized(columns, rows),
            title: state.emulator.title().map(str::to_string),
            pid: state.pid,
            running: master.is_some(),
            exit_code: state.exit_code,
            next_seq: state.next_seq,
            snapshot: state.emulator.snapshot_vt(),
            ring: state
                .ring
                .iter()
                .map(|(seq, data)| (*seq, B64.encode(data)))
                .collect(),
            has_pty: master.is_some(),
        };
        (parked, master)
    }

    fn record(&self) -> TerminalRecord {
        let (columns, rows) = lock(&self.state).emulator.size();
        self.record_sized(columns, rows)
    }

    fn record_sized(&self, columns: u16, rows: u16) -> TerminalRecord {
        TerminalRecord {
            id: self.id.clone(),
            label: self.label.clone(),
            workspace: self.workspace.clone(),
            cwd: self.cwd.clone(),
            command: self.command.clone(),
            env: self.env.clone(),
            created_at_ms: self.created_at_ms,
            columns,
            rows,
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
            pid: state.pid,
            running: state.running,
            exit_code: state.exit_code,
            created_at_ms: self.created_at_ms,
            next_seq: state.next_seq,
            lost: state.lost,
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
        let io = lock(&self.io);
        let io = io.as_ref().context("the terminal has no process")?;
        let mut master = &io.master;
        master.write_all(bytes)?;
        master.flush()?;
        Ok(())
    }

    pub fn resize(&self, columns: u16, rows: u16) -> Result<()> {
        let size = pty_size(columns, rows);
        if let Some(io) = lock(&self.io).as_ref() {
            rustix::termios::tcsetwinsize(
                &io.master,
                rustix::termios::Winsize {
                    ws_row: size.rows,
                    ws_col: size.cols,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                },
            )
            .context("resize")?;
        }
        lock(&self.state).emulator.resize(size.cols, size.rows);
        Ok(())
    }

    pub fn screen_text(&self) -> String {
        lock(&self.state).emulator.screen_text()
    }

    pub fn full_text(&self) -> String {
        lock(&self.state).emulator.full_text()
    }

    /// Sends SIGHUP, as closing a terminal window does.
    pub fn kill(&self) -> Result<()> {
        let state = lock(&self.state);
        if !state.running {
            return Ok(());
        }
        let pid = state
            .pid
            .and_then(|pid| Pid::from_raw(i32::try_from(pid).ok()?))
            .context("the terminal has no process id")?;
        match kill_process(pid, Signal::HUP) {
            Ok(()) | Err(Errno::SRCH) => Ok(()),
            Err(error) => Err(error).context("kill terminal process"),
        }
    }

    pub fn is_running(&self) -> bool {
        lock(&self.state).running
    }
}

#[derive(Default)]
pub struct TerminalManager {
    sessions: Mutex<BTreeMap<String, Arc<TerminalSession>>>,
    /// `terminals.json`; absent in tests that need no persistence.
    registry: Option<PathBuf>,
}

impl TerminalManager {
    /// Adopts what the previous server handed over and marks the rest of its terminals lost.
    pub fn restore(registry: PathBuf, parked: Vec<(ParkedTerminal, Option<OwnedFd>)>) -> Self {
        let mut records: Vec<TerminalRecord> = std::fs::read(&registry)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let mut sessions = BTreeMap::new();
        for (terminal, master) in parked {
            let record = terminal.record.clone();
            let session = TerminalSession::adopt(terminal, master).unwrap_or_else(|error| {
                tracing::warn!("could not adopt terminal {}: {error:#}", record.id);
                TerminalSession::lost(record.clone())
            });
            sessions.insert(record.id.clone(), session);
        }
        let adopted: BTreeSet<String> = sessions.keys().cloned().collect();
        records.retain(|record| !adopted.contains(&record.id));
        for record in records {
            tracing::info!(
                "terminal {} was lost when the last server stopped",
                record.id
            );
            sessions.insert(record.id.clone(), TerminalSession::lost(record));
        }
        let manager = Self {
            sessions: Mutex::new(sessions),
            registry: Some(registry),
        };
        manager.save();
        manager
    }

    fn save(&self) {
        let Some(path) = &self.registry else {
            return;
        };
        let records: Vec<TerminalRecord> = lock(&self.sessions)
            .values()
            .map(|session| session.record())
            .collect();
        let write = || -> Result<()> {
            let partial = path.with_extension("json.partial");
            std::fs::write(&partial, serde_json::to_vec(&records)?)?;
            std::fs::rename(&partial, path)?;
            Ok(())
        };
        if let Err(error) = write() {
            tracing::warn!("could not save {}: {error:#}", path.display());
        }
    }

    pub fn create(&self, options: SpawnOptions) -> Result<Arc<TerminalSession>> {
        let id = format!("term-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let session = TerminalSession::spawn(id.clone(), options)?;
        lock(&self.sessions).insert(id, Arc::clone(&session));
        self.save();
        Ok(session)
    }

    pub fn get(&self, id: &str) -> Option<Arc<TerminalSession>> {
        lock(&self.sessions).get(id).cloned()
    }

    pub fn list(&self) -> Vec<TerminalInfo> {
        lock(&self.sessions).values().map(|s| s.info()).collect()
    }

    pub fn remove(&self, id: &str) -> Option<Arc<TerminalSession>> {
        let removed = lock(&self.sessions).remove(id);
        if removed.is_some() {
            self.save();
        }
        removed
    }

    pub fn kill_all(&self) {
        for session in lock(&self.sessions).values() {
            let _ = session.kill();
        }
    }

    /// After a deliberate stop the next server starts clean instead of listing lost terminals.
    pub fn forget_all(&self) {
        if let Some(path) = &self.registry {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Stops every reader and returns the terminals with the PTY masters of running ones.
    pub fn park_all(&self) -> Vec<(ParkedTerminal, Option<OwnedFd>)> {
        let sessions: Vec<_> = lock(&self.sessions).values().cloned().collect();
        sessions.iter().map(|session| session.park()).collect()
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
    fn respawn_restarts_the_command_and_keeps_the_scrollback() {
        let manager = TerminalManager::default();
        let session = manager.create(options(&["/bin/cat"])).expect("spawn");
        session.write(b"before-sleep\n").expect("write");
        wait_until(|| session.full_text().contains("before-sleep"));
        let first_pid = session.info().pid;
        session.kill().expect("kill");
        wait_until(|| !session.is_running());

        session.respawn().expect("respawn");
        assert!(session.is_running());
        assert_ne!(session.info().pid, first_pid);
        session.write(b"after-wake\n").expect("write");
        wait_until(|| session.full_text().contains("after-wake"));
        assert!(session.full_text().contains("before-sleep"));
        // The old reader is gone; only the new process's exit counts.
        std::thread::sleep(Duration::from_millis(100));
        assert!(session.is_running());
        session.kill().expect("kill");
        wait_until(|| !session.is_running());
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

    #[test]
    fn a_parked_terminal_is_adopted_with_its_process_screen_and_sequence() {
        let manager = TerminalManager::default();
        let session = manager.create(options(&["/bin/cat"])).expect("spawn");
        session.write(b"before-park\n").expect("write");
        wait_until(|| session.screen_text().matches("before-park").count() >= 2);
        let before = session.info();

        let (parked, master) = session.park();
        assert!(parked.has_pty);
        let master = master.expect("master");
        assert!(session.write(b"x").is_err(), "the parked session lets go");
        // Typed while nobody reads; the next owner still sees the echo.
        File::from(master.try_clone().expect("dup"))
            .write_all(b"in-the-gap\n")
            .expect("write");

        assert_eq!(parked.next_seq, before.next_seq);
        let adopted = TerminalSession::adopt(parked, Some(master)).expect("adopt");
        wait_until(|| adopted.screen_text().matches("in-the-gap").count() >= 2);
        let after = adopted.info();
        assert_eq!(after.pid, before.pid);
        assert!(after.running && after.next_seq > before.next_seq);
        assert!(adopted.full_text().contains("before-park"));
        let (resumed, _) = adopted.attach(Some(before.next_seq - 1));
        assert!(resumed.snapshot.is_none(), "resumes by sequence");
        assert_eq!(resumed.chunks[0].0, before.next_seq - 1);

        adopted.write(b"after-park\n").expect("write");
        wait_until(|| adopted.screen_text().matches("after-park").count() >= 2);
        adopted.kill().expect("kill");
        wait_until(|| !adopted.is_running());
        assert_eq!(adopted.info().exit_code, None);
    }

    #[test]
    fn restore_lists_unhanded_terminals_as_lost_and_they_can_respawn() {
        let dir = tempfile::tempdir().expect("dir");
        let registry = dir.path().join("terminals.json");
        let first = TerminalManager::restore(registry.clone(), Vec::new());
        let session = first.create(options(&["/bin/cat"])).expect("spawn");
        let id = session.id().to_string();
        session.kill().expect("kill");
        drop(first);

        let second = TerminalManager::restore(registry.clone(), Vec::new());
        let info = second.get(&id).expect("listed").info();
        assert!(info.lost && !info.running);
        let lost = second.get(&id).expect("listed");
        lost.respawn().expect("respawn");
        assert!(!lost.info().lost && lost.is_running());
        lost.kill().expect("kill");

        second.forget_all();
        let third = TerminalManager::restore(registry, Vec::new());
        assert!(third.list().is_empty());
    }
}
