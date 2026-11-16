//! Saved servers in `servers.json` next to the user settings (`$ICARUS_CONFIG_DIR`, else
//! `$XDG_CONFIG_HOME/icarus` or `~/.config/icarus`).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedServer {
    pub name: String,
    /// `host:port` (or `tcp://host:port`) for a `--listen` address, or `ssh://destination`.
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// For SSH servers: the remote command, if not `icarus-server proxy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// For TCP servers: an SSH destination for the same machine, so the app can open its worktrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Tcp(String),
    Ssh(String),
}

pub fn parse_address(address: &str) -> Result<Target> {
    let address = address.trim();
    if let Some(destination) = address.strip_prefix("ssh://") {
        if destination.is_empty() {
            bail!("`{address}` names no SSH host");
        }
        return Ok(Target::Ssh(destination.trim_end_matches('/').to_string()));
    }
    let hostport = address.strip_prefix("tcp://").unwrap_or(address);
    match hostport.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok() => {
            Ok(Target::Tcp(hostport.to_string()))
        }
        _ => bail!("`{address}` is not host:port or ssh://host"),
    }
}

impl SavedServer {
    pub fn target(&self) -> Result<Target> {
        parse_address(&self.address)
    }

    /// Where the server's worktrees can be opened over SSH: an `ssh://` address, else the `ssh` field.
    pub fn ssh_destination(&self) -> Option<String> {
        match self.target() {
            Ok(Target::Ssh(destination)) => Some(destination),
            _ => self
                .ssh
                .as_deref()
                .map(|destination| destination.trim().trim_start_matches("ssh://"))
                .filter(|destination| !destination.is_empty())
                .map(str::to_owned),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedServers {
    #[serde(default)]
    pub servers: Vec<SavedServer>,
}

pub fn default_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("ICARUS_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return Ok(dir.join("servers.json"));
    }
    let config = match std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        Some(dir) => dir,
        None => dirs::home_dir()
            .context("no home directory")?
            .join(".config"),
    };
    Ok(config.join("icarus").join("servers.json"))
}

impl SavedServers {
    pub fn load() -> Result<Self> {
        Self::load_from(&default_path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&default_path()?)
    }

    /// Writes the file readable by the owner only, since it holds client tokens.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&temporary, path).with_context(|| format!("write {}", path.display()))
    }

    pub fn get(&self, name: &str) -> Option<&SavedServer> {
        self.servers.iter().find(|s| s.name == name)
    }

    /// Adds `server`, replacing a saved server of the same name.
    pub fn upsert(&mut self, server: SavedServer) {
        match self.servers.iter_mut().find(|s| s.name == server.name) {
            Some(existing) => *existing = server,
            None => self.servers.push(server),
        }
    }

    pub fn remove(&mut self, name: &str) -> Option<SavedServer> {
        let index = self.servers.iter().position(|s| s.name == name)?;
        Some(self.servers.remove(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse_as_tcp_or_ssh() {
        assert_eq!(
            parse_address("100.64.1.2:7878").expect("tcp"),
            Target::Tcp("100.64.1.2:7878".into())
        );
        assert_eq!(
            parse_address("tcp://[fd7a:115c:a1e0::1]:7878").expect("v6"),
            Target::Tcp("[fd7a:115c:a1e0::1]:7878".into())
        );
        assert_eq!(
            parse_address("ssh://me@devbox").expect("ssh"),
            Target::Ssh("me@devbox".into())
        );
        assert!(parse_address("devbox").is_err());
        assert!(parse_address("ssh://").is_err());
    }

    #[test]
    fn saved_servers_round_trip_privately() {
        let dir = std::env::temp_dir().join(format!("icarus-servers-{}", std::process::id()));
        let path = dir.join("servers.json");
        assert_eq!(
            SavedServers::load_from(&path).expect("missing is empty"),
            SavedServers::default()
        );
        let mut servers = SavedServers::default();
        let desk = SavedServer {
            name: "desk".into(),
            address: "100.64.0.2:7878".into(),
            token: Some("t".into()),
            fingerprint: Some("ab".into()),
            client_id: None,
            command: None,
            ssh: Some("desk.example".into()),
        };
        servers.upsert(desk.clone());
        servers.upsert(SavedServer {
            token: Some("t2".into()),
            ..desk
        });
        servers.save_to(&path).expect("save");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let loaded = SavedServers::load_from(&path).expect("load");
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(
            loaded.get("desk").and_then(|s| s.token.as_deref()),
            Some("t2")
        );
        assert_eq!(
            loaded.get("desk").and_then(|s| s.ssh.as_deref()),
            Some("desk.example")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn worktrees_open_over_the_ssh_address_or_the_ssh_field() {
        let server = |address: &str, ssh: Option<&str>| SavedServer {
            name: "s".into(),
            address: address.into(),
            token: None,
            fingerprint: None,
            client_id: None,
            command: None,
            ssh: ssh.map(str::to_owned),
        };
        assert_eq!(
            server("ssh://me@devbox", None).ssh_destination().as_deref(),
            Some("me@devbox")
        );
        assert_eq!(
            server("100.64.0.2:7878", Some("ssh://devbox"))
                .ssh_destination()
                .as_deref(),
            Some("devbox")
        );
        assert_eq!(server("100.64.0.2:7878", None).ssh_destination(), None);
        assert_eq!(server("100.64.0.2:7878", Some(" ")).ssh_destination(), None);
        assert!(
            serde_json::from_str::<SavedServer>(r#"{"name":"old","address":"h:1"}"#)
                .expect("files saved before the ssh field still load")
                .ssh
                .is_none()
        );
    }
}
