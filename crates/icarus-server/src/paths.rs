use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Where one server keeps its socket, token and state. `ICARUS_HOME` overrides the default.
#[derive(Debug, Clone)]
pub struct ServerPaths {
    pub home: PathBuf,
}

impl ServerPaths {
    pub fn resolve(explicit: Option<&Path>) -> Result<Self> {
        let home = match explicit {
            Some(path) => path.to_path_buf(),
            None => match std::env::var_os("ICARUS_HOME") {
                Some(path) => PathBuf::from(path),
                None => dirs::home_dir()
                    .context("no home directory")?
                    .join(".icarus"),
            },
        };
        Ok(Self { home })
    }

    pub fn socket(&self) -> PathBuf {
        self.home.join("server.sock")
    }

    pub fn token(&self) -> PathBuf {
        self.home.join("server.token")
    }

    pub fn pid(&self) -> PathBuf {
        self.home.join("server.pid")
    }

    pub fn log(&self) -> PathBuf {
        self.home.join("server.log")
    }

    pub fn tls_cert(&self) -> PathBuf {
        self.home.join("tls-cert.der")
    }

    pub fn tls_key(&self) -> PathBuf {
        self.home.join("tls-key.der")
    }

    /// The TCP addresses the running server listens on, one per line.
    pub fn listen_addrs(&self) -> PathBuf {
        self.home.join("server.addr")
    }

    /// Where a holder keeps terminals between a stopping server and the next one.
    pub fn handoff_socket(&self) -> PathBuf {
        self.home.join("handoff.sock")
    }

    pub fn terminals(&self) -> PathBuf {
        self.home.join("terminals.json")
    }

    pub fn state_db(&self) -> PathBuf {
        self.home.join("state.db")
    }
}
