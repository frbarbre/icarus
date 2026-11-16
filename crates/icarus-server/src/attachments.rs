//! Context queued for the agent composer: PR comments, failed checks, files, notes and turn diffs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use icarus_github::threads::ReviewThread;
use icarus_protocol::RpcError;
use icarus_rules::facts::{Check, CheckKind};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::github::local_branch;
use crate::server::{Server, params, rpc, to_value};
use crate::state::{Attachment, Workspace};

pub const COMPOSER_ATTACH_TOPIC: &str = "composer.attach";
pub const LOG_EXCERPT_BYTES: usize = 8 * 1024;
const MAX_FILE_BYTES: usize = 512 * 1024;
const MAX_DIFF_BYTES: usize = 256 * 1024;

fn internal(error: impl std::fmt::Display) -> RpcError {
    rpc("internal", error)
}

fn escape_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\n', " ")
}

fn source_label(source: Option<&Value>) -> Option<String> {
    let source = source?;
    if let Some(url) = source["url"].as_str() {
        return Some(url.to_owned());
    }
    if let Some(path) = source["path"].as_str() {
        return Some(
            match (source["start_line"].as_u64(), source["end_line"].as_u64()) {
                (Some(start), Some(end)) if end != start => format!("{path}:{start}-{end}"),
                (Some(start), _) => format!("{path}:{start}"),
                _ => path.to_owned(),
            },
        );
    }
    if let Some(reference) = source["ref"].as_str() {
        return Some(reference.to_owned());
    }
    match source {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

/// The self-describing block an agent reads in place of the composer chip.
pub fn context_block(attachment: &Attachment) -> String {
    let mut open = format!(
        "<attachment kind=\"{}\" title=\"{}\"",
        escape_attribute(&attachment.kind),
        escape_attribute(&attachment.title)
    );
    if let Some(source) = source_label(attachment.source.as_ref()) {
        open.push_str(&format!(" source=\"{}\"", escape_attribute(&source)));
    }
    format!(
        "{open}>\n{}\n</attachment>",
        attachment.text.trim_end_matches('\n')
    )
}

/// Keeps the last `limit` bytes, where a failing step's error usually is.
pub fn tail_excerpt(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let tail = &text[start..];
    let tail = tail.split_once('\n').map_or(tail, |(_, rest)| rest);
    format!("… (truncated to the last {} KB)\n{tail}", limit / 1024)
}

fn head_excerpt(text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (truncated)", &text[..end])
}

/// The GitHub Actions run id in a check's details URL (`…/actions/runs/<id>/job/<job>`).
pub fn actions_run_id(url: &str) -> Option<u64> {
    let rest = url.split_once("/actions/runs/")?.1;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn check_conclusion(check: &Check) -> String {
    let value = match &check.kind {
        CheckKind::Run {
            conclusion: Some(conclusion),
            ..
        } => serde_json::to_value(conclusion),
        CheckKind::Run { status, .. } => serde_json::to_value(status),
        CheckKind::Status { state } => serde_json::to_value(state),
    };
    value
        .ok()
        .and_then(|v| v.as_str().map(str::to_lowercase))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn thread_location(thread: &ReviewThread) -> String {
    match (&thread.path, thread.start_line, thread.line) {
        (Some(path), Some(start), Some(line)) if start != line => {
            format!("{path}:{start}-{line}")
        }
        (Some(path), _, Some(line)) => format!("{path}:{line}"),
        (Some(path), _, None) => path.clone(),
        (None, _, _) => "conversation".to_owned(),
    }
}

fn thread_text(thread: &ReviewThread) -> String {
    let mut state = vec![if thread.is_resolved {
        "resolved"
    } else {
        "unresolved"
    }];
    if thread.is_outdated {
        state.push("outdated");
    }
    let mut text = format!("{} ({})\n", thread_location(thread), state.join(", "));
    if let Some(url) = thread.url() {
        text.push_str(url);
        text.push('\n');
    }
    for comment in &thread.comments {
        let author = comment.author.as_deref().unwrap_or("ghost");
        text.push_str(&format!("\n@{author}:\n{}\n", comment.body.trim_end()));
    }
    text
}

fn relative_display(workspace: &Workspace, path: &Path) -> String {
    path.strip_prefix(&workspace.path)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn truncate_title(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= max {
        return line.to_owned();
    }
    let mut short: String = line.chars().take(max.saturating_sub(1)).collect();
    short.push('…');
    short
}

#[derive(Deserialize)]
struct WorkspaceParam {
    workspace: String,
}

impl Server {
    /// Stores the attachment and offers it to every open Icarus window's composer.
    pub(crate) fn add_attachment(
        &self,
        workspace: &Workspace,
        kind: &str,
        title: &str,
        text: &str,
        source: Option<Value>,
    ) -> Result<(Attachment, usize), RpcError> {
        let attachment = self
            .store
            .add_attachment(&workspace.id, kind, title, text, source)
            .map_err(internal)?;
        let delivered = self.send_to_apps(
            COMPOSER_ATTACH_TOPIC,
            json!({
                "workspace_path": workspace.path,
                "attachment": { "id": attachment.id, "kind": attachment.kind, "title": attachment.title },
            }),
        );
        Ok((attachment, delivered))
    }

    fn added(&self, attachment: Attachment, delivered: usize) -> Result<Value, RpcError> {
        let mut value = to_value(attachment)?;
        value["delivered"] = json!(delivered);
        Ok(value)
    }

    fn attachment_by_id(&self, id: &str) -> Result<Attachment, RpcError> {
        self.store
            .attachment(id)
            .map_err(internal)?
            .ok_or_else(|| rpc("not_found", format!("attachment {id}")))
    }

    pub(crate) async fn attachments_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        match method {
            "attachment.add" => {
                #[derive(Deserialize)]
                struct P {
                    workspace: String,
                    kind: String,
                    title: String,
                    text: String,
                    source: Option<Value>,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.workspace)?;
                let (attachment, delivered) =
                    self.add_attachment(&workspace, &p.kind, &p.title, &p.text, p.source)?;
                self.added(attachment, delivered)
            }
            "attachment.get" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                }
                let p: P = params(p)?;
                let attachment = self.attachment_by_id(&p.id)?;
                Ok(json!({ "text": context_block(&attachment) }))
            }
            "attachment.list" => {
                #[derive(Deserialize)]
                struct P {
                    workspace: String,
                    #[serde(default)]
                    include_sent: bool,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.workspace)?;
                to_value(
                    self.store
                        .attachments(&workspace.id, p.include_sent)
                        .map_err(internal)?,
                )
            }
            "attachment.remove" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                }
                let p: P = params(p)?;
                let removed = self.store.remove_attachment(&p.id).map_err(internal)?;
                if !removed {
                    return Err(rpc("not_found", format!("attachment {}", p.id)));
                }
                Ok(json!({ "removed": true }))
            }
            "attachment.clear" => {
                let p: WorkspaceParam = params(p)?;
                let workspace = self.workspace_by_key(&p.workspace)?;
                let mut removed = 0;
                for attachment in self
                    .store
                    .attachments(&workspace.id, false)
                    .map_err(internal)?
                {
                    if self
                        .store
                        .remove_attachment(&attachment.id)
                        .map_err(internal)?
                    {
                        removed += 1;
                    }
                }
                Ok(json!({ "removed": removed }))
            }
            "attachment.mark_sent" => {
                #[derive(Deserialize)]
                struct P {
                    ids: Vec<String>,
                }
                let p: P = params(p)?;
                let marked = self.store.mark_attachments_sent(&p.ids).map_err(internal)?;
                Ok(json!({ "marked": marked }))
            }
            "attach.text" => {
                #[derive(Deserialize)]
                struct P {
                    workspace: String,
                    title: String,
                    text: String,
                }
                let p: P = params(p)?;
                if p.text.trim().is_empty() {
                    return Err(rpc("invalid_params", "text is empty"));
                }
                let workspace = self.workspace_by_key(&p.workspace)?;
                let (attachment, delivered) = self.add_attachment(
                    &workspace,
                    "text",
                    &p.title,
                    &p.text,
                    Some(json!({ "type": "text" })),
                )?;
                self.added(attachment, delivered)
            }
            "attach.file" => {
                #[derive(Deserialize)]
                struct P {
                    workspace: String,
                    path: PathBuf,
                    start_line: Option<usize>,
                    end_line: Option<usize>,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.workspace)?;
                let (attachment, delivered) =
                    self.attach_file(&workspace, &p.path, p.start_line, p.end_line)?;
                self.added(attachment, delivered)
            }
            "attach.turn" => {
                #[derive(Deserialize)]
                struct P {
                    workspace: String,
                    #[serde(default = "latest")]
                    turn: String,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.workspace)?;
                let (attachment, delivered) = self.attach_turn(&workspace, p.turn).await?;
                self.added(attachment, delivered)
            }
            "attach.check" => {
                #[derive(Deserialize)]
                struct P {
                    workspace: String,
                    name: String,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.workspace)?;
                let (attachment, delivered) = self.attach_check(&workspace, &p.name).await?;
                self.added(attachment, delivered)
            }
            "attach.note" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                }
                let p: P = params(p)?;
                let (attachment, delivered) = self.attach_note(&p.id)?;
                self.added(attachment, delivered)
            }
            "attach.pr_comments" => {
                #[derive(Deserialize)]
                struct P {
                    workspace: String,
                    ids: Option<Vec<String>>,
                    #[serde(default)]
                    unresolved: bool,
                    #[serde(default)]
                    humans_only: bool,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.workspace)?;
                self.attach_pr_comments(&workspace, p.ids, p.unresolved, p.humans_only)
                    .await
            }
            other => Err(rpc("unknown_method", other)),
        }
    }

    fn attach_file(
        &self,
        workspace: &Workspace,
        path: &Path,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<(Attachment, usize), RpcError> {
        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            workspace.path.join(path)
        };
        let content = std::fs::read(&full)
            .map_err(|e| rpc("not_found", format!("{}: {e}", full.display())))?;
        let content = String::from_utf8(content)
            .map_err(|_| rpc("invalid_params", format!("{} is not text", full.display())))?;
        let display = relative_display(workspace, &full);
        let (text, start, end) = match start_line {
            None if end_line.is_some() => {
                return Err(rpc("invalid_params", "end_line needs start_line"));
            }
            None => {
                if content.len() > MAX_FILE_BYTES {
                    return Err(rpc(
                        "too_large",
                        format!(
                            "{display} is over {} KB; attach a line range",
                            MAX_FILE_BYTES / 1024
                        ),
                    ));
                }
                (content, None, None)
            }
            Some(start) => {
                let end = end_line.unwrap_or(start);
                if start == 0 || end < start {
                    return Err(rpc(
                        "invalid_params",
                        "lines are 1-based and end_line must not precede start_line",
                    ));
                }
                let lines: Vec<&str> = content
                    .lines()
                    .skip(start - 1)
                    .take(end - start + 1)
                    .collect();
                if lines.is_empty() {
                    return Err(rpc(
                        "invalid_params",
                        format!("{display} has fewer than {start} lines"),
                    ));
                }
                let end = start + lines.len() - 1;
                (lines.join("\n"), Some(start), Some(end))
            }
        };
        let title = match (start, end) {
            (Some(start), Some(end)) if end != start => format!("{display}:{start}-{end}"),
            (Some(start), _) => format!("{display}:{start}"),
            _ => display.clone(),
        };
        self.add_attachment(
            workspace,
            "file",
            &title,
            &text,
            Some(json!({ "type": "file", "path": display, "start_line": start, "end_line": end })),
        )
    }

    async fn attach_turn(
        &self,
        workspace: &Workspace,
        turn: String,
    ) -> Result<(Attachment, usize), RpcError> {
        let path = workspace.path.clone();
        let (turn, diff) = tokio::task::spawn_blocking(move || {
            let turn = icarus_git::turns::resolve_turn(&path, &turn)?;
            let diff = icarus_git::git(&path, &["diff", &turn.parent, &turn.oid])?;
            anyhow::Ok((turn, diff))
        })
        .await
        .map_err(internal)?
        .map_err(|e| rpc("not_found", format!("{e:#}")))?;
        let prompt = turn.meta.prompt.trim();
        let title = if prompt.is_empty() {
            format!("Turn {}", turn.name)
        } else {
            format!("Turn: {}", truncate_title(prompt, 60))
        };
        let text = format!(
            "Agent turn {} ({} files, +{} -{})\nPrompt: {prompt}\n\n{}",
            turn.name,
            turn.files,
            turn.added,
            turn.removed,
            head_excerpt(diff, MAX_DIFF_BYTES)
        );
        self.add_attachment(
            workspace,
            "turn",
            &title,
            &text,
            Some(json!({
                "type": "turn",
                "ref": turn.ref_name,
                "name": turn.name,
                "oid": turn.oid,
                "parent": turn.parent,
            })),
        )
    }

    async fn workspace_pr(&self, workspace: &Workspace) -> Result<crate::github::PrView, RpcError> {
        let branch = local_branch(workspace).await;
        let view = self.pr_view(workspace, branch.as_deref(), false).await;
        if view.pr.is_none() {
            return Err(match &view.error {
                Some(error) if !view.known => rpc("github_failed", error),
                _ => rpc(
                    "no_pull_request",
                    "the workspace's branch has no pull request",
                ),
            });
        }
        Ok(view)
    }

    async fn attach_check(
        &self,
        workspace: &Workspace,
        name: &str,
    ) -> Result<(Attachment, usize), RpcError> {
        let view = self.workspace_pr(workspace).await?;
        let (Some(pr), Some(repo)) = (view.pr.as_ref(), view.repo.as_ref()) else {
            return Err(rpc("no_pull_request", "no pull request"));
        };
        let check = pr
            .checks
            .iter()
            .find(|c| c.name == name)
            .or_else(|| pr.checks.iter().find(|c| c.name.eq_ignore_ascii_case(name)))
            .ok_or_else(|| {
                rpc(
                    "not_found",
                    format!("no check named {name} on #{}", pr.number),
                )
            })?;
        let conclusion = check_conclusion(check);
        let run_id = check.url.as_deref().and_then(actions_run_id);
        let mut text = format!(
            "Check: {}\nConclusion: {conclusion}\nPull request: #{} {}\n",
            check.name, pr.number, pr.url
        );
        if let Some(url) = &check.url {
            text.push_str(&format!("Details: {url}\n"));
        }
        if let Some(started) = &check.started_at {
            text.push_str(&format!("Started: {started}\n"));
        }
        if let Some(completed) = &check.completed_at {
            text.push_str(&format!("Completed: {completed}\n"));
        }
        if let Some(run_id) = run_id {
            match self
                .github
                .client
                .runner()
                .run_failed_log(repo, run_id)
                .await
            {
                Ok(log) if !log.trim().is_empty() => {
                    text.push_str(&format!(
                        "\nFailed log (gh run view {run_id} --log-failed):\n{}\n",
                        tail_excerpt(&log, LOG_EXCERPT_BYTES)
                    ));
                }
                Ok(_) => text.push_str("\nThe run has no failed-step log.\n"),
                Err(error) => text.push_str(&format!("\nFailed log unavailable: {error}\n")),
            }
        }
        self.add_attachment(
            workspace,
            "check",
            &format!("{} ({conclusion})", check.name),
            &text,
            Some(json!({
                "type": "check",
                "name": check.name,
                "url": check.url,
                "run_id": run_id,
                "conclusion": conclusion,
                "pr": pr.number,
            })),
        )
    }

    async fn attach_pr_comments(
        &self,
        workspace: &Workspace,
        ids: Option<Vec<String>>,
        unresolved: bool,
        humans_only: bool,
    ) -> Result<Value, RpcError> {
        let view = self.workspace_pr(workspace).await?;
        let (Some(pr), Some(repo)) = (view.pr.as_ref(), view.repo.as_ref()) else {
            return Err(rpc("no_pull_request", "no pull request"));
        };
        let threads = self
            .github
            .client
            .review_threads(repo, pr.number)
            .await
            .map_err(|e| rpc("github_failed", e))?;
        let mut added = Vec::new();
        let mut delivered = 0;
        for mut thread in threads {
            if let Some(ids) = &ids
                && !ids.contains(&thread.id)
                && !thread.comments.iter().any(|c| ids.contains(&c.id))
            {
                continue;
            }
            if unresolved && thread.is_resolved {
                continue;
            }
            if humans_only {
                thread.comments.retain(|c| !c.is_bot);
            }
            let Some(first) = thread.comments.first() else {
                continue;
            };
            let title = format!(
                "@{} on {}",
                first.author.as_deref().unwrap_or("ghost"),
                thread_location(&thread)
            );
            let source = json!({
                "type": "pr-comment",
                "url": thread.url(),
                "thread_id": thread.id,
                "comment_ids": thread.comments.iter().map(|c| &c.id).collect::<Vec<_>>(),
                "path": thread.path,
                "line": thread.line,
                "pr": pr.number,
                "is_resolved": thread.is_resolved,
                "is_outdated": thread.is_outdated,
            });
            let (attachment, sent) = self.add_attachment(
                workspace,
                "pr-comment",
                &title,
                &thread_text(&thread),
                Some(source),
            )?;
            delivered += sent;
            added.push(attachment);
        }
        Ok(json!({ "attachments": added, "delivered": delivered }))
    }
}

fn latest() -> String {
    "latest".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(kind: &str, title: &str, text: &str, source: Option<Value>) -> Attachment {
        Attachment {
            id: "att-1".into(),
            workspace_id: "ws".into(),
            kind: kind.into(),
            title: title.into(),
            text: text.into(),
            source,
            created_at: 0,
            sent_at: None,
        }
    }

    #[test]
    fn context_blocks_name_kind_title_and_source() {
        let block = context_block(&attachment(
            "pr-comment",
            "@monalisa on \"a.rs\":3",
            "Fix this.\n",
            Some(json!({ "url": "https://github.com/acme/widgets/pull/1#r1", "path": "a.rs" })),
        ));
        assert_eq!(
            block,
            "<attachment kind=\"pr-comment\" title=\"@monalisa on &quot;a.rs&quot;:3\" source=\"https://github.com/acme/widgets/pull/1#r1\">\nFix this.\n</attachment>"
        );
        let file = context_block(&attachment(
            "file",
            "a.rs:2-4",
            "x",
            Some(json!({ "path": "a.rs", "start_line": 2, "end_line": 4 })),
        ));
        assert!(
            file.starts_with("<attachment kind=\"file\" title=\"a.rs:2-4\" source=\"a.rs:2-4\">")
        );
        let bare = context_block(&attachment("text", "Note", "hi", None));
        assert_eq!(
            bare,
            "<attachment kind=\"text\" title=\"Note\">\nhi\n</attachment>"
        );
    }

    #[test]
    fn run_ids_come_from_actions_urls() {
        assert_eq!(
            actions_run_id("https://github.com/acme/widgets/actions/runs/123/job/456"),
            Some(123)
        );
        assert_eq!(actions_run_id("https://ci.example.com/build/9"), None);
    }

    #[test]
    fn log_excerpts_keep_the_tail_on_line_and_char_boundaries() {
        let log = format!("{}\nfinal error ✗\n", "é".repeat(10_000));
        let excerpt = tail_excerpt(&log, LOG_EXCERPT_BYTES);
        assert!(excerpt.len() <= LOG_EXCERPT_BYTES + 64);
        assert!(excerpt.ends_with("final error ✗\n"));
        assert!(excerpt.starts_with("… (truncated"));
        assert_eq!(tail_excerpt("short", LOG_EXCERPT_BYTES), "short");
    }
}
