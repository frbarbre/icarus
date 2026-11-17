//! Login service so the server starts with the user session and outlives every client.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const LABEL: &str = "dev.icarus.server";

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn launchd_plist(server: &Path, home: &Path) -> String {
    let server = xml_escape(&server.to_string_lossy());
    let home = xml_escape(&home.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{server}</string>
    <string>--home</string>
    <string>{home}</string>
    <string>serve</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>StandardErrorPath</key>
  <string>{home}/server.log</string>
</dict>
</plist>
"#
    )
}

pub fn systemd_unit(server: &Path, home: &Path) -> String {
    format!(
        "[Unit]\nDescription=Icarus server\n\n[Service]\nExecStart=\"{}\" --home \"{}\" serve\nRestart=on-failure\nKillMode=process\n\n[Install]\nWantedBy=default.target\n",
        server.display(),
        home.display()
    )
}

fn service_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    Ok(if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"))
    } else {
        home.join(".config/systemd/user/icarus-server.service")
    })
}

/// Whether the login service runs the server for `home`, not some other state directory.
pub fn installed_for(home: &Path) -> bool {
    let Ok(text) = service_path().and_then(|path| Ok(std::fs::read_to_string(path)?)) else {
        return false;
    };
    let home = home.to_string_lossy();
    text.contains(&format!("<string>{}</string>", xml_escape(&home)))
        || text.contains(&format!("--home \"{home}\""))
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("run {program}"))?;
    if !status.success() {
        bail!("{program} {} failed", args.join(" "));
    }
    Ok(())
}

/// launchd answers `bootstrap` with an I/O error while a job it just booted out is still going away.
const BOOTSTRAP_RETRIES: [Duration; 5] = [
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_millis(1600),
    Duration::from_millis(3200),
];

fn retry(delays: &[Duration], mut attempt: impl FnMut() -> Result<()>) -> Result<()> {
    let mut result = attempt();
    for delay in delays {
        if result.is_ok() {
            break;
        }
        std::thread::sleep(*delay);
        result = attempt();
    }
    result
}

fn gui_domain() -> Result<String> {
    let uid = String::from_utf8(Command::new("id").arg("-u").output()?.stdout)?
        .trim()
        .to_string();
    Ok(format!("gui/{uid}"))
}

fn launchd_loaded(target: &str) -> bool {
    Command::new("launchctl")
        .args(["print", target])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn bootstrap(domain: &str, path: &Path) -> Result<()> {
    let target = format!("{domain}/{LABEL}");
    // Bootout first so reinstalling picks up a moved binary; it fails harmlessly when not loaded.
    let _ = Command::new("launchctl")
        .args(["bootout", &target])
        .stderr(Stdio::null())
        .status();
    let deadline = Instant::now() + Duration::from_secs(5);
    while launchd_loaded(&target) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    retry(&BOOTSTRAP_RETRIES, || {
        run("launchctl", &["bootstrap", domain, &path.to_string_lossy()])
    })
}

fn wanted_definition(server: &Path, home: &Path) -> Result<(PathBuf, String)> {
    let server =
        std::fs::canonicalize(server).with_context(|| format!("find {}", server.display()))?;
    let text = if cfg!(target_os = "macos") {
        launchd_plist(&server, home)
    } else {
        systemd_unit(&server, home)
    };
    Ok((service_path()?, text))
}

/// Starts the installed service's server again, re-installing only when its definition changed.
pub fn start_again(server: &Path, home: &Path) -> Result<()> {
    let (path, wanted) = wanted_definition(server, home)?;
    let current = std::fs::read_to_string(&path).ok();
    if cfg!(target_os = "macos") {
        let target = format!("{}/{LABEL}", gui_domain()?);
        if current.as_deref() == Some(wanted.as_str()) && launchd_loaded(&target) {
            return run("launchctl", &["kickstart", &target]);
        }
        install(server, home).map(drop)
    } else {
        if current.as_deref() != Some(wanted.as_str()) {
            std::fs::write(&path, wanted)?;
            run("systemctl", &["--user", "daemon-reload"])?;
        }
        run("systemctl", &["--user", "restart", "icarus-server.service"])
    }
}

/// Kills whatever the service runs and starts it fresh; the way out when a handoff went wrong.
pub fn kickstart(server: &Path, home: &Path) -> Result<()> {
    if cfg!(target_os = "macos") {
        let target = format!("{}/{LABEL}", gui_domain()?);
        if launchd_loaded(&target) {
            return run("launchctl", &["kickstart", "-k", &target]);
        }
        install(server, home).map(drop)
    } else {
        run("systemctl", &["--user", "restart", "icarus-server.service"])
    }
}

pub fn install(server: &Path, home: &Path) -> Result<PathBuf> {
    let (path, wanted) = wanted_definition(server, home)?;
    std::fs::create_dir_all(home)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if cfg!(target_os = "macos") {
        std::fs::write(&path, wanted)?;
        bootstrap(&gui_domain()?, &path)?;
    } else {
        std::fs::write(&path, wanted)?;
        run("systemctl", &["--user", "daemon-reload"])?;
        run(
            "systemctl",
            &["--user", "enable", "--now", "icarus-server.service"],
        )?;
    }
    Ok(path)
}

pub fn uninstall() -> Result<bool> {
    let path = service_path()?;
    if !path.exists() {
        return Ok(false);
    }
    if cfg!(target_os = "macos") {
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("{}/{LABEL}", gui_domain()?)])
            .status();
    } else {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", "icarus-server.service"])
            .status();
    }
    std::fs::remove_file(&path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_step_is_retried_until_it_succeeds_or_the_delays_run_out() {
        let delays = [Duration::from_millis(1); 3];
        let mut calls = 0;
        retry(&delays, || {
            calls += 1;
            if calls < 3 { bail!("busy") } else { Ok(()) }
        })
        .expect("third try works");
        assert_eq!(calls, 3);

        let mut calls = 0;
        let error = retry(&delays, || {
            calls += 1;
            bail!("bootstrap failed {calls}")
        })
        .expect_err("never works");
        assert_eq!(calls, 4);
        assert_eq!(error.to_string(), "bootstrap failed 4");
    }

    #[test]
    fn launchd_plist_runs_serve_with_the_home_and_restarts_on_crash() {
        let plist = launchd_plist(
            Path::new("/opt/icarus/icarus-server"),
            Path::new("/Users/a b/.icarus"),
        );
        assert!(plist.contains("<string>/opt/icarus/icarus-server</string>"));
        assert!(plist.contains("<string>/Users/a b/.icarus</string>"));
        assert!(plist.contains("<string>serve</string>"));
        assert!(plist.contains("<key>SuccessfulExit</key>"));
        assert!(plist.contains(&format!(
            "<string>{}</string>",
            xml_escape("/Users/a b/.icarus")
        )));
    }

    #[test]
    fn systemd_unit_quotes_paths() {
        let unit = systemd_unit(
            Path::new("/opt/icarus-server"),
            Path::new("/home/a b/.icarus"),
        );
        assert!(
            unit.contains("ExecStart=\"/opt/icarus-server\" --home \"/home/a b/.icarus\" serve")
        );
        assert!(
            unit.contains("KillMode=process"),
            "agents must not die when the service stops"
        );
    }
}
