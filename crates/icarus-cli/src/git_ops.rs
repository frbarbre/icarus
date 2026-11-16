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
    }
    Ok(())
}
