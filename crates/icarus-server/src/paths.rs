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

    pub fn state_db(&self) -> PathBuf {
        self.home.join("state.db")
    }
}
