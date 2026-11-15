use std::fmt;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::error::{LinearError, Result};

pub const ENV_VAR: &str = "LINEAR_API_KEY";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// After a failed or rejected command run, wait this long before running it again on its own.
pub const COMMAND_RETRY: Duration = Duration::from_secs(60);

/// A Linear API key. It has no `Display`, and `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(Arc<str>);

impl ApiKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(Arc::from(key.into()))
    }

    /// The raw key, only for the `Authorization` header.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey([redacted])")
    }
}

/// `linear.api_key_command`: an argv array, or a string run with `sh -c`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCommand {
    Argv(Vec<String>),
    Shell(String),
}

impl KeyCommand {
    pub fn from_setting(value: &Value) -> Option<Self> {
        match value {
            Value::String(line) if !line.trim().is_empty() => Some(Self::Shell(line.clone())),
            Value::Array(items) => {
                let argv: Option<Vec<String>> = items
                    .iter()
                    .map(|item| item.as_str().map(str::to_owned))
                    .collect();
                argv.filter(|argv| !argv.is_empty() && !argv[0].is_empty())
                    .map(Self::Argv)
            }
            _ => None,
        }
    }

    fn command(&self) -> Command {
        match self {
            Self::Argv(argv) => {
                let mut command = Command::new(&argv[0]);
                command.args(&argv[1..]);
                command
            }
            Self::Shell(line) => {
                let mut command = Command::new("/bin/sh");
                command.arg("-c").arg(line);
                command
            }
        }
    }

    /// Runs the command; the error names what went wrong without any of its output.
    pub async fn run(&self) -> std::result::Result<ApiKey, String> {
        let mut command = self.command();
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(COMMAND_TIMEOUT, command.output())
            .await
            .map_err(|_| "timed out".to_owned())?
            .map_err(|e| format!("could not start: {}", e.kind()))?;
        if !output.status.success() {
            return Err(match output.status.code() {
                Some(code) => format!("exited with status {code}"),
                None => "was killed by a signal".to_owned(),
            });
        }
        let text = String::from_utf8(output.stdout).map_err(|_| "printed non-UTF-8".to_owned())?;
        let key = text.trim();
        if key.is_empty() {
            return Err("printed nothing".to_owned());
        }
        Ok(ApiKey::new(key))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Env,
    Command,
}

/// Where the key stands. Never carries the key or any command output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStatus {
    Env,
    Command,
    /// Configured but not tried yet.
    CommandPending,
    CommandFailed(String),
    Rejected,
    NotConfigured,
}

impl KeyStatus {
    pub fn configured(&self) -> bool {
        matches!(self, Self::Env | Self::Command | Self::CommandPending)
    }
}

#[derive(Default)]
struct State {
    command: Option<KeyCommand>,
    cached: Option<ApiKey>,
    failed: Option<(Instant, String)>,
    rejected: bool,
}

/// Resolves the key: `LINEAR_API_KEY`, else `linear.api_key_command` (run once, kept in memory), else nothing.
pub struct KeyResolver {
    env_key: Option<ApiKey>,
    env_rejected: std::sync::atomic::AtomicBool,
    state: Mutex<State>,
}

impl KeyResolver {
    pub fn new(env_key: Option<String>) -> Self {
        Self {
            env_key: env_key
                .map(|key| key.trim().to_owned())
                .filter(|key| !key.is_empty())
                .map(ApiKey::new),
            env_rejected: std::sync::atomic::AtomicBool::new(false),
            state: Mutex::new(State::default()),
        }
    }

    pub fn from_env() -> Self {
        Self::new(std::env::var(ENV_VAR).ok())
    }

    /// Follows the setting; a different command drops the cached key and any backoff.
    pub async fn set_command(&self, command: Option<KeyCommand>) {
        let mut state = self.state.lock().await;
        if state.command != command {
            *state = State {
                command,
                ..State::default()
            };
        }
    }

    /// Lets the next `key` run the command again at once (e.g. a manual refresh after `bw unlock`).
    pub async fn reset_backoff(&self) {
        let mut state = self.state.lock().await;
        state.failed = None;
        state.rejected = false;
        self.env_rejected
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    pub async fn key(&self) -> Result<(ApiKey, KeySource)> {
        if let Some(key) = &self.env_key {
            if self.env_rejected.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(LinearError::Unauthorized);
            }
            return Ok((key.clone(), KeySource::Env));
        }
        let mut state = self.state.lock().await;
        if let Some(key) = &state.cached {
            return Ok((key.clone(), KeySource::Command));
        }
        let Some(command) = state.command.clone() else {
            return Err(LinearError::NotConfigured);
        };
        if let Some((at, _)) = &state.failed
            && at.elapsed() < COMMAND_RETRY
        {
            return Err(if state.rejected {
                LinearError::Unauthorized
            } else {
                LinearError::NotConfigured
            });
        }
        match command.run().await {
            Ok(key) => {
                state.cached = Some(key.clone());
                state.failed = None;
                Ok((key, KeySource::Command))
            }
            Err(reason) => {
                state.failed = Some((Instant::now(), reason));
                state.rejected = false;
                Err(LinearError::NotConfigured)
            }
        }
    }

    /// Linear said 401 for `key`: forget it so the command runs again; returns whether a retry may help.
    pub async fn rejected(&self, source: KeySource, retried: bool) -> bool {
        match source {
            KeySource::Env => {
                self.env_rejected
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                false
            }
            KeySource::Command => {
                let mut state = self.state.lock().await;
                state.cached = None;
                if retried {
                    state.failed = Some((Instant::now(), "key rejected by Linear".to_owned()));
                    state.rejected = true;
                }
                !retried
            }
        }
    }

    pub async fn status(&self) -> KeyStatus {
        if self.env_key.is_some() {
            return if self.env_rejected.load(std::sync::atomic::Ordering::Relaxed) {
                KeyStatus::Rejected
            } else {
                KeyStatus::Env
            };
        }
        let state = self.state.lock().await;
        if state.cached.is_some() {
            return KeyStatus::Command;
        }
        match (&state.command, &state.failed) {
            (None, _) => KeyStatus::NotConfigured,
            (Some(_), Some(_)) if state.rejected => KeyStatus::Rejected,
            (Some(_), Some((_, reason))) => KeyStatus::CommandFailed(reason.clone()),
            (Some(_), None) => KeyStatus::CommandPending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn script(dir: &std::path::Path, body: &str) -> KeyCommand {
        let path = dir.join("key.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
        KeyCommand::Argv(vec!["/bin/sh".into(), path.to_string_lossy().into_owned()])
    }

    #[test]
    fn the_setting_takes_an_argv_array_or_a_shell_string() {
        assert_eq!(
            KeyCommand::from_setting(&json!(["bw", "get", "password", "Linear API key"])),
            Some(KeyCommand::Argv(vec![
                "bw".into(),
                "get".into(),
                "password".into(),
                "Linear API key".into()
            ]))
        );
        assert_eq!(
            KeyCommand::from_setting(&json!("bw get password 'Linear API key'")),
            Some(KeyCommand::Shell("bw get password 'Linear API key'".into()))
        );
        for empty in [
            json!(null),
            json!(""),
            json!([]),
            json!([""]),
            json!([1]),
            json!(3),
        ] {
            assert_eq!(KeyCommand::from_setting(&empty), None, "{empty}");
        }
    }

    #[tokio::test]
    async fn the_env_key_beats_the_command() {
        let dir = tempfile::tempdir().expect("dir");
        let marker = dir.path().join("ran");
        let resolver = KeyResolver::new(Some(" lin_env \n".into()));
        resolver
            .set_command(Some(script(
                dir.path(),
                &format!("touch '{}'; echo lin_cmd", marker.display()),
            )))
            .await;
        let (key, source) = resolver.key().await.expect("key");
        assert_eq!(key.expose(), "lin_env");
        assert_eq!(source, KeySource::Env);
        assert!(
            !marker.exists(),
            "the command must not run when the env key is set"
        );
        assert_eq!(resolver.status().await, KeyStatus::Env);
    }

    #[tokio::test]
    async fn command_output_is_trimmed_and_cached_for_the_process_lifetime() {
        let dir = tempfile::tempdir().expect("dir");
        let count = dir.path().join("count");
        let resolver = KeyResolver::new(None);
        resolver
            .set_command(Some(script(
                dir.path(),
                &format!(
                    "echo x >> '{}'; printf '  lin_cmd_key \\n\\n'",
                    count.display()
                ),
            )))
            .await;
        assert_eq!(resolver.status().await, KeyStatus::CommandPending);
        for _ in 0..3 {
            let (key, source) = resolver.key().await.expect("key");
            assert_eq!(key.expose(), "lin_cmd_key");
            assert_eq!(source, KeySource::Command);
        }
        let runs = std::fs::read_to_string(&count).expect("count");
        assert_eq!(runs.lines().count(), 1, "the command runs once");
        assert_eq!(resolver.status().await, KeyStatus::Command);
    }

    #[tokio::test]
    async fn a_shell_string_runs_through_sh() {
        let resolver = KeyResolver::new(None);
        resolver
            .set_command(Some(KeyCommand::Shell("echo lin_a | tr a b".into())))
            .await;
        assert_eq!(resolver.key().await.expect("key").0.expose(), "lin_b");
    }

    #[tokio::test]
    async fn a_failing_command_means_not_configured_and_leaks_nothing() {
        let dir = tempfile::tempdir().expect("dir");
        let resolver = KeyResolver::new(None);
        resolver
            .set_command(Some(script(
                dir.path(),
                "echo lin_partial_secret; echo 'vault is locked lin_err_secret' >&2; exit 3",
            )))
            .await;
        let error = resolver.key().await.expect_err("fails");
        assert!(matches!(error, LinearError::NotConfigured));
        let status = resolver.status().await;
        assert_eq!(
            status,
            KeyStatus::CommandFailed("exited with status 3".into())
        );
        assert!(!status.configured());
        for text in [
            format!("{error}"),
            format!("{error:?}"),
            format!("{status:?}"),
        ] {
            assert!(!text.contains("secret"), "leaked command output: {text}");
        }
    }

    #[tokio::test]
    async fn empty_output_and_a_missing_binary_are_not_configured() {
        let dir = tempfile::tempdir().expect("dir");
        let resolver = KeyResolver::new(None);
        resolver
            .set_command(Some(script(dir.path(), "printf '  \\n'")))
            .await;
        assert!(matches!(
            resolver.key().await,
            Err(LinearError::NotConfigured)
        ));
        resolver
            .set_command(Some(KeyCommand::Argv(vec!["/nonexistent/bw".into()])))
            .await;
        assert!(matches!(
            resolver.key().await,
            Err(LinearError::NotConfigured)
        ));
        assert!(matches!(
            resolver.status().await,
            KeyStatus::CommandFailed(reason) if reason.starts_with("could not start")
        ));
    }

    #[tokio::test]
    async fn without_env_or_command_there_is_no_key() {
        let resolver = KeyResolver::new(Some("   ".into()));
        assert!(matches!(
            resolver.key().await,
            Err(LinearError::NotConfigured)
        ));
        assert_eq!(resolver.status().await, KeyStatus::NotConfigured);
    }

    #[test]
    fn the_key_never_shows_in_debug_output() {
        let key = ApiKey::new("lin_api_supersecret");
        assert_eq!(format!("{key:?}"), "ApiKey([redacted])");
    }
}
