//! Login service so the server starts with the user session and outlives every client.

use std::path::{Path, PathBuf};
use std::process::Command;

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

pub fn install(server: &Path, home: &Path) -> Result<PathBuf> {
    let server =
        std::fs::canonicalize(server).with_context(|| format!("find {}", server.display()))?;
    std::fs::create_dir_all(home)?;
    let path = service_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if cfg!(target_os = "macos") {
        std::fs::write(&path, launchd_plist(&server, home))?;
        let uid = String::from_utf8(Command::new("id").arg("-u").output()?.stdout)?
            .trim()
            .to_string();
        let domain = format!("gui/{uid}");
        // Bootout first so reinstalling picks up a moved binary; it fails harmlessly when not loaded.
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("{domain}/{LABEL}")])
            .status();
        run(
            "launchctl",
            &["bootstrap", &domain, &path.to_string_lossy()],
        )?;
    } else {
        std::fs::write(&path, systemd_unit(&server, home))?;
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
        let uid = String::from_utf8(Command::new("id").arg("-u").output()?.stdout)?
            .trim()
            .to_string();
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{uid}/{LABEL}")])
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
    fn launchd_plist_runs_serve_with_the_home_and_restarts_on_crash() {
        let plist = launchd_plist(
            Path::new("/opt/icarus/icarus-server"),
            Path::new("/Users/a b/.icarus"),
        );
        assert!(plist.contains("<string>/opt/icarus/icarus-server</string>"));
        assert!(plist.contains("<string>/Users/a b/.icarus</string>"));
        assert!(plist.contains("<string>serve</string>"));
        assert!(plist.contains("<key>SuccessfulExit</key>"));
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
