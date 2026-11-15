//! `icarus notifications`, `icarus account`, and `icarus agent history|resume`.

use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use super::{Cli, connect, print};

#[derive(Subcommand)]
pub enum NotificationsCommand {
    /// Mark one notification read, or all of them.
    Read {
        #[arg(required_unless_present = "all")]
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
    /// Delete every notification.
    Clear,
}

#[derive(Subcommand)]
pub enum AccountCommand {
    /// Accounts per provider, with the default marked and the latest usage meters.
    List {
        #[arg(long)]
        provider: Option<String>,
    },
    /// Make a new account and print the command that signs in to it.
    Add {
        /// claude or codex.
        provider: String,
        label: String,
    },
    /// Forget an account and delete its config dir (unless --keep-files).
    Remove {
        id: String,
        #[arg(long)]
        keep_files: bool,
    },
    /// Use this account for new sessions; `system` (with --provider) goes back to the provider's own login.
    Use {
        id: String,
        #[arg(long)]
        provider: Option<String>,
    },
}

fn ago(at_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    let secs = now.saturating_sub(at_ms) / 1000;
    match secs {
        0..60 => format!("{secs}s ago"),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86_400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

fn notifications_text(v: &Value) -> String {
    let items = v.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return "no notifications".into();
    }
    items
        .iter()
        .map(|n| {
            let mark = if n["read"] == json!(true) { " " } else { "*" };
            let mut line = format!(
                "{mark} {}  {}  {}  {}",
                n["id"].as_str().unwrap_or_default(),
                ago(n["created_at"].as_u64().unwrap_or_default()),
                n["kind"].as_str().unwrap_or_default(),
                n["title"].as_str().unwrap_or_default(),
            );
            if let Some(body) = n["body"].as_str().filter(|b| !b.is_empty()) {
                line.push_str(&format!("\n    {}", body.replace('\n', " ")));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn notifications(
    cli: &Cli,
    unread: bool,
    limit: usize,
    command: Option<&NotificationsCommand>,
) -> Result<()> {
    let client = connect(cli).await?;
    match command {
        None => {
            let value = client
                .request(
                    "notification.list",
                    json!({ "unread_only": unread, "limit": limit }),
                )
                .await?;
            print(cli, &value, notifications_text);
        }
        Some(NotificationsCommand::Read { id, all }) => {
            let value = client
                .request("notification.read", json!({ "id": id, "all": all }))
                .await?;
            print(cli, &value, |v| {
                format!("marked {} read", v["updated"].as_u64().unwrap_or(0))
            });
        }
        Some(NotificationsCommand::Clear) => {
            let value = client.request("notification.clear", json!({})).await?;
            print(cli, &value, |v| {
                format!("cleared {}", v["cleared"].as_u64().unwrap_or(0))
            });
        }
    }
    Ok(())
}

fn percent(window: &Value) -> Option<String> {
    let used = window["utilization"].as_f64()?;
    Some(format!("{:.0}%", used * 100.0))
}

/// Claude Code's `rate_limit_event` payload: the session and weekly windows.
fn usage_text(usage: &Value) -> String {
    if usage.is_null() {
        return "no usage seen yet".into();
    }
    let windows = &usage["rate_limits"]["unifiedWindows"];
    let mut parts = Vec::new();
    for (key, label) in [("five_hour", "session"), ("seven_day", "week")] {
        if let Some(text) = percent(&windows[key]) {
            parts.push(format!("{label} {text}"));
        }
    }
    if parts.is_empty() {
        parts.push(
            usage["rate_limits"]["status"]
                .as_str()
                .unwrap_or("usage seen")
                .to_owned(),
        );
    }
    format!(
        "{} ({})",
        parts.join(", "),
        ago(usage["at_ms"].as_u64().unwrap_or_default())
    )
}

fn accounts_text(v: &Value) -> String {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|a| {
            let mark = if a["default"] == json!(true) {
                "*"
            } else {
                " "
            };
            let login = match (a["managed"].as_bool(), a["email"].as_str()) {
                (Some(false), _) => String::new(),
                (_, Some(email)) => format!("  {email}"),
                _ if a["logged_in"] == json!(true) => "  signed in".into(),
                _ => "  not signed in".into(),
            };
            format!(
                "{mark} {:<7} {}  {}{login}  {}",
                a["provider"].as_str().unwrap_or_default(),
                a["id"].as_str().unwrap_or_default(),
                a["label"].as_str().unwrap_or_default(),
                usage_text(&a["usage"]),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn account(cli: &Cli, command: &AccountCommand) -> Result<()> {
    let client = connect(cli).await?;
    match command {
        AccountCommand::List { provider } => {
            let value = client
                .request("account.list", json!({ "provider": provider }))
                .await?;
            print(cli, &value, accounts_text);
        }
        AccountCommand::Add { provider, label } => {
            let value = client
                .request(
                    "account.add",
                    json!({ "provider": provider, "label": label }),
                )
                .await?;
            print(cli, &value, |v| {
                format!(
                    "added {} ({})\nsign in with:\n  {}",
                    v["label"].as_str().unwrap_or_default(),
                    v["id"].as_str().unwrap_or_default(),
                    v["login_command"].as_str().unwrap_or_default()
                )
            });
        }
        AccountCommand::Remove { id, keep_files } => {
            let value = client
                .request(
                    "account.remove",
                    json!({ "id": id, "keep_files": keep_files }),
                )
                .await?;
            print(cli, &value, |_| String::new());
        }
        AccountCommand::Use { id, provider } => {
            if id == "system" && provider.is_none() {
                bail!("`icarus account use system` needs --provider claude|codex");
            }
            let value = client
                .request("account.select", json!({ "id": id, "provider": provider }))
                .await?;
            print(cli, &value, |v| {
                format!(
                    "{} now uses {}",
                    v["provider"].as_str().unwrap_or_default(),
                    v["default"].as_str().unwrap_or_default()
                )
            });
        }
    }
    Ok(())
}

fn history_text(v: &Value) -> String {
    let items = v.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return "no past sessions".into();
    }
    items
        .iter()
        .map(|s| {
            let mut line = format!(
                "{}  {}  {}  {}  {} turn(s)  {}",
                s["id"].as_str().unwrap_or_default(),
                ago(s["last_activity_ms"].as_u64().unwrap_or_default()),
                s["state"].as_str().unwrap_or_default(),
                s["provider"].as_str().unwrap_or_default(),
                s["turns"],
                s["title"].as_str().unwrap_or("-"),
            );
            if let Some(prompt) = s["first_prompt"].as_str() {
                line.push_str(&format!("\n    > {}", prompt.replace('\n', " ")));
            }
            if let Some(snippet) = s["snippet"].as_str() {
                line.push_str(&format!("\n    … {snippet} …"));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn history(
    cli: &Cli,
    workspace: Option<&str>,
    search: Option<&str>,
    limit: usize,
    before: Option<u64>,
) -> Result<()> {
    let client = connect(cli).await?;
    let value = client
        .request(
            "agent.history",
            json!({ "workspace": workspace, "query": search, "limit": limit, "before": before }),
        )
        .await?;
    print(cli, &value, history_text);
    Ok(())
}

pub async fn resume(cli: &Cli, id: &str) -> Result<()> {
    let client = connect(cli).await?;
    let value = client.request("agent.resume", json!({ "id": id })).await?;
    print(cli, &value, |v| {
        format!(
            "{}  {}",
            v["id"].as_str().unwrap_or_default(),
            v["state"].as_str().unwrap_or_default()
        )
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_shows_the_session_and_week_windows() {
        let usage = json!({ "at_ms": 0, "rate_limits": { "unifiedWindows": {
            "five_hour": { "utilization": 0.25 }, "seven_day": { "utilization": 0.5 } } } });
        assert!(usage_text(&usage).starts_with("session 25%, week 50% ("));
        assert_eq!(usage_text(&Value::Null), "no usage seen yet");
    }
}
