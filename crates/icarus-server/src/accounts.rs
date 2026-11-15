//! Several logins per agent provider (plan D13, §7c), done like Orca: each managed account is its own
//! config dir (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`) that the user signs in to with the provider's own
//! CLI. Icarus only picks which dir a launch uses; it never reads, copies or stores credentials.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use icarus_protocol::RpcError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::{AgentInfo, AgentOptions};
use crate::next_action::configured_command;
use crate::server::{Server, params, rpc};
use crate::state::Account;

/// The provider's own login, used when no managed account is chosen.
pub const SYSTEM: &str = "system";
pub const PROVIDERS: [&str; 2] = ["claude", "codex"];

/// Orca's list: a key or token in the environment would win over the account's login.
const CLAUDE_AUTH_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
];

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Env vars removed from a launch that uses a managed account.
pub fn auth_env_vars(provider: &str) -> &'static [&'static str] {
    match provider {
        "claude" => CLAUDE_AUTH_ENV,
        _ => &[],
    }
}

/// The env var that points the provider's CLI at an account's config dir.
pub fn config_env(provider: &str) -> Option<&'static str> {
    match provider {
        "claude" => Some("CLAUDE_CONFIG_DIR"),
        "codex" => Some("CODEX_HOME"),
        _ => None,
    }
}

/// Where managed homes live under the state dir, as in Orca's user data dir.
fn profiles_dir(provider: &str) -> &'static str {
    match provider {
        "codex" => "codex-accounts",
        _ => "claude-profiles",
    }
}

pub fn account_home(state_home: &Path, provider: &str, id: &str) -> PathBuf {
    state_home
        .join(profiles_dir(provider))
        .join(id)
        .join("home")
}

fn shell_quote(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@".contains(c))
    {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// What the user runs to sign in to the account, through the provider's own flow.
pub fn login_command(provider: &str, home: &Path, binary: &str) -> String {
    let var = config_env(provider).unwrap_or("HOME");
    let login = match provider {
        "codex" => "login",
        _ => "/login",
    };
    format!(
        "{var}={} {} {login}",
        shell_quote(&home.to_string_lossy()),
        shell_quote(binary)
    )
}

/// The newest rate-limit payload among the agents that ran on this login.
pub fn latest_usage(agents: &[AgentInfo], provider: &str, account: Option<&str>) -> Value {
    agents
        .iter()
        .filter(|a| a.provider == provider && a.account.as_deref() == account)
        .filter_map(|a| Some((a.rate_limits_at_ms?, a.rate_limits.as_ref()?, &a.id)))
        .max_by_key(|(at, _, _)| *at)
        .map_or(
            Value::Null,
            |(at, limits, id)| json!({ "rate_limits": limits, "at_ms": at, "agent_id": id }),
        )
}

/// The signed-in email from Claude Code's state file (no credentials live there).
fn claude_login_email(home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(home.join(".claude.json")).ok()?;
    let state: Value = serde_json::from_str(&text).ok()?;
    state["oauthAccount"]["emailAddress"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

fn default_key(provider: &str) -> String {
    format!("accounts.default.{provider}")
}

fn check_provider(provider: &str) -> Result<(), RpcError> {
    if PROVIDERS.contains(&provider) {
        Ok(())
    } else {
        Err(rpc(
            "invalid_params",
            format!("accounts exist for claude and codex, not {provider}"),
        ))
    }
}

impl Server {
    fn internal(error: anyhow::Error) -> RpcError {
        rpc("internal", format!("{error:#}"))
    }

    pub(crate) fn default_account(&self, provider: &str) -> Option<String> {
        self.store
            .kv_get(&default_key(provider))
            .ok()
            .flatten()
            .and_then(|v| v.as_str().map(str::to_owned))
    }

    pub(crate) fn find_account(&self, key: &str) -> Result<Account, RpcError> {
        self.store
            .account(key)
            .map_err(Self::internal)?
            .ok_or_else(|| rpc("not_found", format!("account {key}")))
    }

    /// Points the launch at the requested account, else the provider's default; `system` means none.
    pub(crate) fn apply_account(
        &self,
        options: &mut AgentOptions,
        requested: Option<&str>,
    ) -> Result<(), RpcError> {
        let Some(var) = config_env(&options.provider) else {
            return match requested {
                None | Some(SYSTEM) => Ok(()),
                Some(_) => Err(rpc(
                    "invalid_params",
                    format!("{} has no accounts", options.provider),
                )),
            };
        };
        let account = match requested.filter(|r| !r.is_empty()) {
            Some(SYSTEM) => None,
            Some(key) => Some(self.find_account(key)?),
            // A default whose account was removed falls back to the system login.
            None => self
                .default_account(&options.provider)
                .and_then(|id| self.store.account(&id).ok().flatten()),
        };
        let Some(account) = account else {
            return Ok(());
        };
        if account.provider != options.provider {
            return Err(rpc(
                "invalid_params",
                format!(
                    "account {} is for {}, not {}",
                    account.label, account.provider, options.provider
                ),
            ));
        }
        std::fs::create_dir_all(&account.home).map_err(|e| rpc("io", e))?;
        options
            .env
            .insert(var.to_owned(), account.home.to_string_lossy().into_owned());
        options.account = Some(account.id);
        Ok(())
    }

    fn login_binary(&self, provider: &str) -> String {
        configured_command(&self.resolved_settings(None), provider)
            .and_then(|command| command.into_iter().next())
            .unwrap_or_else(|| provider.to_owned())
    }

    fn account_json(&self, account: &Account, agents: &[AgentInfo]) -> Value {
        let mut value = serde_json::to_value(account).unwrap_or_default();
        let (logged_in, email) = match account.provider.as_str() {
            "claude" => {
                let email = claude_login_email(&account.home);
                (email.is_some(), email)
            }
            _ => (account.home.join("auth.json").is_file(), None),
        };
        value["managed"] = json!(true);
        value["default"] =
            json!(self.default_account(&account.provider).as_deref() == Some(&account.id));
        value["logged_in"] = json!(logged_in);
        value["email"] = json!(email);
        value["login_command"] = json!(login_command(
            &account.provider,
            &account.home,
            &self.login_binary(&account.provider)
        ));
        value["usage"] = latest_usage(agents, &account.provider, Some(&account.id));
        value
    }

    pub(crate) async fn accounts_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        match method {
            "account.list" => {
                #[derive(Deserialize)]
                struct P {
                    provider: Option<String>,
                }
                let p: P = params(p)?;
                let agents = self.agents.list();
                let accounts = self.store.accounts().map_err(Self::internal)?;
                let mut out = Vec::new();
                for provider in PROVIDERS {
                    if p.provider
                        .as_deref()
                        .is_some_and(|wanted| wanted != provider)
                    {
                        continue;
                    }
                    out.push(json!({
                        "id": SYSTEM,
                        "provider": provider,
                        "label": "System login",
                        "home": null,
                        "managed": false,
                        "default": self.default_account(provider).is_none(),
                        "usage": latest_usage(&agents, provider, None),
                    }));
                    for account in accounts.iter().filter(|a| a.provider == provider) {
                        out.push(self.account_json(account, &agents));
                    }
                }
                Ok(json!(out))
            }
            "account.add" => {
                #[derive(Deserialize)]
                struct P {
                    provider: String,
                    label: String,
                }
                let p: P = params(p)?;
                check_provider(&p.provider)?;
                let label = p.label.trim();
                if label.is_empty() || label == SYSTEM {
                    return Err(rpc("invalid_params", "a label is required"));
                }
                if self
                    .store
                    .accounts()
                    .map_err(Self::internal)?
                    .iter()
                    .any(|a| a.provider == p.provider && a.label == label)
                {
                    return Err(rpc(
                        "exists",
                        format!("{} already has an account {label}", p.provider),
                    ));
                }
                let id = format!("acct-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
                let home = account_home(&self.paths.home, &p.provider, &id);
                std::fs::create_dir_all(&home).map_err(|e| rpc("io", e))?;
                restrict(&home);
                let account = Account {
                    id,
                    provider: p.provider,
                    label: label.to_owned(),
                    home,
                    created_at_ms: now_ms(),
                };
                self.store.add_account(&account).map_err(Self::internal)?;
                Ok(self.account_json(&account, &self.agents.list()))
            }
            "account.remove" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    #[serde(default)]
                    keep_files: bool,
                }
                let p: P = params(p)?;
                let account = self.find_account(&p.id)?;
                self.store
                    .remove_account(&account.id)
                    .map_err(Self::internal)?;
                if self.default_account(&account.provider).as_deref() == Some(&account.id) {
                    let _ = self.store.kv_delete(&default_key(&account.provider));
                }
                let root = self.paths.home.join(profiles_dir(&account.provider));
                let dir = account.home.parent().map(Path::to_path_buf);
                let removed_files = match dir {
                    // Only a dir Icarus made; a sign-in under it goes with it.
                    Some(dir) if !p.keep_files && dir.starts_with(&root) && dir != root => {
                        std::fs::remove_dir_all(&dir).is_ok()
                    }
                    _ => false,
                };
                Ok(json!({ "removed": true, "removed_files": removed_files }))
            }
            "account.select" => {
                #[derive(Deserialize)]
                struct P {
                    provider: Option<String>,
                    id: Option<String>,
                }
                let p: P = params(p)?;
                match p.id.as_deref().filter(|id| *id != SYSTEM && !id.is_empty()) {
                    None => {
                        let provider = p
                            .provider
                            .ok_or_else(|| rpc("invalid_params", "provider is required"))?;
                        check_provider(&provider)?;
                        self.store
                            .kv_delete(&default_key(&provider))
                            .map_err(Self::internal)?;
                        Ok(json!({ "provider": provider, "default": SYSTEM }))
                    }
                    Some(key) => {
                        let account = self.find_account(key)?;
                        if p.provider.as_deref().is_some_and(|w| w != account.provider) {
                            return Err(rpc(
                                "invalid_params",
                                format!("account {} is for {}", account.label, account.provider),
                            ));
                        }
                        self.store
                            .kv_set(&default_key(&account.provider), &json!(account.id))
                            .map_err(Self::internal)?;
                        Ok(json!({ "provider": account.provider, "default": account.id }))
                    }
                }
            }
            other => Err(rpc("unknown_method", other)),
        }
    }
}

fn restrict(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for path in [Some(dir), dir.parent()].into_iter().flatten() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentState;

    #[test]
    fn homes_and_login_commands_follow_orca_and_the_providers_own_login() {
        let home = account_home(Path::new("/s"), "claude", "acct-1");
        assert_eq!(home, Path::new("/s/claude-profiles/acct-1/home"));
        assert_eq!(
            login_command("claude", &home, "claude"),
            "CLAUDE_CONFIG_DIR=/s/claude-profiles/acct-1/home claude /login"
        );
        let codex = account_home(Path::new("/my state"), "codex", "acct-2");
        assert_eq!(
            login_command("codex", &codex, "codex"),
            "CODEX_HOME='/my state/codex-accounts/acct-2/home' codex login"
        );
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert!(auth_env_vars("claude").contains(&"ANTHROPIC_API_KEY"));
        assert!(auth_env_vars("codex").is_empty());
    }

    fn agent(id: &str, account: Option<&str>, at: Option<u64>) -> AgentInfo {
        AgentInfo {
            id: id.into(),
            provider: "claude".into(),
            title: None,
            workspace: None,
            cwd: PathBuf::from("/w"),
            state: AgentState::Idle,
            provider_session: None,
            model: None,
            turns: 0,
            created_at_ms: 0,
            last_activity_ms: 0,
            next_seq: 0,
            rate_limits: at.map(|at| json!({ "at": at })),
            rate_limits_at_ms: at,
            account: account.map(str::to_owned),
        }
    }

    #[test]
    fn usage_is_the_newest_rate_limit_of_the_accounts_agents() {
        let agents = [
            agent("a1", Some("acct-1"), Some(10)),
            agent("a2", Some("acct-1"), Some(30)),
            agent("a3", Some("acct-1"), None),
            agent("a4", None, Some(50)),
        ];
        let usage = latest_usage(&agents, "claude", Some("acct-1"));
        assert_eq!(usage["agent_id"], "a2");
        assert_eq!(usage["rate_limits"]["at"], 30);
        assert_eq!(latest_usage(&agents, "claude", None)["agent_id"], "a4");
        assert!(latest_usage(&agents, "codex", None).is_null());
    }
}
