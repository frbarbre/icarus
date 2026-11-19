//! Notes on lines of a workspace's files, with threads. Users and agents write them; a user's reply is
//! answered by its own throwaway agent session (never the workspace's running agent), whose answer
//! lands back in the thread.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use icarus_protocol::RpcError;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::{AgentEvent, AgentOptions, AgentThread};
use crate::next_action::{choose_agent, setting};
use crate::server::{Server, params as rpc_params, rpc, to_value};
use crate::state::{Attachment, Store, Workspace};

pub const NOTE_CHANGED_TOPIC: &str = "note.changed";
pub const DEFAULT_REPLY_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const MAX_EXCERPT_LINES: usize = 200;
const MAX_EXCERPT_BYTES: usize = 32 * 1024;

pub const AUTHOR_USER: &str = "user";
pub const AUTHOR_AGENT: &str = "agent";
pub const AUTHOR_REPLY_AGENT: &str = "reply-agent";

pub const STATUS_PENDING: &str = "pending";
pub const STATUS_RUNNING: &str = "running";
pub const STATUS_DONE: &str = "done";
pub const STATUS_FAILED: &str = "failed";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Note {
    pub id: String,
    pub workspace_id: String,
    pub path: String,
    pub line: u32,
    pub end_line: u32,
    pub side: Option<String>,
    pub base: Option<String>,
    pub author: String,
    pub agent_id: Option<String>,
    pub body: String,
    pub excerpt: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub resolved: bool,
    pub resolved_at: Option<u64>,
    pub replies: Vec<NoteReply>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NoteReply {
    pub id: String,
    pub note_id: String,
    pub author: String,
    pub agent_id: Option<String>,
    pub body: String,
    pub status: Option<String>,
    pub error: Option<String>,
    pub changed_files: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn short_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
}

const NOTE_COLUMNS: &str = "id, workspace_id, path, line, end_line, side, base, author, agent_id, body, excerpt, created_at, updated_at, resolved_at";
const REPLY_COLUMNS: &str =
    "id, note_id, author, agent_id, body, status, error, changed_files, created_at, updated_at";

fn note_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    let resolved_at = row.get::<_, Option<i64>>(13)?.map(|t| t as u64);
    Ok(Note {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        path: row.get(2)?,
        line: row.get(3)?,
        end_line: row.get(4)?,
        side: row.get(5)?,
        base: row.get(6)?,
        author: row.get(7)?,
        agent_id: row.get(8)?,
        body: row.get(9)?,
        excerpt: row.get(10)?,
        created_at: row.get::<_, i64>(11)? as u64,
        updated_at: row.get::<_, i64>(12)? as u64,
        resolved: resolved_at.is_some(),
        resolved_at,
        replies: Vec::new(),
    })
}

fn reply_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NoteReply> {
    let changed: Option<String> = row.get(7)?;
    Ok(NoteReply {
        id: row.get(0)?,
        note_id: row.get(1)?,
        author: row.get(2)?,
        agent_id: row.get(3)?,
        body: row.get(4)?,
        status: row.get(5)?,
        error: row.get(6)?,
        changed_files: changed
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_default(),
        created_at: row.get::<_, i64>(8)? as u64,
        updated_at: row.get::<_, i64>(9)? as u64,
    })
}

impl Store {
    pub fn add_note(&self, note: &Note) -> Result<()> {
        self.db().execute(
            &format!("INSERT INTO notes ({NOTE_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"),
            params![
                note.id,
                note.workspace_id,
                note.path,
                note.line,
                note.end_line,
                note.side,
                note.base,
                note.author,
                note.agent_id,
                note.body,
                note.excerpt,
                note.created_at as i64,
                note.updated_at as i64,
                note.resolved_at.map(|t| t as i64)
            ],
        )?;
        Ok(())
    }

    fn replies_of(db: &rusqlite::Connection, note_id: &str) -> Result<Vec<NoteReply>> {
        let mut statement = db.prepare(&format!(
            "SELECT {REPLY_COLUMNS} FROM note_replies WHERE note_id = ?1 ORDER BY created_at, rowid"
        ))?;
        let rows = statement.query_map(params![note_id], reply_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn note(&self, id: &str) -> Result<Option<Note>> {
        let db = self.db();
        let note = db
            .query_row(
                &format!("SELECT {NOTE_COLUMNS} FROM notes WHERE id = ?1"),
                params![id],
                note_from_row,
            )
            .optional()?;
        let Some(mut note) = note else {
            return Ok(None);
        };
        note.replies = Self::replies_of(&db, &note.id)?;
        Ok(Some(note))
    }

    pub fn notes(
        &self,
        workspace_id: &str,
        path: Option<&str>,
        include_resolved: bool,
    ) -> Result<Vec<Note>> {
        let db = self.db();
        let mut statement = db.prepare(&format!(
            "SELECT {NOTE_COLUMNS} FROM notes
             WHERE workspace_id = ?1 AND (?2 IS NULL OR path = ?2) AND (?3 OR resolved_at IS NULL)
             ORDER BY path, line, created_at, rowid"
        ))?;
        let rows =
            statement.query_map(params![workspace_id, path, include_resolved], note_from_row)?;
        let mut notes = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        for note in &mut notes {
            note.replies = Self::replies_of(&db, &note.id)?;
        }
        Ok(notes)
    }

    pub fn edit_note(&self, id: &str, body: &str) -> Result<bool> {
        Ok(self.db().execute(
            "UPDATE notes SET body = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, body, now_ms() as i64],
        )? > 0)
    }

    pub fn set_note_resolved(&self, id: &str, resolved: bool) -> Result<bool> {
        let at = resolved.then(|| now_ms() as i64);
        Ok(self.db().execute(
            "UPDATE notes SET resolved_at = ?2 WHERE id = ?1",
            params![id, at],
        )? > 0)
    }

    pub fn remove_note(&self, id: &str) -> Result<bool> {
        Ok(self
            .db()
            .execute("DELETE FROM notes WHERE id = ?1", params![id])?
            > 0)
    }

    pub fn add_note_reply(&self, reply: &NoteReply) -> Result<()> {
        self.db().execute(
            &format!("INSERT INTO note_replies ({REPLY_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"),
            params![
                reply.id,
                reply.note_id,
                reply.author,
                reply.agent_id,
                reply.body,
                reply.status,
                reply.error,
                serde_json::to_string(&reply.changed_files)?,
                reply.created_at as i64,
                reply.updated_at as i64
            ],
        )?;
        Ok(())
    }

    pub fn note_reply(&self, id: &str) -> Result<Option<NoteReply>> {
        Ok(self
            .db()
            .query_row(
                &format!("SELECT {REPLY_COLUMNS} FROM note_replies WHERE id = ?1"),
                params![id],
                reply_from_row,
            )
            .optional()?)
    }

    pub fn update_note_reply(&self, reply: &NoteReply) -> Result<bool> {
        Ok(self.db().execute(
            "UPDATE note_replies SET agent_id = ?2, body = ?3, status = ?4, error = ?5, changed_files = ?6, updated_at = ?7
             WHERE id = ?1",
            params![
                reply.id,
                reply.agent_id,
                reply.body,
                reply.status,
                reply.error,
                serde_json::to_string(&reply.changed_files)?,
                reply.updated_at as i64
            ],
        )? > 0)
    }

    pub fn remove_note_reply(&self, id: &str) -> Result<bool> {
        Ok(self
            .db()
            .execute("DELETE FROM note_replies WHERE id = ?1", params![id])?
            > 0)
    }

    pub fn is_note_reply_agent(&self, agent_id: &str) -> Result<bool> {
        Ok(self
            .db()
            .query_row(
                "SELECT 1 FROM note_replies WHERE agent_id = ?1 AND author = ?2 LIMIT 1",
                params![agent_id, AUTHOR_REPLY_AGENT],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn fail_unfinished_note_replies(&self, error: &str) -> Result<Vec<Option<String>>> {
        let db = self.db();
        let mut statement =
            db.prepare("SELECT agent_id FROM note_replies WHERE status IN (?1, ?2)")?;
        let affected = statement
            .query_map(params![STATUS_PENDING, STATUS_RUNNING], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<Option<String>>>>()?;
        drop(statement);
        db.execute(
            "UPDATE note_replies SET status = ?3, error = ?4, updated_at = ?5 WHERE status IN (?1, ?2)",
            params![
                STATUS_PENDING,
                STATUS_RUNNING,
                STATUS_FAILED,
                error,
                now_ms() as i64
            ],
        )?;
        Ok(affected)
    }
}

pub fn location(path: &str, line: u32, end_line: u32) -> String {
    if end_line > line {
        format!("{path}:{line}-{end_line}")
    } else {
        format!("{path}:{line}")
    }
}

pub fn excerpt_of(content: &str, start: u32, end: u32) -> Option<String> {
    let start = start.max(1) as usize;
    let count = (end as usize).saturating_sub(start) + 1;
    let mut lines: Vec<&str> = content
        .lines()
        .skip(start - 1)
        .take(count.min(MAX_EXCERPT_LINES))
        .collect();
    if lines.is_empty() {
        return None;
    }
    let mut size = 0;
    if let Some(cut) = lines.iter().position(|l| {
        size += l.len() + 1;
        size > MAX_EXCERPT_BYTES
    }) {
        lines.truncate(cut.max(1));
    }
    let mut text = lines.join("\n");
    if lines.len() < count {
        text.push_str("\n… (truncated)");
    }
    Some(text)
}

fn read_excerpt(root: &Path, path: &str, start: u32, end: u32) -> Option<String> {
    let content = std::fs::read(root.join(path)).ok()?;
    excerpt_of(&String::from_utf8(content).ok()?, start, end)
}

fn numbered(excerpt: &str, start: u32) -> String {
    let width = (start as usize + excerpt.lines().count()).to_string().len();
    excerpt
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if line == "… (truncated)" {
                line.to_owned()
            } else {
                format!("{:>width$}  {line}", start as usize + i)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn fence_for(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    "`".repeat(longest.max(2) + 1)
}

fn author_label(author: &str, agent_id: Option<&str>) -> String {
    match (author, agent_id) {
        (AUTHOR_USER, _) => "User".to_owned(),
        (AUTHOR_REPLY_AGENT, _) => "Reply agent".to_owned(),
        (_, Some(id)) => format!("Agent ({id})"),
        _ => "Agent".to_owned(),
    }
}

pub fn thread_text(note: &Note, code: Option<&str>) -> String {
    let place = location(&note.path, note.line, note.end_line);
    let mut text = format!(
        "Note {} on {place}{}{}\n",
        note.id,
        if note.side.as_deref() == Some("old") {
            " (base side of the diff)"
        } else {
            ""
        },
        if note.resolved { " (resolved)" } else { "" }
    );
    if let Some(code) = code.or(note.excerpt.as_deref()) {
        let fence = fence_for(code);
        text.push_str(&format!(
            "\nCode at {place}:\n{fence}\n{}\n{fence}\n",
            numbered(code, note.line)
        ));
    }
    text.push_str(&format!(
        "\n{}:\n{}\n",
        author_label(&note.author, note.agent_id.as_deref()),
        note.body.trim_end()
    ));
    for reply in &note.replies {
        let label = author_label(&reply.author, reply.agent_id.as_deref());
        match reply.status.as_deref() {
            Some(STATUS_PENDING | STATUS_RUNNING) => continue,
            Some(STATUS_FAILED) => {
                text.push_str(&format!(
                    "\n{label} (failed: {}):\n{}\n",
                    reply.error.as_deref().unwrap_or("unknown error"),
                    reply.body.trim_end()
                ));
            }
            _ => text.push_str(&format!("\n{label}:\n{}\n", reply.body.trim_end())),
        }
    }
    text
}

pub fn reply_prompt(note: &Note, worktree: &Path, code: Option<&str>) -> String {
    format!(
        "You are answering a note thread in the git worktree {worktree}. The note points at {place}. \
         The thread is below; answer its last message.\n\n\
         - Your final message is posted into the thread as your reply, as is. Keep it short and in markdown; no preamble.\n\
         - Read whatever you need. Change files only when the last message asks for a change, and keep the change to what was asked. \
         Icarus lists the files you changed under your reply.\n\
         - Do not commit, push, reset, stash or switch branches, and do not message other agents.\n\
         - This session ends once you answer, so do not ask follow-up questions; say what you assumed instead.\n\n\
         <note-thread>\n{thread}</note-thread>\n",
        worktree = worktree.display(),
        place = location(&note.path, note.line, note.end_line),
        thread = thread_text(note, code),
    )
}

fn last_text(events: &[AgentEvent]) -> Option<String> {
    events
        .iter()
        .rev()
        .filter(|e| e.kind == "message" && e.data["parent"].is_null())
        .find_map(|e| {
            let text: Vec<&str> = e.data["content"]
                .as_array()?
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect();
            (!text.is_empty()).then(|| text.join("\n"))
        })
}

pub fn turn_outcome(events: &[AgentEvent]) -> Option<Result<String, String>> {
    let start = events
        .iter()
        .position(|e| e.kind == "user_message")
        .unwrap_or(0);
    let turn = &events[start..];
    for (i, event) in turn.iter().enumerate() {
        match event.kind.as_str() {
            "turn_end" => {
                let result = event.data["result"].as_str().unwrap_or_default();
                if event.data["is_error"] == json!(true) {
                    let reason = if result.trim().is_empty() {
                        "the agent's turn failed".to_owned()
                    } else {
                        result.trim().to_owned()
                    };
                    return Some(Err(reason));
                }
                let answer = last_text(&turn[..i])
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| result.to_owned());
                return Some(Ok(answer.trim().to_owned()));
            }
            "exit" => {
                let stderr = turn[..i]
                    .iter()
                    .rev()
                    .find(|e| e.kind == "stderr")
                    .and_then(|e| e.data["line"].as_str())
                    .map(|l| format!(": {l}"))
                    .unwrap_or_default();
                return Some(Err(format!("the agent exited before answering{stderr}")));
            }
            _ => {}
        }
    }
    None
}

fn changed_section(files: &[String]) -> String {
    let list: Vec<String> = files.iter().map(|f| format!("- `{f}`")).collect();
    format!("Files changed:\n{}", list.join("\n"))
}

fn relative_path(workspace: &Workspace, path: &str) -> Result<String, RpcError> {
    let given = Path::new(path);
    let relative = if given.is_absolute() {
        let roots = [
            std::fs::canonicalize(&workspace.path).unwrap_or_else(|_| workspace.path.clone()),
            workspace.path.clone(),
        ];
        let fulls = [
            std::fs::canonicalize(given).unwrap_or_else(|_| given.to_path_buf()),
            given.to_path_buf(),
        ];
        roots
            .iter()
            .flat_map(|root| fulls.iter().map(move |full| full.strip_prefix(root)))
            .find_map(Result::ok)
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                rpc(
                    "invalid_params",
                    format!("{path} is not inside the workspace"),
                )
            })?
    } else {
        given.to_path_buf()
    };
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => {
                return Err(rpc(
                    "invalid_params",
                    format!("{path} must stay inside the workspace"),
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(rpc("invalid_params", "path is empty"));
    }
    Ok(parts.join("/"))
}

fn internal(error: impl std::fmt::Display) -> RpcError {
    rpc("internal", format!("{error:#}"))
}

#[derive(Deserialize, Default)]
pub(crate) struct Target {
    pub(crate) workspace: Option<String>,
    pub(crate) cwd: Option<PathBuf>,
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

struct Outcome {
    body: Option<String>,
    error: Option<String>,
    changed: Vec<String>,
}

impl Server {
    pub(crate) fn note_workspace(&self, target: &Target) -> Result<Workspace, RpcError> {
        if let Some(key) = target.workspace.as_deref().filter(|k| !k.is_empty()) {
            return self.workspace_by_key(key);
        }
        let Some(cwd) = &target.cwd else {
            return Err(rpc(
                "invalid_params",
                "name the workspace, or run inside its worktree",
            ));
        };
        let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.clone());
        self.store
            .workspaces(None)
            .map_err(internal)?
            .into_iter()
            .filter_map(|ws| {
                let root = std::fs::canonicalize(&ws.path).unwrap_or_else(|_| ws.path.clone());
                cwd.starts_with(&root)
                    .then(|| (root.components().count(), ws))
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, ws)| ws)
            .ok_or_else(|| {
                rpc(
                    "not_found",
                    format!("{} is not inside an Icarus workspace", cwd.display()),
                )
            })
    }

    fn note_by_id(&self, id: &str) -> Result<Note, RpcError> {
        self.store
            .note(id)
            .map_err(internal)?
            .ok_or_else(|| rpc("not_found", format!("note {id}")))
    }

    fn note_changed(&self, note: &Note, removed: bool) {
        self.broadcast(
            NOTE_CHANGED_TOPIC,
            json!({ "workspace": note.workspace_id, "id": note.id, "path": note.path, "removed": removed }),
        );
    }

    fn note_value(&self, id: &str) -> Result<Value, RpcError> {
        to_value(self.note_by_id(id)?)
    }

    fn current_code(&self, note: &Note) -> Option<String> {
        let workspace = self.store.workspace(&note.workspace_id).ok().flatten()?;
        if note.side.as_deref() == Some("old") {
            return None;
        }
        read_excerpt(&workspace.path, &note.path, note.line, note.end_line)
    }

    async fn kill_reply_sessions(&self, replies: &[NoteReply]) {
        for reply in replies {
            if matches!(
                reply.status.as_deref(),
                Some(STATUS_PENDING | STATUS_RUNNING)
            ) && let Some(agent) = reply.agent_id.as_deref().and_then(|id| self.agents.get(id))
            {
                let _ = agent.kill().await;
                self.agents.remove(agent.id());
            }
        }
    }

    pub(crate) async fn notes_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        match method {
            "note.add" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(flatten)]
                    target: Target,
                    path: String,
                    line: u32,
                    end_line: Option<u32>,
                    body: String,
                    author: Option<String>,
                    agent_id: Option<String>,
                    side: Option<String>,
                    base: Option<String>,
                }
                let p: P = rpc_params(p)?;
                let workspace = self.note_workspace(&p.target)?;
                if p.body.trim().is_empty() {
                    return Err(rpc("invalid_params", "the note is empty"));
                }
                let end_line = p.end_line.unwrap_or(p.line);
                if p.line == 0 || end_line < p.line {
                    return Err(rpc(
                        "invalid_params",
                        "lines are 1-based and end_line must not precede line",
                    ));
                }
                if let Some(side) = p.side.as_deref()
                    && !matches!(side, "new" | "old")
                {
                    return Err(rpc("invalid_params", "side is new or old"));
                }
                let author = match (p.author.as_deref(), &p.agent_id) {
                    (Some(AUTHOR_USER), None) | (None, None) => AUTHOR_USER,
                    (Some(AUTHOR_AGENT) | None, _) | (Some(AUTHOR_USER), Some(_)) => AUTHOR_AGENT,
                    (Some(other), _) => {
                        return Err(rpc(
                            "invalid_params",
                            format!("author {other} is not user or agent"),
                        ));
                    }
                };
                let path = relative_path(&workspace, &p.path)?;
                let full = workspace.path.join(&path);
                let excerpt = if p.side.as_deref() == Some("old") {
                    None
                } else if full.is_file() {
                    let content = std::fs::read(&full).map_err(internal)?;
                    let content = String::from_utf8_lossy(&content);
                    let count = content.lines().count() as u32;
                    if p.line > count.max(1) {
                        return Err(rpc(
                            "invalid_params",
                            format!("{path} has {count} lines; line {} is past the end", p.line),
                        ));
                    }
                    excerpt_of(&content, p.line, end_line.min(count.max(1)))
                } else {
                    None
                };
                let now = now_ms();
                let note = Note {
                    id: short_id("note"),
                    workspace_id: workspace.id.clone(),
                    path,
                    line: p.line,
                    end_line,
                    side: p.side,
                    base: p.base,
                    author: author.to_owned(),
                    agent_id: p.agent_id,
                    body: p.body,
                    excerpt,
                    created_at: now,
                    updated_at: now,
                    resolved: false,
                    resolved_at: None,
                    replies: Vec::new(),
                };
                self.store.add_note(&note).map_err(internal)?;
                self.note_changed(&note, false);
                to_value(note)
            }
            "note.list" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(flatten)]
                    target: Target,
                    path: Option<String>,
                    #[serde(default)]
                    include_resolved: bool,
                }
                let p: P = rpc_params(p)?;
                let workspace = self.note_workspace(&p.target)?;
                let path = p
                    .path
                    .as_deref()
                    .map(|path| relative_path(&workspace, path))
                    .transpose()?;
                to_value(
                    self.store
                        .notes(&workspace.id, path.as_deref(), p.include_resolved)
                        .map_err(internal)?,
                )
            }
            "note.get" => {
                let p: Id = rpc_params(p)?;
                self.note_value(&p.id)
            }
            "note.thread" => {
                let p: Id = rpc_params(p)?;
                let note = self.note_by_id(&p.id)?;
                Ok(json!({ "text": thread_text(&note, self.current_code(&note).as_deref()) }))
            }
            "note.reply" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    body: String,
                    author: Option<String>,
                    agent_id: Option<String>,
                    // An agent replying through the CLI is talking to the user, so nothing answers it.
                    ask: Option<bool>,
                }
                let p: P = rpc_params(p)?;
                if p.body.trim().is_empty() {
                    return Err(rpc("invalid_params", "the reply is empty"));
                }
                let note = self.note_by_id(&p.id)?;
                let author = match (p.author.as_deref(), &p.agent_id) {
                    (Some(AUTHOR_USER), None) | (None, None) => AUTHOR_USER,
                    (Some(AUTHOR_AGENT) | None, _) | (Some(AUTHOR_USER), Some(_)) => AUTHOR_AGENT,
                    (Some(other), _) => {
                        return Err(rpc(
                            "invalid_params",
                            format!("author {other} is not user or agent"),
                        ));
                    }
                };
                let now = now_ms();
                let reply = NoteReply {
                    id: short_id("reply"),
                    note_id: note.id.clone(),
                    author: author.to_owned(),
                    agent_id: p.agent_id,
                    body: p.body,
                    status: None,
                    error: None,
                    changed_files: Vec::new(),
                    created_at: now,
                    updated_at: now,
                };
                self.store.add_note_reply(&reply).map_err(internal)?;
                let agent_reply = if p.ask.unwrap_or(author == AUTHOR_USER) {
                    let pending = NoteReply {
                        id: short_id("reply"),
                        note_id: note.id.clone(),
                        author: AUTHOR_REPLY_AGENT.to_owned(),
                        agent_id: None,
                        body: String::new(),
                        status: Some(STATUS_PENDING.to_owned()),
                        error: None,
                        changed_files: Vec::new(),
                        created_at: now,
                        updated_at: now,
                    };
                    self.store.add_note_reply(&pending).map_err(internal)?;
                    let server = Arc::clone(self);
                    let (note_id, pending_id) = (note.id.clone(), pending.id.clone());
                    tokio::spawn(async move { server.run_note_reply(&note_id, &pending_id).await });
                    Some(pending)
                } else {
                    None
                };
                self.note_changed(&note, false);
                Ok(
                    json!({ "note": self.note_value(&note.id)?, "reply": reply, "agent_reply": agent_reply }),
                )
            }
            "note.edit" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    body: String,
                    reply_id: Option<String>,
                }
                let p: P = rpc_params(p)?;
                if p.body.trim().is_empty() {
                    return Err(rpc("invalid_params", "the text is empty"));
                }
                let note = self.note_by_id(&p.id)?;
                match &p.reply_id {
                    Some(reply_id) => {
                        let mut reply = note
                            .replies
                            .iter()
                            .find(|r| r.id == *reply_id)
                            .cloned()
                            .ok_or_else(|| rpc("not_found", format!("reply {reply_id}")))?;
                        reply.body = p.body;
                        reply.updated_at = now_ms();
                        self.store.update_note_reply(&reply).map_err(internal)?;
                    }
                    None => {
                        self.store.edit_note(&note.id, &p.body).map_err(internal)?;
                    }
                }
                self.note_changed(&note, false);
                self.note_value(&note.id)
            }
            "note.remove" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    reply_id: Option<String>,
                }
                let p: P = rpc_params(p)?;
                let note = self.note_by_id(&p.id)?;
                match &p.reply_id {
                    Some(reply_id) => {
                        let reply = note
                            .replies
                            .iter()
                            .find(|r| r.id == *reply_id)
                            .ok_or_else(|| rpc("not_found", format!("reply {reply_id}")))?;
                        self.store.remove_note_reply(reply_id).map_err(internal)?;
                        self.kill_reply_sessions(std::slice::from_ref(reply)).await;
                        self.note_changed(&note, false);
                        Ok(json!({ "removed": true, "note": self.note_value(&note.id)? }))
                    }
                    None => {
                        self.store.remove_note(&note.id).map_err(internal)?;
                        self.kill_reply_sessions(&note.replies).await;
                        self.note_changed(&note, true);
                        Ok(json!({ "removed": true }))
                    }
                }
            }
            "note.resolve" | "note.unresolve" => {
                let p: Id = rpc_params(p)?;
                let note = self.note_by_id(&p.id)?;
                self.store
                    .set_note_resolved(&note.id, method == "note.resolve")
                    .map_err(internal)?;
                self.note_changed(&note, false);
                self.note_value(&note.id)
            }
            other => Err(rpc("unknown_method", other)),
        }
    }

    pub(crate) fn attach_note(&self, id: &str) -> Result<(Attachment, usize), RpcError> {
        let note = self.note_by_id(id)?;
        let workspace = self.workspace_by_key(&note.workspace_id)?;
        let place = location(&note.path, note.line, note.end_line);
        let text = thread_text(&note, self.current_code(&note).as_deref());
        let source = json!({
            "type": "note",
            "id": note.id,
            "path": note.path,
            "start_line": note.line,
            "end_line": note.end_line,
        });
        self.add_attachment(
            &workspace,
            "note",
            &format!("Note on {place}"),
            &text,
            Some(source),
        )
    }

    async fn run_note_reply(self: Arc<Self>, note_id: &str, reply_id: &str) {
        let outcome = self.answer_note(note_id, reply_id).await;
        let Ok(Some(mut reply)) = self.store.note_reply(reply_id) else {
            return;
        };
        let mut body = outcome.body.unwrap_or_default();
        if !outcome.changed.is_empty() {
            let section = changed_section(&outcome.changed);
            body = if body.trim().is_empty() {
                section
            } else {
                format!("{}\n\n{section}", body.trim_end())
            };
        }
        reply.body = body;
        reply.status = Some(
            if outcome.error.is_some() {
                STATUS_FAILED
            } else {
                STATUS_DONE
            }
            .to_owned(),
        );
        reply.error = outcome.error;
        reply.changed_files = outcome.changed;
        reply.updated_at = now_ms();
        if let Err(error) = self.store.update_note_reply(&reply) {
            tracing::warn!("cannot store the answer to note {note_id}: {error:#}");
        }
        if let Ok(Some(note)) = self.store.note(note_id) {
            self.note_changed(&note, false);
        }
    }

    async fn answer_note(self: &Arc<Self>, note_id: &str, reply_id: &str) -> Outcome {
        let failed = |error: String| Outcome {
            body: None,
            error: Some(error),
            changed: Vec::new(),
        };
        let Ok(Some(mut note)) = self.store.note(note_id) else {
            return failed("the note was removed".to_owned());
        };
        // Replies posted after this one belong to their own sessions.
        if let Some(own) = note.replies.iter().position(|r| r.id == reply_id) {
            note.replies.truncate(own);
        }
        let Ok(Some(workspace)) = self.store.workspace(&note.workspace_id) else {
            return failed("the workspace was removed".to_owned());
        };
        let root = self
            .store
            .project(&workspace.project_id)
            .ok()
            .flatten()
            .map(|p| p.root);
        let settings = self.resolved_settings(root.as_deref());
        let timeout_ms = setting(&settings, &["notes", "reply_timeout_ms"])
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_REPLY_TIMEOUT_MS)
            .max(1);
        let code = self.current_code(&note);
        let prompt = reply_prompt(&note, &workspace.path, code.as_deref());
        let worktree = workspace.path.clone();
        let before = snapshot(&worktree).await;

        let choice = choose_agent(&settings, None);
        let mut options = AgentOptions {
            provider: choice.provider,
            cwd: workspace.path.clone(),
            // No workspace, so the session never shows up as one of the workspace's agents.
            workspace: None,
            title: Some(format!(
                "Note reply: {}",
                location(&note.path, note.line, note.end_line)
            )),
            model: choice.model,
            bypass_permissions: true,
            resume: None,
            command: choice.command,
            env: [
                ("ICARUS_WORKSPACE_ID".to_owned(), workspace.id.clone()),
                ("ICARUS_NOTE_ID".to_owned(), note.id.clone()),
            ]
            .into(),
            account: None,
            remote_control: false,
            permission_mode: None,
        };
        if let Err(error) = self.apply_account(&mut options, None) {
            return failed(error.message);
        }
        let agent = match self.agents.create(options).await {
            Ok(agent) => agent,
            Err(error) => return failed(format!("could not start the agent: {error:#}")),
        };
        if let Ok(Some(mut reply)) = self.store.note_reply(reply_id) {
            reply.agent_id = Some(agent.id().to_owned());
            reply.status = Some(STATUS_RUNNING.to_owned());
            reply.updated_at = now_ms();
            let _ = self.store.update_note_reply(&reply);
            self.note_changed(&note, false);
        } else {
            self.end_session(&agent).await;
            return failed("the reply was removed".to_owned());
        }
        let answer = match agent.send(&prompt, &[]).await {
            Err(error) => Err(format!("could not send the thread to the agent: {error:#}")),
            Ok(()) => {
                match tokio::time::timeout(Duration::from_millis(timeout_ms), wait_for_turn(&agent))
                    .await
                {
                    Ok(answer) => answer,
                    Err(_) => Err(format!(
                        "no answer within {}; the session was stopped",
                        human_duration(timeout_ms)
                    )),
                }
            }
        };
        self.end_session(&agent).await;
        let changed = match before {
            Some(before) => changed_files(&worktree, &before).await,
            None => Vec::new(),
        };
        match answer {
            Ok(body) => Outcome {
                body: Some(body),
                error: None,
                changed,
            },
            Err(error) => Outcome {
                body: None,
                error: Some(error),
                changed,
            },
        }
    }

    // Killed, not slept, so a restart never revives it; its log stays in history.
    async fn end_session(&self, agent: &Arc<AgentThread>) {
        let _ = agent.kill().await;
        self.agents.remove(agent.id());
    }
}

async fn wait_for_turn(agent: &Arc<AgentThread>) -> Result<String, String> {
    loop {
        if let Some(outcome) = turn_outcome(&agent.events_since(0)) {
            return outcome;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn human_duration(ms: u64) -> String {
    match ms {
        ms if ms % 60_000 == 0 => format!("{} min", ms / 60_000),
        ms if ms % 1000 == 0 => format!("{} s", ms / 1000),
        ms => format!("{ms} ms"),
    }
}

async fn snapshot(worktree: &Path) -> Option<String> {
    let worktree = worktree.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if !icarus_git::turns::is_worktree(&worktree) {
            return None;
        }
        icarus_git::turns::snapshot_tree(&worktree)
            .map_err(|e| tracing::warn!("note reply snapshot: {e:#}"))
            .ok()
    })
    .await
    .ok()
    .flatten()
}

async fn changed_files(worktree: &Path, before: &str) -> Vec<String> {
    let Some(after) = snapshot(worktree).await else {
        return Vec::new();
    };
    if after == before {
        return Vec::new();
    }
    let worktree = worktree.to_path_buf();
    let before = before.to_owned();
    tokio::task::spawn_blocking(move || {
        icarus_git::git(
            &worktree,
            &["diff", "--name-only", "--no-renames", &before, &after],
        )
        .map(|out| out.lines().map(str::to_owned).collect())
        .unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Workspace;

    fn note() -> Note {
        Note {
            id: "note-1".into(),
            workspace_id: "ws-1".into(),
            path: "src/a.rs".into(),
            line: 3,
            end_line: 4,
            side: None,
            base: None,
            author: AUTHOR_USER.into(),
            agent_id: None,
            body: "Why is this here?".into(),
            excerpt: Some("let x = 1;\nlet y = 2;".into()),
            created_at: 1,
            updated_at: 1,
            resolved: false,
            resolved_at: None,
            replies: Vec::new(),
        }
    }

    fn reply(id: &str, author: &str, body: &str, status: Option<&str>) -> NoteReply {
        NoteReply {
            id: id.into(),
            note_id: "note-1".into(),
            author: author.into(),
            agent_id: None,
            body: body.into(),
            status: status.map(str::to_owned),
            error: None,
            changed_files: Vec::new(),
            created_at: 2,
            updated_at: 2,
        }
    }

    fn workspace_store() -> Store {
        let store = Store::in_memory().expect("store");
        let project = store
            .add_project("w", Path::new("/src/w"), None, None)
            .expect("project");
        store
            .insert_workspace(&Workspace {
                id: "ws-1".into(),
                project_id: project.id,
                name: "a".into(),
                path: PathBuf::from("/w/a"),
                branch: None,
                base_ref: None,
                status: None,
                title: None,
                issue: None,
                pull_request: None,
                created_at_ms: 1,
                pinned: false,
                manual_order: None,
                activity_at_ms: None,
                discovery: Default::default(),
            })
            .expect("workspace");
        store
    }

    #[test]
    fn notes_and_replies_round_trip_and_cascade() {
        let store = workspace_store();
        let mut first = note();
        store.add_note(&first).expect("add");
        let mut other = note();
        other.id = "note-2".into();
        other.path = "b.rs".into();
        store.add_note(&other).expect("add");
        let mut answer = reply("r2", AUTHOR_REPLY_AGENT, "", Some(STATUS_PENDING));
        store
            .add_note_reply(&reply("r1", AUTHOR_USER, "Explain", None))
            .expect("reply");
        store.add_note_reply(&answer).expect("pending");
        answer.status = Some(STATUS_DONE.into());
        answer.body = "Because.".into();
        answer.changed_files = vec!["src/a.rs".into()];
        assert!(store.update_note_reply(&answer).expect("update"));
        assert!(store.is_note_reply_agent("nobody").is_ok_and(|x| !x));

        first.replies = vec![reply("r1", AUTHOR_USER, "Explain", None), answer];
        assert_eq!(store.note("note-1").expect("get"), Some(first.clone()));
        let listed = store.notes("ws-1", None, false).expect("list");
        assert_eq!(
            listed.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            ["note-2", "note-1"],
            "file order"
        );
        assert_eq!(
            store.notes("ws-1", Some("b.rs"), false).expect("q").len(),
            1
        );

        assert!(store.set_note_resolved("note-2", true).expect("resolve"));
        assert_eq!(store.notes("ws-1", None, false).expect("q").len(), 1);
        let all = store.notes("ws-1", None, true).expect("q");
        assert!(all.iter().any(|n| n.id == "note-2" && n.resolved));

        assert!(store.edit_note("note-1", "Edited").expect("edit"));
        assert_eq!(
            store.note("note-1").expect("get").expect("note").body,
            "Edited"
        );
        assert!(store.remove_note("note-1").expect("remove"));
        assert_eq!(store.note_reply("r1").expect("get"), None, "cascade");
        store.remove_workspace("ws-1").expect("remove workspace");
        assert_eq!(store.note("note-2").expect("get"), None, "cascade");
    }

    #[test]
    fn unfinished_replies_fail_on_restart() {
        let store = workspace_store();
        store.add_note(&note()).expect("add");
        store
            .add_note_reply(&reply("r1", AUTHOR_REPLY_AGENT, "", Some(STATUS_RUNNING)))
            .expect("reply");
        store
            .add_note_reply(&reply("r2", AUTHOR_REPLY_AGENT, "ok", Some(STATUS_DONE)))
            .expect("reply");
        let mut running = reply("r3", AUTHOR_REPLY_AGENT, "", Some(STATUS_RUNNING));
        running.agent_id = Some("agent-7".into());
        store.add_note_reply(&running).expect("reply");
        let affected = store
            .fail_unfinished_note_replies("restarted")
            .expect("fail");
        assert_eq!(affected, [None, Some("agent-7".to_owned())]);
        let failed = store.note_reply("r1").expect("get").expect("reply");
        assert_eq!(failed.status.as_deref(), Some(STATUS_FAILED));
        assert_eq!(failed.error.as_deref(), Some("restarted"));
        let done = store.note_reply("r2").expect("get").expect("reply");
        assert_eq!(done.status.as_deref(), Some(STATUS_DONE));
    }

    #[test]
    fn thread_text_has_location_code_and_finished_replies() {
        let mut note = note();
        let mut done = reply("r2", AUTHOR_REPLY_AGENT, "It sets x.", Some(STATUS_DONE));
        done.agent_id = Some("agent-9".into());
        let mut failed = reply("r4", AUTHOR_REPLY_AGENT, "", Some(STATUS_FAILED));
        failed.error = Some("timed out".into());
        note.replies = vec![
            reply("r1", AUTHOR_USER, "What does it do?", None),
            done,
            reply("r3", AUTHOR_REPLY_AGENT, "", Some(STATUS_RUNNING)),
            failed,
        ];
        let text = thread_text(&note, Some("let x = 1;\nlet y = 2;"));
        assert!(text.starts_with("Note note-1 on src/a.rs:3-4\n"), "{text}");
        assert!(
            text.contains("```\n3  let x = 1;\n4  let y = 2;\n```"),
            "{text}"
        );
        assert!(text.contains("User:\nWhy is this here?"));
        assert!(text.contains("User:\nWhat does it do?"));
        assert!(text.contains("Reply agent:\nIt sets x."));
        assert!(text.contains("Reply agent (failed: timed out)"));
        assert_eq!(
            text.matches("Reply agent").count(),
            2,
            "running ones skipped"
        );

        let prompt = reply_prompt(&note, Path::new("/w/a"), None);
        assert!(prompt.contains("git worktree /w/a"));
        assert!(prompt.contains("<note-thread>\nNote note-1"));
        assert!(
            prompt.contains("1;\n4  let y"),
            "falls back to the stored excerpt"
        );
    }

    #[test]
    fn excerpts_fence_and_number_safely() {
        assert_eq!(excerpt_of("a\nb\nc\n", 2, 3).as_deref(), Some("b\nc"));
        assert_eq!(excerpt_of("a\n", 5, 6), None);
        let long = "x\n".repeat(500);
        let cut = excerpt_of(&long, 1, 400).expect("excerpt");
        assert!(cut.ends_with("… (truncated)"));
        assert_eq!(cut.lines().count(), MAX_EXCERPT_LINES + 1);
        assert_eq!(fence_for("a ```` b"), "`````");
        assert_eq!(numbered("a\nb", 9), " 9  a\n10  b");
    }

    fn event(kind: &str, data: Value) -> AgentEvent {
        AgentEvent {
            seq: 0,
            at_ms: 0,
            kind: kind.into(),
            data,
        }
    }

    #[test]
    fn turn_outcomes_take_the_last_answer_or_the_failure() {
        let message = |text: &str| {
            event(
                "message",
                json!({ "parent": null, "content": [{ "type": "text", "text": text }] }),
            )
        };
        let done = [
            event("session", json!({})),
            event("user_message", json!({ "text": "q" })),
            message("thinking out loud"),
            message("The answer."),
            event(
                "turn_end",
                json!({ "is_error": false, "result": "ignored" }),
            ),
        ];
        assert_eq!(turn_outcome(&done), Some(Ok("The answer.".to_owned())));
        assert_eq!(turn_outcome(&done[..4]), None);
        let failed = [
            event("user_message", json!({})),
            event("turn_end", json!({ "is_error": true, "result": "boom" })),
        ];
        assert_eq!(turn_outcome(&failed), Some(Err("boom".to_owned())));
        let exited = [
            event("user_message", json!({})),
            event("stderr", json!({ "line": "not logged in" })),
            event("exit", json!({ "code": 1 })),
        ];
        assert_eq!(
            turn_outcome(&exited),
            Some(Err(
                "the agent exited before answering: not logged in".to_owned()
            ))
        );
    }

    #[test]
    fn note_paths_are_relative_and_stay_inside() {
        let workspace = Workspace {
            id: "ws".into(),
            project_id: "p".into(),
            name: "a".into(),
            path: PathBuf::from("/nonexistent/w/a"),
            branch: None,
            base_ref: None,
            status: None,
            title: None,
            issue: None,
            pull_request: None,
            created_at_ms: 1,
            pinned: false,
            manual_order: None,
            activity_at_ms: None,
            discovery: Default::default(),
        };
        assert_eq!(
            relative_path(&workspace, "./src//a.rs").expect("rel"),
            "src/a.rs"
        );
        assert_eq!(
            relative_path(&workspace, "/nonexistent/w/a/src/a.rs").expect("abs"),
            "src/a.rs"
        );
        assert!(relative_path(&workspace, "../x").is_err());
        assert!(relative_path(&workspace, "/elsewhere/x").is_err());
        assert_eq!(location("a", 3, 3), "a:3");
        assert_eq!(location("a", 3, 5), "a:3-5");
        assert_eq!(human_duration(600_000), "10 min");
    }
}
