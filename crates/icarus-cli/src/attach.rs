//! `icarus attach …` and `icarus composer …`: context for the agent composer.

use std::io::Read as _;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum AttachCommand {
    /// Review threads of the workspace's pull request, one attachment per thread.
    Comments {
        workspace: String,
        /// Only these thread or comment ids (comma-separated or repeated).
        #[arg(long, value_delimiter = ',')]
        ids: Vec<String>,
        /// Skip resolved threads.
        #[arg(long)]
        unresolved: bool,
        /// Drop comments written by bots.
        #[arg(long)]
        humans_only: bool,
    },
    /// A check of the pull request, with the failed log excerpt for GitHub Actions runs.
    Check { workspace: String, name: String },
    /// A file, or a line range of it: `path`, `path:12` or `path:12-40`.
    File {
        workspace: String,
        target: String,
        #[arg(long)]
        start: Option<usize>,
        #[arg(long)]
        end: Option<usize>,
    },
    /// Free text; `-` or no text reads stdin.
    Text {
        workspace: String,
        title: String,
        text: Option<String>,
    },
    /// The diff of an agent turn (`latest`, a turn name or its commit).
    Turn {
        workspace: String,
        #[arg(default_value = "latest")]
        turn: String,
    },
}

#[derive(Subcommand)]
pub enum ComposerCommand {
    /// List the attachments waiting in a workspace's composer.
    Show {
        workspace: String,
        /// Include attachments already sent.
        #[arg(long)]
        all: bool,
    },
    /// Remove every attachment that has not been sent yet.
    Clear { workspace: String },
}

/// `path:12-40` → (`path`, 12, 40); a plain path keeps no range.
pub fn split_range(target: &str) -> (String, Option<usize>, Option<usize>) {
    if let Some((path, range)) = target.rsplit_once(':') {
        let (start, end) = match range.split_once('-') {
            Some((start, end)) => (start.parse().ok(), end.parse().ok()),
            None => (range.parse().ok(), None),
        };
        if let Some(start) = start
            && (end.is_some() || !range.contains('-'))
        {
            return (path.to_owned(), Some(start), end);
        }
    }
    (target.to_owned(), None, None)
}

fn read_stdin() -> Result<String> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("read stdin")?;
    Ok(text)
}

/// The server request for an attach command.
pub fn attach_request(command: &AttachCommand) -> Result<(&'static str, Value)> {
    Ok(match command {
        AttachCommand::Comments {
            workspace,
            ids,
            unresolved,
            humans_only,
        } => (
            "attach.pr_comments",
            json!({
                "workspace": workspace,
                "ids": if ids.is_empty() { Value::Null } else { json!(ids) },
                "unresolved": unresolved,
                "humans_only": humans_only,
            }),
        ),
        AttachCommand::Check { workspace, name } => (
            "attach.check",
            json!({ "workspace": workspace, "name": name }),
        ),
        AttachCommand::File {
            workspace,
            target,
            start,
            end,
        } => {
            let (path, parsed_start, parsed_end) = split_range(target);
            let path = std::path::Path::new(&path);
            // Why canonicalize only existing relative paths: the server resolves the rest against the worktree.
            let path = if path.is_relative() && path.exists() {
                std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
            } else {
                path.to_path_buf()
            };
            (
                "attach.file",
                json!({
                    "workspace": workspace,
                    "path": path,
                    "start_line": start.or(parsed_start),
                    "end_line": end.or(parsed_end),
                }),
            )
        }
        AttachCommand::Text {
            workspace,
            title,
            text,
        } => {
            let text = match text.as_deref() {
                None | Some("-") => read_stdin()?,
                Some(text) => text.to_owned(),
            };
            if text.trim().is_empty() {
                bail!("nothing to attach");
            }
            (
                "attach.text",
                json!({ "workspace": workspace, "title": title, "text": text }),
            )
        }
        AttachCommand::Turn { workspace, turn } => (
            "attach.turn",
            json!({ "workspace": workspace, "turn": turn }),
        ),
    })
}

fn delivery(delivered: &Value) -> String {
    match delivered.as_u64().unwrap_or(0) {
        0 => "queued; no Icarus window is open".to_owned(),
        1 => "sent to the open Icarus window".to_owned(),
        n => format!("sent to {n} Icarus windows"),
    }
}

pub fn attached(value: &Value) -> String {
    if let Some(list) = value["attachments"].as_array() {
        if list.is_empty() {
            return "no matching comments".to_owned();
        }
        let mut lines: Vec<String> = list
            .iter()
            .map(|a| {
                format!(
                    "{}  {}",
                    a["id"].as_str().unwrap_or("?"),
                    a["title"].as_str().unwrap_or("")
                )
            })
            .collect();
        lines.push(format!(
            "{} attachment(s), {}",
            list.len(),
            delivery(&value["delivered"])
        ));
        return lines.join("\n");
    }
    format!(
        "{}  {} ({})",
        value["id"].as_str().unwrap_or("?"),
        value["title"].as_str().unwrap_or(""),
        delivery(&value["delivered"])
    )
}

pub fn composer(value: &Value) -> String {
    let list = value.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "composer is empty".to_owned();
    }
    list.iter()
        .map(|a| {
            format!(
                "{}  {:<10}  {}{}",
                a["id"].as_str().unwrap_or("?"),
                a["kind"].as_str().unwrap_or(""),
                a["title"].as_str().unwrap_or(""),
                if a["sent_at"].is_null() {
                    ""
                } else {
                    "  (sent)"
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_parse_from_file_targets() {
        assert_eq!(split_range("src/a.rs"), ("src/a.rs".into(), None, None));
        assert_eq!(
            split_range("src/a.rs:12"),
            ("src/a.rs".into(), Some(12), None)
        );
        assert_eq!(
            split_range("src/a.rs:12-40"),
            ("src/a.rs".into(), Some(12), Some(40))
        );
        assert_eq!(split_range("odd:name"), ("odd:name".into(), None, None));
    }

    #[test]
    fn comment_requests_omit_empty_id_filters() {
        let (method, params) = attach_request(&AttachCommand::Comments {
            workspace: "ws".into(),
            ids: Vec::new(),
            unresolved: true,
            humans_only: false,
        })
        .expect("request");
        assert_eq!(method, "attach.pr_comments");
        assert_eq!(
            params,
            json!({ "workspace": "ws", "ids": null, "unresolved": true, "humans_only": false })
        );
    }
}
