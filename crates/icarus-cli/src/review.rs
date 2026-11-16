use std::io::Read as _;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use icarus_client::Client;
use serde_json::{Value, json};

#[derive(Args, Clone)]
pub struct At {
    /// Workspace id, name or path (defaults to the one containing the current directory).
    #[arg(long, short = 'w', global = true)]
    workspace: Option<String>,
    /// Review this pull request number instead of the workspace branch's PR.
    #[arg(long, global = true)]
    pr: Option<u64>,
}

#[derive(Args, Clone)]
pub struct Body {
    /// Markdown body.
    #[arg(long, conflicts_with = "body_file")]
    body: Option<String>,
    /// Read the markdown body from a file (`-` for stdin).
    #[arg(long, value_name = "PATH")]
    body_file: Option<String>,
}

impl Body {
    fn read(&self) -> Result<Option<String>> {
        match (&self.body, self.body_file.as_deref()) {
            (Some(body), _) => Ok(Some(body.clone())),
            (None, Some("-")) => {
                let mut text = String::new();
                std::io::stdin().read_to_string(&mut text)?;
                Ok(Some(text))
            }
            (None, Some(path)) => Ok(Some(
                std::fs::read_to_string(path).with_context(|| format!("read {path}"))?,
            )),
            (None, None) => Ok(None),
        }
    }

    fn required(&self) -> Result<String> {
        match self.read()? {
            Some(body) => Ok(body),
            None => bail!("pass --body or --body-file"),
        }
    }
}

#[derive(Subcommand)]
pub enum ReviewCommand {
    /// The review of the workspace's pull request: standing, base, drafts, summary and verdict.
    Show {
        /// Skip the short-lived PR cache.
        #[arg(long)]
        refresh: bool,
    },
    /// Show the diff base options, or pick one: since-review, whole, commit:<sha> or review:<sha>.
    Base { pick: Option<String> },
    /// Draft comments queued for the review.
    Comment {
        #[command(subcommand)]
        command: CommentCommand,
    },
    /// Show, set or clear the review summary (and the verdict to submit with).
    Summary {
        #[command(flatten)]
        body: Body,
        #[arg(long)]
        clear: bool,
        /// comment, approve or request-changes.
        #[arg(long)]
        verdict: Option<String>,
    },
    /// Submit one review with every queued comment: comment, approve or request-changes.
    Submit {
        /// Defaults to the verdict saved with `review summary --verdict`.
        verdict: Option<String>,
        /// Defaults to the saved summary.
        #[command(flatten)]
        body: Body,
    },
    /// Review threads on the pull request, with resolved and outdated state mapped to current lines.
    Threads {
        /// Only threads on this file.
        #[arg(long)]
        file: Option<String>,
        #[arg(long)]
        unresolved: bool,
    },
    /// Reply in a review thread (`PRRT_…` id from `review threads`).
    Reply {
        thread: String,
        #[command(flatten)]
        body: Body,
    },
    /// Resolve a review thread.
    Resolve { thread: String },
    /// Unresolve a review thread.
    Unresolve { thread: String },
    /// React to a comment (its `PRRC_…`/node id): +1, -1, laugh, confused, heart, hooray, rocket, eyes.
    React {
        subject: String,
        reaction: String,
        #[arg(long)]
        remove: bool,
    },
    /// Upload an image or video as a GitHub attachment and print the markdown to put in a comment.
    Upload { file: String },
    /// People who can be @-mentioned or asked for a review (the repository's assignable users).
    People {
        #[arg(long)]
        refresh: bool,
    },
}

/// A published comment's reference: node id, or the numeric id of a PR conversation comment.
fn published(kind: &str, node_id: &str) -> Value {
    match (kind, node_id.parse::<u64>()) {
        ("issue_comment", Ok(id)) => json!({ "kind": kind, "id": id }),
        _ => json!({ "kind": kind, "node_id": node_id }),
    }
}

#[derive(Subcommand)]
pub enum CommentCommand {
    /// Queue a draft comment on a line or range (a file comment when no --line, or when outside the diff).
    Add {
        /// Repository-relative path.
        #[arg(long)]
        file: String,
        /// The line, or the first line of a range with --end-line.
        #[arg(long)]
        line: Option<u32>,
        #[arg(long)]
        end_line: Option<u32>,
        /// right (the new file, default) or left (the old file).
        #[arg(long)]
        side: Option<String>,
        /// Post it on GitHub right away instead of queueing it for the review ("Comment now").
        #[arg(long)]
        now: bool,
        #[command(flatten)]
        body: Body,
    },
    /// List the queued drafts.
    List,
    /// Edit one of your comments already on GitHub (review comment node id, `PRRC_…`).
    EditPublished {
        node_id: String,
        /// review_comment (default), review_summary, or issue_comment (pass the numeric id).
        #[arg(long, default_value = "review_comment")]
        kind: String,
        #[command(flatten)]
        body: Body,
    },
    /// Delete one of your comments already on GitHub.
    DeletePublished {
        node_id: String,
        #[arg(long, default_value = "review_comment")]
        kind: String,
    },
    /// Replace a draft's body.
    Edit {
        id: String,
        #[command(flatten)]
        body: Body,
    },
    /// Delete a draft.
    Rm { id: String },
    /// Delete every draft.
    Clear,
}

fn workspace(at: &At) -> Result<String> {
    match &at.workspace {
        Some(workspace) => Ok(workspace.clone()),
        None => Ok(std::env::current_dir()?.to_string_lossy().into_owned()),
    }
}

fn base_params(at: &At) -> Result<Value> {
    Ok(json!({ "workspace": workspace(at)?, "pr": at.pr }))
}

fn with(mut base: Value, extra: Value) -> Value {
    for (key, value) in extra.as_object().into_iter().flatten() {
        base[key] = value.clone();
    }
    base
}

pub async fn run(
    cli: &super::Cli,
    client: &Client,
    at: &At,
    command: &ReviewCommand,
) -> Result<()> {
    let timeout = Duration::from_secs(120);
    let call = |method: &'static str, params: Value| async move {
        client.request_timeout(method, params, timeout).await
    };
    let base = base_params(at)?;
    match command {
        ReviewCommand::Show { refresh } => {
            let value = call("review.get", with(base, json!({ "refresh": refresh }))).await?;
            super::print(cli, &value, show_text);
        }
        ReviewCommand::Base { pick: None } => {
            let value = call("review.get", base).await?;
            super::print(cli, &value["base"], base_text);
        }
        ReviewCommand::Base { pick: Some(pick) } => {
            let value = call("review.set_base", with(base, json!({ "base": pick }))).await?;
            super::print(cli, &value, |v| {
                let mut text = format!(
                    "base: {} → {} ({})",
                    v["pick"].as_str().unwrap_or_default(),
                    v["base_ref"].as_str().unwrap_or_default(),
                    v["kind"].as_str().unwrap_or_default()
                );
                if v["conflicted"] == true {
                    text.push_str("\nreplaying your reviewed version hit conflicts; files with conflict markers were touched by both the rebase and the new commits");
                }
                if v["fell_back"] == true {
                    text.push_str("\nthe reviewed commit is not available, so this shows the whole pull request");
                }
                text
            });
        }
        ReviewCommand::Comment { command } => match command {
            CommentCommand::Add {
                file,
                line,
                end_line,
                side,
                now,
                body,
            } => {
                let (start_line, line) = match (line, end_line) {
                    (Some(start), Some(end)) => (Some(*start), Some(*end)),
                    (Some(line), None) => (None, Some(*line)),
                    (None, Some(_)) => bail!("--end-line needs --line"),
                    (None, None) => (None, None),
                };
                let params = with(
                    base,
                    json!({ "path": file, "line": line, "start_line": start_line, "side": side,
                        "file": line.is_none(), "body": body.required()? }),
                );
                if *now {
                    let value = call("review.comment.post", params).await?;
                    super::print(cli, &value, |v| {
                        let mut text = format!(
                            "posted {}",
                            v["comment"]["url"].as_str().unwrap_or("the comment")
                        );
                        if let Some(note) = v["note"].as_str() {
                            text.push('\n');
                            text.push_str(note);
                        }
                        text
                    });
                    return Ok(());
                }
                let value = call("review.comment.add", params).await?;
                super::print(cli, &value, |v| {
                    let mut text = format!(
                        "queued {} ({} draft(s) in this review)",
                        draft_line(&v["draft"]),
                        v["drafts"]
                    );
                    if let Some(note) = v["note"].as_str() {
                        text.push('\n');
                        text.push_str(note);
                    }
                    text
                });
            }
            CommentCommand::List => {
                let value = call("review.comment.list", base).await?;
                super::print(cli, &value, |v| drafts_text(&v["drafts"]));
            }
            CommentCommand::Edit { id, body } => {
                let value = call(
                    "review.comment.edit",
                    with(base, json!({ "id": id, "body": body.required()? })),
                )
                .await?;
                super::print(cli, &value, |v| {
                    format!("edited {}", draft_line(&v["draft"]))
                });
            }
            CommentCommand::Rm { id } => {
                let value = call("review.comment.remove", with(base, json!({ "id": id }))).await?;
                super::print(cli, &value, |v| {
                    format!("removed {id}; {} draft(s) left", v["drafts"])
                });
            }
            CommentCommand::Clear => {
                let value = call("review.comment.clear", base).await?;
                super::print(cli, &value, |v| {
                    format!("cleared {} draft(s)", v["cleared"])
                });
            }
            CommentCommand::EditPublished {
                node_id,
                kind,
                body,
            } => {
                let mut params = with(base, published(kind, node_id));
                params["body"] = json!(body.required()?);
                let value = call("review.comment.edit_published", params).await?;
                super::print(cli, &value, |_| format!("edited {node_id}"));
            }
            CommentCommand::DeletePublished { node_id, kind } => {
                let value = call(
                    "review.comment.delete_published",
                    with(base, published(kind, node_id)),
                )
                .await?;
                super::print(cli, &value, |_| format!("deleted {node_id}"));
            }
        },
        ReviewCommand::People { refresh } => {
            let value = call(
                "review.mentionables",
                with(base, json!({ "refresh": refresh })),
            )
            .await?;
            super::print(cli, &value, |v| {
                v["users"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|user| match user["name"].as_str() {
                        Some(name) => format!("{}  {name}", user["login"].as_str().unwrap_or("")),
                        None => user["login"].as_str().unwrap_or("").to_owned(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        ReviewCommand::Summary {
            body,
            clear,
            verdict,
        } => {
            let text = if *clear {
                Some(String::new())
            } else {
                body.read()?
            };
            let value = if text.is_none() && verdict.is_none() {
                call("review.get", base).await?
            } else {
                call(
                    "review.set_summary",
                    with(base, json!({ "body": text, "verdict": verdict })),
                )
                .await?
            };
            super::print(cli, &value, |v| {
                format!(
                    "verdict: {}\n{}",
                    v["verdict"].as_str().unwrap_or("(none)"),
                    v["summary"].as_str().unwrap_or_default()
                )
            });
        }
        ReviewCommand::Submit { verdict, body } => {
            let value = call(
                "review.submit",
                with(base, json!({ "verdict": verdict, "body": body.read()? })),
            )
            .await?;
            super::print(cli, &value, |v| {
                format!(
                    "{} {} comment(s)\n{}",
                    v["message"].as_str().unwrap_or("Review submitted."),
                    v["comments"],
                    v["url"].as_str().unwrap_or_default()
                )
            });
        }
        ReviewCommand::Threads { file, unresolved } => {
            let value = call(
                "review.threads",
                with(base, json!({ "path": file, "unresolved": unresolved })),
            )
            .await?;
            super::print(cli, &value, threads_text);
        }
        ReviewCommand::Reply { thread, body } => {
            let value = call(
                "review.reply",
                json!({ "workspace": at.workspace, "thread": thread, "body": body.required()? }),
            )
            .await?;
            super::print(cli, &value, |v| {
                format!(
                    "replied in {thread}: {}",
                    v["comment"]["url"].as_str().unwrap_or_default()
                )
            });
        }
        ReviewCommand::Resolve { thread } | ReviewCommand::Unresolve { thread } => {
            let method = if matches!(command, ReviewCommand::Resolve { .. }) {
                "review.resolve"
            } else {
                "review.unresolve"
            };
            let value = call(
                method,
                json!({ "workspace": at.workspace, "thread": thread }),
            )
            .await?;
            super::print(cli, &value, |v| {
                format!(
                    "{thread} is {}",
                    if v["resolved"] == true {
                        "resolved"
                    } else {
                        "open"
                    }
                )
            });
        }
        ReviewCommand::React {
            subject,
            reaction,
            remove,
        } => {
            let value = call(
                "review.react",
                json!({ "workspace": at.workspace, "subject": subject, "reaction": reaction, "remove": remove }),
            )
            .await?;
            super::print(cli, &value, |v| {
                format!(
                    "{} {} on {subject}",
                    if v["reacted"] == true {
                        "added"
                    } else {
                        "removed"
                    },
                    v["reaction"].as_str().unwrap_or_default()
                )
            });
        }
        ReviewCommand::Upload { file } => {
            let path = std::fs::canonicalize(file).with_context(|| format!("find {file}"))?;
            let value = client
                .request_timeout(
                    "review.upload",
                    with(base, json!({ "path": path })),
                    Duration::from_secs(660),
                )
                .await?;
            super::print(cli, &value, |v| {
                v["markdown"].as_str().unwrap_or_default().trim().to_owned()
            });
        }
    }
    Ok(())
}

fn draft_line(draft: &Value) -> String {
    let path = draft["path"].as_str().unwrap_or_default();
    let place = match (draft["start_line"].as_u64(), draft["line"].as_u64()) {
        (Some(start), Some(end)) => format!("{path}:{start}-{end}"),
        (None, Some(line)) => format!("{path}:{line}"),
        _ => format!("{path} (file)"),
    };
    let side = if draft["side"] == "LEFT" {
        " [old]"
    } else {
        ""
    };
    format!(
        "{}  {place}{side}",
        draft["id"].as_str().unwrap_or_default()
    )
}

fn first_line(text: &str) -> &str {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
}

fn drafts_text(drafts: &Value) -> String {
    let drafts = drafts.as_array().cloned().unwrap_or_default();
    if drafts.is_empty() {
        return "no draft comments".to_owned();
    }
    drafts
        .iter()
        .map(|d| {
            format!(
                "{}\n    {}",
                draft_line(d),
                first_line(d["body"].as_str().unwrap_or_default())
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn base_text(base: &Value) -> String {
    let selected = base["selected"].as_str().unwrap_or_default();
    let mut lines = vec![format!(
        "base: {selected} (Changes read against {})",
        base["applied_ref"].as_str().unwrap_or("?")
    )];
    for option in base["options"].as_array().into_iter().flatten() {
        let value = option["value"].as_str().unwrap_or_default();
        let marker = if value == selected { "*" } else { " " };
        let tags: Vec<&str> = option["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let unavailable = if option["available"] == false {
            "  (not available)"
        } else {
            ""
        };
        let tags = if tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", tags.join(", "))
        };
        lines.push(format!(
            "{marker} {}{tags}{unavailable}\n    {value}",
            option["label"].as_str().unwrap_or_default()
        ));
    }
    if let Some(error) = base["error"].as_str() {
        lines.push(format!("error: {error}"));
    }
    lines.join("\n")
}

fn show_text(value: &Value) -> String {
    let pr = &value["pr"];
    let mut lines = vec![format!(
        "{}#{} {}",
        pr["repo"].as_str().unwrap_or_default(),
        pr["number"],
        pr["title"].as_str().unwrap_or_default()
    )];
    if let Some(error) = value["context_error"].as_str() {
        lines.push(format!("GitHub: {error}"));
    }
    lines.push(format!(
        "standing: {}{}",
        value["standing"].as_str().unwrap_or("none"),
        if value["viewer_did_author"] == true {
            " (your pull request: only comment)"
        } else {
            ""
        }
    ));
    lines.push(base_text(&value["base"]));
    lines.push(String::new());
    lines.push(drafts_text(&value["drafts"]));
    let summary = value["summary"].as_str().unwrap_or_default();
    if !summary.trim().is_empty() {
        lines.push(format!("summary: {}", first_line(summary)));
    }
    if let Some(verdict) = value["verdict"].as_str() {
        lines.push(format!("verdict: {verdict}"));
    }
    lines.join("\n")
}

fn threads_text(value: &Value) -> String {
    let threads = value["threads"].as_array().cloned().unwrap_or_default();
    if threads.is_empty() {
        return "no review threads".to_owned();
    }
    threads
        .iter()
        .map(|t| {
            let state = if t["is_resolved"] == true {
                "resolved"
            } else {
                "open"
            };
            let mut text = format!(
                "{}  {} {}  [{state}]",
                t["id"].as_str().unwrap_or_default(),
                t["path"].as_str().unwrap_or_default(),
                t["placement"]["label"].as_str().unwrap_or_default()
            );
            for comment in t["comments"].as_array().into_iter().flatten() {
                let reactions: Vec<String> = comment["reactions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|r| {
                        format!(
                            "{} {}",
                            r["content"].as_str().unwrap_or_default(),
                            r["count"]
                        )
                    })
                    .collect();
                text.push_str(&format!(
                    "\n    @{} ({}): {}{}",
                    comment["author"].as_str().unwrap_or("ghost"),
                    comment["id"].as_str().unwrap_or_default(),
                    first_line(comment["body"].as_str().unwrap_or_default()),
                    if reactions.is_empty() {
                        String::new()
                    } else {
                        format!("  [{}]", reactions.join(", "))
                    }
                ));
            }
            text
        })
        .collect::<Vec<_>>()
        .join("\n")
}
