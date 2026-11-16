//! PTY masters pass through a holder process, so the master never closes and no child gets SIGHUP.

use std::io::{IoSlice, IoSliceMut, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use rustix::io::{FdFlags, fcntl_setfd};
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};
use serde::{Deserialize, Serialize};

use crate::paths::ServerPaths;
use crate::terminal::ParkedTerminal;

pub const VERSION: u32 = 1;

const FDS_PER_MESSAGE: usize = 32;
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_HOLD: Duration = Duration::from_secs(120);

const PUT: u8 = b'P';
const TAKE: u8 = b'T';
const OK: u8 = b'K';
const FULL: u8 = b'F';
const EMPTY: u8 = b'E';
const BUSY: u8 = b'X';

/// Everything one server hands the next.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Parcel {
    pub version: u32,
    /// Kept so connected clients reconnect with the token they already hold.
    pub token: Option<String>,
    #[serde(default)]
    pub listen: Vec<String>,
    #[serde(default)]
    pub allow_public: bool,
    #[serde(default)]
    pub terminals: Vec<ParkedTerminal>,
}

impl Parcel {
    fn pty_count(&self) -> usize {
        self.terminals.iter().filter(|t| t.has_pty).count()
    }

    /// Pairs each terminal with its PTY master, in order.
    pub fn into_terminals(self, fds: Vec<OwnedFd>) -> Vec<(ParkedTerminal, Option<OwnedFd>)> {
        let mut fds = fds.into_iter();
        self.terminals
            .into_iter()
            .map(|terminal| {
                let fd = if terminal.has_pty { fds.next() } else { None };
                (terminal, fd)
            })
            .collect()
    }
}

fn send(stream: &UnixStream, parcel: &Parcel, fds: &[OwnedFd]) -> Result<()> {
    ensure!(parcel.pty_count() == fds.len(), "parcel and fds disagree");
    let json = serde_json::to_vec(parcel)?;
    let mut writer = stream;
    writer.write_all(&(json.len() as u64).to_be_bytes())?;
    writer.write_all(&json)?;
    for chunk in fds.chunks(FDS_PER_MESSAGE) {
        let borrowed: Vec<BorrowedFd<'_>> = chunk.iter().map(AsFd::as_fd).collect();
        let mut space =
            [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(FDS_PER_MESSAGE))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        ensure!(
            control.push(SendAncillaryMessage::ScmRights(&borrowed)),
            "fd batch too large"
        );
        let count = (chunk.len() as u32).to_be_bytes();
        let sent = sendmsg(
            stream,
            &[IoSlice::new(&count)],
            &mut control,
            SendFlags::empty(),
        )
        .context("send PTY fds")?;
        writer.write_all(&count[sent..])?;
    }
    Ok(())
}

fn receive(stream: &UnixStream) -> Result<(Parcel, Vec<OwnedFd>)> {
    let mut reader = stream;
    let mut length = [0u8; 8];
    reader.read_exact(&mut length)?;
    let length = usize::try_from(u64::from_be_bytes(length))?;
    let mut json = vec![0u8; length];
    reader.read_exact(&mut json)?;
    let parcel: Parcel = serde_json::from_slice(&json).context("parse the handoff parcel")?;
    ensure!(
        parcel.version == VERSION,
        "handoff version {} is not {VERSION}",
        parcel.version
    );
    let expected = parcel.pty_count();
    let mut fds = Vec::with_capacity(expected);
    while fds.len() < expected {
        let mut count = [0u8; 4];
        let mut space =
            [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(FDS_PER_MESSAGE))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let message = recvmsg(
            stream,
            &mut [IoSliceMut::new(&mut count)],
            &mut control,
            RecvFlags::empty(),
        )
        .context("receive PTY fds")?;
        ensure!(message.bytes > 0, "the sender closed the handoff early");
        ensure!(
            !message.flags.contains(ReturnFlags::CTRUNC),
            "PTY fds were truncated"
        );
        let mut received = 0;
        for item in control.drain() {
            if let RecvAncillaryMessage::ScmRights(incoming) = item {
                for fd in incoming {
                    fcntl_setfd(&fd, FdFlags::CLOEXEC)?;
                    fds.push(fd);
                    received += 1;
                }
            }
        }
        reader.read_exact(&mut count[message.bytes..])?;
        ensure!(
            received == u32::from_be_bytes(count) as usize,
            "expected {} PTY fds, got {received}",
            u32::from_be_bytes(count)
        );
    }
    Ok((parcel, fds))
}

fn read_byte(stream: &UnixStream) -> Result<u8> {
    let mut byte = [0u8; 1];
    let mut reader = stream;
    reader.read_exact(&mut byte)?;
    Ok(byte[0])
}

fn write_byte(stream: &UnixStream, byte: u8) -> Result<()> {
    let mut writer = stream;
    writer.write_all(&[byte])?;
    Ok(())
}

fn connect(paths: &ServerPaths) -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect(paths.handoff_socket())?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

/// Gives the parcel to the holder; on success the holder owns the only other copies of the fds.
pub fn put(paths: &ServerPaths, parcel: &Parcel, fds: &[OwnedFd]) -> Result<()> {
    let stream = connect(paths).context("reach the handoff holder")?;
    write_byte(&stream, PUT)?;
    send(&stream, parcel, fds)?;
    match read_byte(&stream)? {
        OK => Ok(()),
        BUSY => bail!("the handoff holder already holds another server's terminals"),
        other => bail!("unexpected handoff reply {other}"),
    }
}

/// Takes what a holder keeps for this home, if one is running.
pub fn take(paths: &ServerPaths) -> Result<Option<(Parcel, Vec<OwnedFd>)>> {
    let Ok(stream) = connect(paths) else {
        return Ok(None);
    };
    write_byte(&stream, TAKE)?;
    match read_byte(&stream)? {
        EMPTY => Ok(None),
        FULL => {
            let received = receive(&stream)?;
            write_byte(&stream, OK)?;
            Ok(Some(received))
        }
        other => bail!("unexpected handoff reply {other}"),
    }
}

fn hold_timeout() -> Duration {
    std::env::var("ICARUS_HANDOFF_HOLD_SECS")
        .ok()
        .and_then(|secs| secs.parse().ok())
        .map_or(DEFAULT_HOLD, Duration::from_secs)
}

enum Next {
    Continue,
    Done,
}

fn serve(stream: &UnixStream, held: &mut Option<(Parcel, Vec<OwnedFd>)>) -> Result<Next> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    match read_byte(stream)? {
        PUT if held.is_some() => {
            write_byte(stream, BUSY)?;
            Ok(Next::Continue)
        }
        PUT => {
            let received = receive(stream)?;
            tracing::info!(
                "holding {} terminal(s) for the next server",
                received.0.terminals.len()
            );
            *held = Some(received);
            write_byte(stream, OK)?;
            Ok(Next::Continue)
        }
        TAKE => {
            let Some((parcel, fds)) = held.take() else {
                write_byte(stream, EMPTY)?;
                return Ok(Next::Done);
            };
            let handed = write_byte(stream, FULL)
                .and_then(|()| send(stream, &parcel, &fds))
                .and_then(|()| read_byte(stream));
            match handed {
                Ok(OK) => Ok(Next::Done),
                Ok(other) => {
                    *held = Some((parcel, fds));
                    bail!("unexpected handoff ack {other}")
                }
                Err(error) => {
                    *held = Some((parcel, fds));
                    Err(error)
                }
            }
        }
        other => bail!("unknown handoff request {other}"),
    }
}

/// `icarus-server hold`: keeps parked terminals alive until the next server takes them.
pub fn hold(paths: &ServerPaths) -> Result<()> {
    let path = paths.handoff_socket();
    if UnixStream::connect(&path).is_ok() {
        bail!("a handoff holder is already running at {}", path.display());
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + hold_timeout();
    let mut held = None;
    loop {
        if Instant::now() >= deadline {
            if held.is_some() {
                tracing::warn!("no server took the parked terminals in time; they end now");
            }
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                match serve(&stream, &mut held) {
                    Ok(Next::Done) => break,
                    Ok(Next::Continue) => {}
                    Err(error) => tracing::warn!("handoff: {error:#}"),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error).context("accept a handoff connection"),
        }
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// Starts a holder (from this same binary) and waits until it accepts connections.
pub fn spawn_holder(paths: &ServerPaths) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.log())?;
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .arg("--home")
        .arg(&paths.home)
        .arg("hold")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        // Its own process group, so a service manager stopping the server leaves it alone.
        .process_group(0)
        .spawn()
        .context("start the handoff holder")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if UnixStream::connect(paths.handoff_socket()).is_ok() {
            // Reaped by a thread so the holder never lingers as a zombie of this server.
            std::thread::spawn(move || child.wait());
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!("the handoff holder exited early ({status})");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            bail!("the handoff holder did not start");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parcel_and_its_fds_cross_a_socket() {
        let (left, right) = UnixStream::pair().expect("pair");
        let (pipe_read, pipe_write) = rustix::pipe::pipe().expect("pipe");
        let terminals = (0..40)
            .map(|i| ParkedTerminal {
                record: crate::terminal::TerminalRecord {
                    id: format!("term-{i}"),
                    label: None,
                    workspace: None,
                    cwd: "/tmp".into(),
                    command: vec!["/bin/cat".into()],
                    env: Default::default(),
                    created_at_ms: 0,
                    columns: 80,
                    rows: 24,
                    task: false,
                },
                title: None,
                pid: None,
                running: i % 2 == 0,
                exit_code: None,
                next_seq: i,
                snapshot: String::new(),
                ring: Vec::new(),
                has_pty: i % 2 == 0,
            })
            .collect();
        let parcel = Parcel {
            version: VERSION,
            token: Some("t".into()),
            terminals,
            ..Parcel::default()
        };
        let fds: Vec<OwnedFd> = (0..20)
            .map(|_| rustix::io::dup(&pipe_write).expect("dup"))
            .collect();
        std::thread::scope(|scope| {
            scope.spawn(|| send(&left, &parcel, &fds).expect("send"));
            let (received, fds) = receive(&right).expect("receive");
            assert_eq!(received.terminals.len(), 40);
            assert_eq!(fds.len(), 20);
            let paired = received.into_terminals(fds);
            assert!(paired.iter().all(|(t, fd)| t.has_pty == fd.is_some()));
            let (_, last) = paired
                .into_iter()
                .rev()
                .find(|(t, _)| t.has_pty)
                .expect("pty");
            rustix::io::write(last.expect("fd"), b"z").expect("write through a received fd");
        });
        let mut byte = [0u8; 1];
        rustix::io::read(&pipe_read, &mut byte).expect("read");
        assert_eq!(&byte, b"z");
    }
}
