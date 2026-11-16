//! `icarus notes …`: notes on lines of a workspace's files, the agent → diff channel.

use std::io::Read as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use icarus_client::Client;
use serde_json::{Value, json};

const REPLY_WAIT: Duration = Duration::from_secs(15 * 60);

#[derive(Subcommand)]
pub enum NotesCommand {
    /// Add a note on `path:line` or `path:line-end` (relative to the current directory or the
    /// worktree). The body is markdown; a literal `\n` becomes a newline; `-` reads stdin.
    /// Agents (ICARUS_AGENT_ID set) are recorded as the author.
    Add {
        target: String,
        #[arg(num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
        body: Vec<String>,
        /// `old` when the line numbers are the base side of the diff.
        #[arg(long)]
        side: Option<String>,
    },
    /// List the workspace's open notes (`--all` includes resolved ones).
    List {
        /// Only notes on this file.
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Show a note with its code and whole thread.
    Show {
        id: String,
    },
    /// Reply under a note. A person's reply is answered by a fresh agent session in the worktree
    /// (never the workspace's running agent); `--wait` prints its answer.
    Reply {
        id: String,
        #[arg(num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
        body: Vec<String>,
        #[arg(long)]
        wait: bool,
    },
    /// Change a note's text, or a reply's with `--reply`.
    Edit {
        id: String,
        #[arg(num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
        body: Vec<String>,
        #[arg(long)]
        reply: Option<String>,
    },
    /// Remove a note with its thread, or one reply with `--reply`.
    Rm {
        id: String,
        #[arg(long)]
        reply: Option<String>,
    },
    Resolve {
        id: String,
    },
    Unresolve {
        id: String,
    },
    /// Put the whole conversation in the workspace agent's composer as one attachment.
    Attach {
        id: String,
    },
}

pub fn body_text(words: &[String]) -> Result<String> {
    let text = match words {
        [] => read_stdin()?,
        [dash] if dash == "-" => read_stdin()?,
        words => words.join(" "),
    };
    // `a\nb` typed in a shell arrives as a backslash and an `n`.
    let text = text.replace("\\n", "\n");
    if text.trim().is_empty() {
        bail!("the text is empty");
    }
    Ok(text)
}

fn read_stdin() -> Result<String> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("read stdin")?;
    Ok(text)
}

fn agent_id() -> Option<String> {
    std::env::var("ICARUS_AGENT_ID")
        .ok()
        .filter(|id| !id.is_empty())
}

pub fn target(workspace: Option<&str>) -> Value {
    let workspace = workspace.map(str::to_owned).or_else(|| {
        std::env::var("ICARUS_WORKSPACE_ID")
            .ok()
            .filter(|w| !w.is_empty())
    });
    json!({ "workspace": workspace, "cwd": std::env::current_dir().ok() })
}

fn resolve_path(path: &str) -> String {
    let given = Path::new(path);
    // Why only existing relative paths: the server resolves the rest against the worktree root.
    if given.is_relative() && given.exists() {
        return std::fs::canonicalize(given)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_owned());
    }
    path.to_owned()
}

pub fn add_request(
    workspace: Option<&str>,
    target_spec: &str,
    body: &[String],
    side: Option<&str>,
) -> Result<Value> {
    let (path, line, end) = super::attach::split_range(target_spec);
    let Some(line) = line else {
        bail!("give a line: `{target_spec}:12` or `{target_spec}:12-20`");
    };
    let mut request = target(workspace);
    request["path"] = json!(resolve_path(&path));
    request["line"] = json!(line);
    request["end_line"] = json!(end);
    request["body"] = json!(body_text(body)?);
    request["side"] = json!(side);
    request["agent_id"] = json!(agent_id());
    Ok(request)
}

pub async fn run(
    cli: &super::Cli,
    client: &Client,
    workspace: Option<&str>,
    command: &NotesCommand,
) -> Result<()> {
    match command {
        NotesCommand::Add { target, body, side } => {
            let request = add_request(workspace, target, body, side.as_deref())?;
            let note = client.request("note.add", request).await?;
            super::print(cli, &note, |n| format!("{}  {}", id(n), location(n)));
        }
        NotesCommand::List { path, all } => {
            let mut request = target(workspace);
            request["path"] = json!(path.as_deref().map(resolve_path));
            request["include_resolved"] = json!(all);
            let notes = client.request("note.list", request).await?;
            super::print(cli, &notes, list_text);
        }
        NotesCommand::Show { id } => {
            let note = client.request("note.get", json!({ "id": id })).await?;
            super::print(cli, &note, show_text);
        }
        NotesCommand::Reply {
            id: note_id,
            body,
            wait,
        } => {
            let replied = client
                .request(
                    "note.reply",
                    json!({ "id": note_id, "body": body_text(body)?, "agent_id": agent_id() }),
                )
                .await?;
            let pending = replied["agent_reply"]["id"].as_str().map(str::to_owned);
            match (pending, wait) {
                (Some(pending), true) => {
                    let answer = wait_for_answer(client, note_id, &pending).await?;
                    super::print(cli, &answer, answer_text);
                    if answer["status"] == "failed" {
                        std::process::exit(1);
                    }
                }
                _ => super::print(cli, &replied, |r| replied_text(r, note_id)),
            }
        }
        NotesCommand::Edit {
            id: note_id,
            body,
            reply,
        } => {
            let note = client
                .request(
                    "note.edit",
                    json!({ "id": note_id, "body": body_text(body)?, "reply_id": reply }),
                )
                .await?;
            super::print(cli, &note, |n| format!("edited {}", id(n)));
        }
        NotesCommand::Rm { id: note_id, reply } => {
            let removed = client
                .request("note.remove", json!({ "id": note_id, "reply_id": reply }))
                .await?;
            super::print(cli, &removed, |_| match reply {
                Some(reply) => format!("removed {reply} from {note_id}"),
                None => format!("removed {note_id}"),
            });
        }
        NotesCommand::Resolve { id: note_id } | NotesCommand::Unresolve { id: note_id } => {
            let resolve = matches!(command, NotesCommand::Resolve { .. });
            let method = if resolve {
                "note.resolve"
            } else {
                "note.unresolve"
            };
            let note = client.request(method, json!({ "id": note_id })).await?;
            super::print(cli, &note, |n| {
                format!(
                    "{} {}",
                    if resolve { "resolved" } else { "reopened" },
                    id(n)
                )
            });
        }
        NotesCommand::Attach { id: note_id } => {
            let attached = client
                .request("attach.note", json!({ "id": note_id }))
                .await?;
            super::print(cli, &attached, super::attach::attached);
        }
    }
    Ok(())
}

async fn wait_for_answer(client: &Client, note_id: &str, reply_id: &str) -> Result<Value> {
    let deadline = Instant::now() + REPLY_WAIT;
    loop {
        let note = client.request("note.get", json!({ "id": note_id })).await?;
        let reply = note["replies"]
            .as_array()
            .and_then(|r| r.iter().find(|r| r["id"] == reply_id))
            .cloned()
            .context("the reply was removed")?;
        if matches!(reply["status"].as_str(), Some("done" | "failed")) {
            return Ok(reply);
        }
        if Instant::now() > deadline {
            bail!("no answer yet; see `icarus notes show {note_id}`");
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

fn id(note: &Value) -> &str {
    note["id"].as_str().unwrap_or("?")
}

fn location(note: &Value) -> String {
    let path = note["path"].as_str().unwrap_or("?");
    match (note["line"].as_u64(), note["end_line"].as_u64()) {
        (Some(line), Some(end)) if end > line => format!("{path}:{line}-{end}"),
        (Some(line), _) => format!("{path}:{line}"),
        _ => path.to_owned(),
    }
}

fn author(entry: &Value) -> String {
    match (entry["author"].as_str(), entry["agent_id"].as_str()) {
        (Some("reply-agent"), _) => "reply agent".to_owned(),
        (Some("agent"), Some(agent)) => format!("agent {agent}"),
        (Some(author), _) => author.to_owned(),
        _ => "?".to_owned(),
    }
}

pub fn list_text(notes: &Value) -> String {
    let notes = notes.as_array().cloned().unwrap_or_default();
    if notes.is_empty() {
        return "no notes".to_owned();
    }
    notes
        .iter()
        .map(|note| {
            let replies = note["replies"].as_array().map_or(0, Vec::len);
            let mut header = format!("{}  {}  {}", id(note), location(note), author(note));
            if replies > 0 {
                header.push_str(&format!(
                    "  {replies} repl{}",
                    if replies == 1 { "y" } else { "ies" }
                ));
            }
            if note["resolved"] == true {
                header.push_str("  resolved");
            }
            let first = note["body"]
                .as_str()
                .and_then(|b| b.lines().next())
                .unwrap_or_default();
            format!("{header}\n    {first}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn show_text(note: &Value) -> String {
    let mut out = format!(
        "{}  {}  {}{}\n",
        id(note),
        location(note),
        author(note),
        if note["resolved"] == true {
            "  resolved"
        } else {
            ""
        }
    );
    if let Some(excerpt) = note["excerpt"].as_str() {
        let start = note["line"].as_u64().unwrap_or(1);
        for (i, line) in excerpt.lines().enumerate() {
            out.push_str(&format!("  {:>5} | {line}\n", start + i as u64));
        }
    }
    out.push_str(&format!(
        "\n{}\n",
        indent(note["body"].as_str().unwrap_or_default())
    ));
    for reply in note["replies"].as_array().into_iter().flatten() {
        out.push_str(&format!("\n  {} ({})", author(reply), id(reply)));
        if let Some(status) = reply["status"].as_str() {
            out.push_str(&format!(", {status}"));
        }
        out.push_str(":\n");
        if let Some(error) = reply["error"].as_str() {
            out.push_str(&format!("    error: {error}\n"));
        }
        let body = reply["body"].as_str().unwrap_or_default();
        if !body.is_empty() {
            out.push_str(&format!("{}\n", indent(body)));
        }
    }
    out.trim_end().to_owned()
}

fn replied_text(replied: &Value, note_id: &str) -> String {
    let reply = id(&replied["reply"]);
    match replied["agent_reply"]["id"].as_str() {
        Some(pending) => format!(
            "{reply} posted; an agent session is answering ({pending}). `icarus notes show {note_id}` shows its answer."
        ),
        None => format!("{reply} posted"),
    }
}

fn answer_text(reply: &Value) -> String {
    match reply["status"].as_str() {
        Some("failed") => format!(
            "the reply session failed: {}",
            reply["error"].as_str().unwrap_or("unknown error")
        ),
        _ => reply["body"].as_str().unwrap_or_default().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_join_words_and_turn_escaped_newlines_into_lines() {
        assert_eq!(
            body_text(&["Fix".into(), "this\\nplease".into()]).expect("body"),
            "Fix this\nplease"
        );
        assert!(body_text(&["  ".into()]).is_err());
    }

    #[test]
    fn add_requests_need_a_line_and_carry_the_range() {
        let request =
            add_request(Some("ws"), "src/nope.rs:3-5", &["hi".into()], None).expect("request");
        assert_eq!(request["workspace"], "ws");
        assert_eq!(request["path"], "src/nope.rs");
        assert_eq!(request["line"], 3);
        assert_eq!(request["end_line"], 5);
        assert_eq!(request["body"], "hi");
        assert!(add_request(Some("ws"), "src/nope.rs", &["hi".into()], None).is_err());
    }

    #[test]
    fn listings_show_location_author_replies_and_state() {
        let notes = json!([{
            "id": "note-1", "path": "a.rs", "line": 2, "end_line": 4, "author": "agent",
            "agent_id": "agent-x", "body": "Look here\nmore", "resolved": true,
            "replies": [{ "id": "reply-1" }],
        }]);
        assert_eq!(
            list_text(&notes),
            "note-1  a.rs:2-4  agent agent-x  1 reply  resolved\n    Look here"
        );
        assert_eq!(list_text(&json!([])), "no notes");
        let shown = show_text(&json!({
            "id": "note-1", "path": "a.rs", "line": 2, "end_line": 2, "author": "user",
            "excerpt": "let a = 1;", "body": "Why?",
            "replies": [{ "id": "reply-2", "author": "reply-agent", "status": "done", "body": "Because." }],
        }));
        assert_eq!(
            shown,
            "note-1  a.rs:2  user\n      2 | let a = 1;\n\n    Why?\n\n  reply agent (reply-2), done:\n    Because."
        );
    }
}
