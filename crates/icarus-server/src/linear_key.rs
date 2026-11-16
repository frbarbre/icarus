//! Where a pasted Linear API key lives: the macOS login Keychain, or a 0600 file under the server home.

use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const KEYCHAIN_SERVICE: &str = "dev.icarus.linear";
const KEYCHAIN_ACCOUNT: &str = "api-key";
const SECURITY: &str = "/usr/bin/security";
const SECURITY_TIMEOUT: Duration = Duration::from_secs(15);
/// `security` exits with this when the item does not exist.
const ITEM_NOT_FOUND: i32 = 44;

pub trait KeyStore: Send + Sync {
    fn load(&self) -> io::Result<Option<String>>;
    fn save(&self, key: &str) -> io::Result<()>;
    fn clear(&self) -> io::Result<()>;
    /// Where the key is kept, for people: "Keychain" or "file".
    fn kind(&self) -> &'static str;
}

/// The store the server uses: fixtures and non-macOS hosts get the file, macOS the Keychain.
pub fn default_store(home: &Path) -> Box<dyn KeyStore> {
    let file = home.join("linear-api-key");
    if cfg!(target_os = "macos") && std::env::var_os("ICARUS_LINEAR_FIXTURES").is_none() {
        Box::new(KeychainStore)
    } else {
        Box::new(FileStore::new(file))
    }
}

/// A key as Linear issues them; anything else is refused before it reaches a store or a command line.
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 512
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl KeyStore for FileStore {
    fn load(&self) -> io::Result<Option<String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(Some(text.trim().to_owned()).filter(|key| !key.is_empty())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn save(&self, key: &str) -> io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = self.path.with_extension("tmp");
        let _ = std::fs::remove_file(&temp);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(key.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, &self.path)
    }

    fn clear(&self) -> io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    fn kind(&self) -> &'static str {
        "file"
    }
}

/// A generic password in the login Keychain, written through `security -i` so the key never shows in argv.
pub struct KeychainStore;

fn security(args: &[&str], stdin: Option<&str>) -> io::Result<(i32, String)> {
    let mut child = Command::new(SECURITY)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes())?;
    }
    let deadline = Instant::now() + SECURITY_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the Keychain did not answer",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout.read_to_string(&mut out)?;
    }
    Ok((status.code().unwrap_or(-1), out))
}

fn keychain_error(action: &str, code: i32) -> io::Error {
    io::Error::other(format!(
        "could not {action} the Keychain item (security exited {code})"
    ))
}

impl KeyStore for KeychainStore {
    fn load(&self) -> io::Result<Option<String>> {
        let (code, out) = security(
            &[
                "find-generic-password",
                "-s",
                KEYCHAIN_SERVICE,
                "-a",
                KEYCHAIN_ACCOUNT,
                "-w",
            ],
            None,
        )?;
        match code {
            0 => Ok(Some(out.trim().to_owned()).filter(|key| !key.is_empty())),
            ITEM_NOT_FOUND => Ok(None),
            code => Err(keychain_error("read", code)),
        }
    }

    fn save(&self, key: &str) -> io::Result<()> {
        if !valid_key(key) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a Linear API key",
            ));
        }
        let line = format!(
            "add-generic-password -U -s \"{KEYCHAIN_SERVICE}\" -a \"{KEYCHAIN_ACCOUNT}\" -l \"Icarus Linear API key\" -w \"{key}\"\n"
        );
        match security(&["-i"], Some(&line))? {
            (0, _) => Ok(()),
            (code, _) => Err(keychain_error("save", code)),
        }
    }

    fn clear(&self) -> io::Result<()> {
        let (code, _) = security(
            &[
                "delete-generic-password",
                "-s",
                KEYCHAIN_SERVICE,
                "-a",
                KEYCHAIN_ACCOUNT,
            ],
            None,
        )?;
        match code {
            0 | ITEM_NOT_FOUND => Ok(()),
            code => Err(keychain_error("delete", code)),
        }
    }

    fn kind(&self) -> &'static str {
        "Keychain"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn the_file_store_round_trips_with_owner_only_permissions() {
        let dir = tempfile::tempdir().expect("dir");
        let store = FileStore::new(dir.path().join("linear-api-key"));
        assert_eq!(store.load().expect("load"), None);
        store.save("lin_api_one").expect("save");
        store.save("lin_api_two").expect("overwrite");
        assert_eq!(store.load().expect("load").as_deref(), Some("lin_api_two"));
        let mode = std::fs::metadata(dir.path().join("linear-api-key"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        store.clear().expect("clear");
        store.clear().expect("clearing twice is fine");
        assert_eq!(store.load().expect("load"), None);
    }

    #[test]
    fn only_key_shaped_text_is_accepted() {
        assert!(valid_key("lin_api_AbC123"));
        for bad in [
            "",
            "lin api",
            "lin\"api",
            "lin_api\n",
            "lin'api",
            "lin\\api",
        ] {
            assert!(!valid_key(bad), "{bad:?}");
        }
    }
}
