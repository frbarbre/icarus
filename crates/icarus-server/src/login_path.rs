use std::io::{IsTerminal, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MARKER: &str = "__ICARUS_LOGIN_PATH__";
const TIMEOUT: Duration = Duration::from_secs(15);
const ATTEMPTS: usize = 2;

const KEEP: &str = "ICARUS_KEEP_PATH";

/// Where version managers and package managers put the user's tools, relative to home or absolute.
const USUAL_DIRS: &[&str] = &[
    "~/.local/bin",
    "~/.cargo/bin",
    "~/.local/share/mise/shims",
    "~/.bun/bin",
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/usr/local/bin",
];

/// Re-runs the server with the login shell's `PATH` first: launchd, systemd and plain SSH commands
/// start it without the profile that puts `claude`, `codex` and `gh` on it. Returns if nothing changes.
pub fn adopt() {
    // From a terminal the PATH is already the user's, and an interactive shell would fight it for the tty.
    if std::env::var_os(KEEP).is_some() || std::io::stdin().is_terminal() {
        return;
    }
    let shell = crate::terminal::default_shell();
    let current = std::env::var("PATH").unwrap_or_default();
    let merged = match probe(&shell, TIMEOUT, ATTEMPTS) {
        Ok(login) => merge(&login, &current),
        Err(reason) => {
            let home = dirs::home_dir().unwrap_or_default();
            let merged = fallback(&home, &current, |dir| dir.is_dir());
            eprintln!(
                "warning: could not read PATH from the login shell {shell} ({reason}); using {merged}"
            );
            merged
        }
    };
    if merged == current {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let error = Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("PATH", merged)
        .env(KEEP, "1")
        .exec();
    eprintln!("could not restart with the login PATH: {error}");
}

fn probe(shell: &str, timeout: Duration, attempts: usize) -> Result<String, String> {
    let mut reasons = Vec::new();
    for _ in 0..attempts {
        match login_shell_path(shell, timeout) {
            Ok(path) => return Ok(path),
            Err(reason) => reasons.push(reason),
        }
    }
    Err(reasons.join("; "))
}

fn fallback(home: &Path, current: &str, exists: impl Fn(&Path) -> bool) -> String {
    let dirs: Vec<String> = USUAL_DIRS
        .iter()
        .map(|dir| match dir.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(dir),
        })
        .filter(|dir| exists(dir))
        .map(|dir| dir.to_string_lossy().into_owned())
        .collect();
    merge(&dirs.join(":"), current)
}

fn login_shell_path(shell: &str, timeout: Duration) -> Result<String, String> {
    let script = format!("printf '{MARKER}%s{MARKER}' \"$PATH\"");
    // Interactive too: many profiles only set PATH in the interactive rc file (.zshrc, .bashrc).
    let mut child = Command::new(shell)
        .args(["-i", "-l", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("start: {error}"))?;
    // Read while it runs so a chatty profile can't fill the pipe and stall the shell.
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).ok();
        output
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            result => {
                child.kill().ok();
                child.wait().ok();
                return Err(match result {
                    Err(error) => format!("wait: {error}"),
                    _ => format!("timed out after {}s", timeout.as_secs_f32()),
                });
            }
        }
    }
    let output = reader.join().map_err(|_| "reader panicked")?;
    // Shell integrations (terminal hooks) print bytes that aren't UTF-8 around the marker.
    parse(&String::from_utf8_lossy(&output)).ok_or_else(|| "no PATH in its output".to_owned())
}

fn parse(output: &str) -> Option<String> {
    let (_, rest) = output.split_once(MARKER)?;
    let (path, _) = rest.split_once(MARKER)?;
    (!path.trim().is_empty()).then(|| path.to_owned())
}

fn merge(login: &str, current: &str) -> String {
    let mut entries: Vec<&str> = Vec::new();
    for entry in login.split(':').chain(current.split(':')) {
        if !entry.is_empty() && !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    entries.join(":")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_shell(dir: &Path, body: &str) -> String {
        let path = dir.join("fake-shell");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write the fake shell");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn the_path_is_read_from_between_the_markers_despite_profile_noise() {
        let output = format!("Welcome!\n{MARKER}/opt/bin:/usr/bin{MARKER}\nbye");
        assert_eq!(parse(&output).as_deref(), Some("/opt/bin:/usr/bin"));
        assert_eq!(parse("no markers here"), None);
        assert_eq!(parse(&format!("{MARKER}{MARKER}")), None);
    }

    #[test]
    fn login_entries_come_first_and_current_ones_are_kept_once() {
        assert_eq!(
            merge("/home/u/.local/bin:/usr/bin", "/usr/bin:/bin:"),
            "/home/u/.local/bin:/usr/bin:/bin"
        );
    }

    #[test]
    fn the_fallback_prepends_the_usual_tool_dirs_that_exist() {
        let home = Path::new("/home/u");
        let existing = [
            "/home/u/.local/bin",
            "/home/u/.bun/bin",
            "/opt/homebrew/bin",
        ];
        let path = fallback(home, "/usr/bin:/bin:/opt/homebrew/bin", |dir| {
            existing.iter().any(|known| Path::new(known) == dir)
        });
        assert_eq!(
            path,
            "/home/u/.local/bin:/home/u/.bun/bin:/opt/homebrew/bin:/usr/bin:/bin"
        );
    }

    #[test]
    fn a_login_shell_reports_its_path() {
        let path = login_shell_path("/bin/sh", TIMEOUT).expect("sh prints PATH");
        assert!(path.contains("/bin"), "{path}");
    }

    #[test]
    fn a_slow_shell_times_out_on_every_attempt_and_says_so() {
        let dir = tempfile::tempdir().expect("dir");
        let shell = fake_shell(dir.path(), "exec sleep 5");
        let started = Instant::now();
        let error = probe(&shell, Duration::from_millis(200), 2).expect_err("never answers");
        assert!(started.elapsed() < Duration::from_secs(3), "{error}");
        assert_eq!(error.matches("timed out").count(), 2, "{error}");
    }

    #[test]
    fn a_shell_that_is_slow_once_succeeds_on_the_retry() {
        let dir = tempfile::tempdir().expect("dir");
        let flag = dir.path().join("slow-once");
        let shell = fake_shell(
            dir.path(),
            &format!(
                "if [ ! -e '{flag}' ]; then touch '{flag}'; exec sleep 5; fi\nprintf '{MARKER}/fake/bin{MARKER}'",
                flag = flag.display()
            ),
        );
        let path = probe(&shell, Duration::from_millis(1500), 2).expect("the retry answers");
        assert_eq!(path, "/fake/bin");
    }
}
