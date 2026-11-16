//! `icarus git …`: the Source Control actions, run by the server in the workspace's worktree.

use std::path::Path;

use anyhow::{Result, bail};
use clap::Subcommand;
use icarus_client::Client;
use serde_json::{Value, json};

use crate::notes::target;

#[derive(Subcommand)]
pub enum GitCommand {
    /// Branch, upstream, and how far ahead/behind it is.
    Status,
    /// Throw away the uncommitted change of each path (committed branch changes stay); untracked
    /// files are deleted.
    Discard {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    Stage {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    Unstage {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Commit what's staged, or every change when nothing is staged. Commit hooks run.
    Commit {
        /// The message; `--amend` without one keeps the previous message.
        #[arg(short, long)]
        message: Option<String>,
        #[arg(long)]
        amend: bool,
        /// Stage everything first, even when something is already staged.
        #[arg(short, long)]
        all: bool,
    },
    /// Push the branch, publishing it if it has no upstream yet.
    Push {
        #[arg(long)]
        force_with_lease: bool,
    },
    Pull,
    /// Pull, then push.
    Sync,
    /// The commits on the workspace branch since its base, newest first.
    Commits {
        /// Compare against this ref instead of the workspace's base.
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
}

fn relative_age(seconds: i64) -> String {
    let minutes = seconds.max(0) / 60;
    match minutes {
        0 => "just now".to_string(),
        1..=59 => format!("{minutes}m ago"),
        60..=1439 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1440),
    }
}

/// `abc1234  Fix login  (Ada, 3h ago)` per commit, or why there are none.
fn commits_text(value: &Value, now: i64) -> String {
    let commits = value["commits"].as_array().cloned().unwrap_or_default();
    if commits.is_empty() {
        let compare = &value["compare"];
        return match compare["status"].as_str() {
            Some("ready") => format!(
                "No commits since {}",
                compare["base"].as_str().unwrap_or("the base")
            ),
            Some("no_base") => "No base branch to compare against".to_string(),
            Some("invalid_base") => format!(
                "The base {} doesn't exist",
                compare["base"].as_str().unwrap_or_default()
            ),
            Some("no_merge_base") => "The branch shares no history with its base".to_string(),
            _ => "No commits yet".to_string(),
        };
    }
    let mut lines: Vec<String> = commits
        .iter()
        .map(|commit| {
            format!(
                "{}  {}  ({}, {})",
                commit["short"].as_str().unwrap_or_default(),
                commit["subject"].as_str().unwrap_or_default(),
                commit["author"].as_str().unwrap_or_default(),
                relative_age(now - commit["time"].as_i64().unwrap_or(now))
            )
        })
        .collect();
    if value["truncated"].as_bool() == Some(true) {
        lines.push("… older commits not shown (raise --limit)".to_string());
    }
    lines.join("\n")
}

/// Paths given on the command line, made absolute so the server can place them in the worktree.
fn absolute_paths(paths: &[String]) -> Vec<String> {
    let cwd = std::env::current_dir().ok();
    paths
        .iter()
        .map(|path| match (&cwd, Path::new(path).is_relative()) {
            (Some(cwd), true) => cwd.join(path).to_string_lossy().into_owned(),
            _ => path.clone(),
        })
        .collect()
}

fn with_paths(workspace: Option<&str>, paths: &[String]) -> Value {
    let mut request = target(workspace);
    request["paths"] = json!(absolute_paths(paths));
    request
}

fn sync_text(state: &Value) -> String {
    let branch = state["branch"].as_str().unwrap_or("(detached)");
    match state["upstream"].as_str() {
        Some(upstream) => format!(
            "{branch} → {upstream}  ↑{} ↓{}",
            state["ahead"].as_u64().unwrap_or(0),
            state["behind"].as_u64().unwrap_or(0)
        ),
        None => format!("{branch} (not published)"),
    }
}

pub async fn run(
    cli: &super::Cli,
    client: &Client,
    workspace: Option<&str>,
    command: &GitCommand,
) -> Result<()> {
    match command {
        GitCommand::Status => {
            let state = client.request("git.status", target(workspace)).await?;
            super::print(cli, &state, sync_text);
        }
        GitCommand::Discard { paths } => {
            let done = client
                .request("git.discard", with_paths(workspace, paths))
                .await?;
            super::print(cli, &done, |v| {
                v["discarded"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|d| {
                        format!(
                            "{}: {}",
                            d["label"].as_str().unwrap_or("discarded"),
                            d["path"].as_str().unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        GitCommand::Stage { paths } | GitCommand::Unstage { paths } => {
            let method = if matches!(command, GitCommand::Stage { .. }) {
                "git.stage"
            } else {
                "git.unstage"
            };
            let value = client.request(method, with_paths(workspace, paths)).await?;
            super::print(cli, &value, |_| String::new());
        }
        GitCommand::Commit {
            message,
            amend,
            all,
        } => {
            if message.is_none() && !amend {
                bail!("give a message with -m (or --amend to keep the last one)");
            }
            let mut request = target(workspace);
            request["message"] = json!(message.clone().unwrap_or_default());
            request["amend"] = json!(amend);
            request["all"] = json!(all);
            let outcome = client.request("git.commit", request).await?;
            super::print(cli, &outcome, |v| {
                format!(
                    "{} {}",
                    v["oid"]
                        .as_str()
                        .unwrap_or_default()
                        .get(..8)
                        .unwrap_or_default(),
                    v["summary"].as_str().unwrap_or_default()
                )
            });
        }
        GitCommand::Push { force_with_lease } => {
            let mut request = target(workspace);
            request["force_with_lease"] = json!(force_with_lease);
            let state = client.request("git.push", request).await?;
            super::print(cli, &state, sync_text);
        }
        GitCommand::Pull => {
            let state = client.request("git.pull", target(workspace)).await?;
            super::print(cli, &state, sync_text);
        }
        GitCommand::Sync => {
            let state = client.request("git.sync", target(workspace)).await?;
            super::print(cli, &state, sync_text);
        }
        GitCommand::Commits { base, limit } => {
            let mut request = target(workspace);
            if let Some(base) = base {
                request["base"] = json!(base);
            }
            if let Some(limit) = limit {
                request["limit"] = json!(limit);
            }
            let commits = client.request("git.commits", request).await?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs() as i64)
                .unwrap_or_default();
            super::print(cli, &commits, |value| commits_text(value, now));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commits_read_newest_first_with_author_and_age() {
        let value = json!({
            "compare": { "status": "ready", "base": "origin/main" },
            "commits": [
                { "oid": "b".repeat(40), "short": "bbbbbbb", "subject": "Fix login", "author": "Ada", "time": 1_000 },
                { "oid": "a".repeat(40), "short": "aaaaaaa", "subject": "Start", "author": "Bo", "time": 1_000 - 2 * 3600 },
            ],
            "truncated": true,
        });
        assert_eq!(
            commits_text(&value, 1_000 + 180),
            "bbbbbbb  Fix login  (Ada, 3m ago)\naaaaaaa  Start  (Bo, 2h ago)\n… older commits not shown (raise --limit)"
        );
        assert_eq!(
            commits_text(
                &json!({ "compare": { "status": "ready", "base": "origin/main" }, "commits": [] }),
                0
            ),
            "No commits since origin/main"
        );
        assert_eq!(
            commits_text(
                &json!({ "compare": { "status": "invalid_base", "base": "origin/x" }, "commits": [] }),
                0
            ),
            "The base origin/x doesn't exist"
        );
    }
}
