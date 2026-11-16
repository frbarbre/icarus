use std::io::{IsTerminal, Read};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MARKER: &str = "__ICARUS_LOGIN_PATH__";
const TIMEOUT: Duration = Duration::from_secs(5);

const KEEP: &str = "ICARUS_KEEP_PATH";

/// Re-runs the server with the login shell's `PATH` first: launchd, systemd and plain SSH commands
/// start it without the profile that puts `claude`, `codex` and `gh` on it. Returns if nothing changes.
pub fn adopt() {
    // From a terminal the PATH is already the user's, and an interactive shell would fight it for the tty.
    if std::env::var_os(KEEP).is_some() || std::io::stdin().is_terminal() {
        return;
    }
    let Some(login) = login_shell_path(&crate::terminal::default_shell()) else {
        return;
    };
    let current = std::env::var("PATH").unwrap_or_default();
    let merged = merge(&login, &current);
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

fn login_shell_path(shell: &str) -> Option<String> {
    let script = format!("printf '{MARKER}%s{MARKER}' \"$PATH\"");
    // Interactive too: many profiles only set PATH in the interactive rc file (.zshrc, .bashrc).
    let mut child = Command::new(shell)
        .args(["-i", "-l", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Read while it runs so a chatty profile can't fill the pipe and stall the shell.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).ok();
        output
    });
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                child.kill().ok();
                child.wait().ok();
                return None;
            }
        }
    }
    let output = reader.join().ok()?;
    // Shell integrations (terminal hooks) print bytes that aren't UTF-8 around the marker.
    parse(&String::from_utf8_lossy(&output))
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
    fn a_login_shell_reports_its_path() {
        let path = login_shell_path("/bin/sh").expect("sh prints PATH");
        assert!(path.contains("/bin"), "{path}");
    }
}
