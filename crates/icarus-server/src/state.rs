//! Durable server state: projects and workspaces, in SQLite.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    pub remote: Option<String>,
    pub base_ref: Option<String>,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Workspace {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub base_ref: Option<String>,
    pub status: Option<String>,
    pub title: Option<String>,
    /// Linked issue, e.g. `{ "provider": "linear", "id": "ABC-12", "title": "…" }`.
    pub issue: Option<Value>,
    /// Linked pull request, e.g. `{ "number": 12, "url": "…" }`.
    pub pull_request: Option<Value>,
    pub created_at_ms: u64,
    /// Pinned cards list first in their sidebar group.
    #[serde(default)]
    pub pinned: bool,
}

/// Context queued for a workspace's composer; `sent_at` is set once it went out with a message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    pub id: String,
    pub workspace_id: String,
    pub kind: String,
    pub title: String,
    pub text: String,
    pub source: Option<Value>,
    pub created_at: u64,
    pub sent_at: Option<u64>,
}

pub struct Store {
    connection: Mutex<Connection>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn json_column(value: &Option<Value>) -> Option<String> {
    value.as_ref().map(Value::to_string)
}

fn parse_json(text: Option<String>) -> Option<Value> {
    text.and_then(|t| serde_json::from_str(&t).ok())
}

/// Adds a column that older `state.db` files were created without.
fn add_missing_column(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        params![table, column],
        |row| row.get(0),
    )?;
    if !exists {
        connection.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))?;
    }
    Ok(())
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let connection =
            Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        Self::init(connection)
    }

    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(connection: Connection) -> Result<Self> {
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS projects (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL,
                 root TEXT NOT NULL UNIQUE,
                 remote TEXT,
                 base_ref TEXT,
                 created_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS workspaces (
                 id TEXT PRIMARY KEY,
                 project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                 name TEXT NOT NULL,
                 path TEXT NOT NULL UNIQUE,
                 branch TEXT,
                 base_ref TEXT,
                 status TEXT,
                 title TEXT,
                 issue TEXT,
                 pull_request TEXT,
                 created_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS kv (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS attachments (
                 id TEXT PRIMARY KEY,
                 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                 kind TEXT NOT NULL,
                 title TEXT NOT NULL,
                 text TEXT NOT NULL,
                 source TEXT,
                 created_at INTEGER NOT NULL,
                 sent_at INTEGER
             );
             CREATE INDEX IF NOT EXISTS attachments_by_workspace ON attachments(workspace_id, created_at);
             CREATE TABLE IF NOT EXISTS paired_clients (
                 id TEXT PRIMARY KEY,
                 label TEXT NOT NULL,
                 token_hash TEXT NOT NULL UNIQUE,
                 created_at_ms INTEGER NOT NULL,
                 last_seen_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS notifications (
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 subject TEXT NOT NULL,
                 title TEXT NOT NULL,
                 body TEXT NOT NULL,
                 workspace_id TEXT,
                 data TEXT,
                 created_at INTEGER NOT NULL,
                 read INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS notifications_by_time ON notifications(created_at);
             CREATE TABLE IF NOT EXISTS notification_keys (
                 key TEXT PRIMARY KEY,
                 last_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS review_sessions (
                 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                 pr TEXT NOT NULL,
                 data TEXT NOT NULL,
                 updated_at_ms INTEGER NOT NULL,
                 PRIMARY KEY (workspace_id, pr)
             );
             CREATE TABLE IF NOT EXISTS accounts (
                 id TEXT PRIMARY KEY,
                 provider TEXT NOT NULL,
                 label TEXT NOT NULL,
                 home TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS notes (
                 id TEXT PRIMARY KEY,
                 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                 path TEXT NOT NULL,
                 line INTEGER NOT NULL,
                 end_line INTEGER NOT NULL,
                 side TEXT,
                 base TEXT,
                 author TEXT NOT NULL,
                 agent_id TEXT,
                 body TEXT NOT NULL,
                 excerpt TEXT,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL,
                 resolved_at INTEGER
             );
             CREATE INDEX IF NOT EXISTS notes_by_workspace ON notes(workspace_id, path, line);
             CREATE TABLE IF NOT EXISTS note_replies (
                 id TEXT PRIMARY KEY,
                 note_id TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
                 author TEXT NOT NULL,
                 agent_id TEXT,
                 body TEXT NOT NULL,
                 status TEXT,
                 error TEXT,
                 changed_files TEXT,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS note_replies_by_note ON note_replies(note_id, created_at);
             CREATE INDEX IF NOT EXISTS note_replies_by_agent ON note_replies(agent_id);",
        )?;
        add_missing_column(
            &connection,
            "workspaces",
            "pinned",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub(crate) fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn add_project(
        &self,
        name: &str,
        root: &Path,
        remote: Option<&str>,
        base_ref: Option<&str>,
    ) -> Result<Project> {
        if let Some(existing) = self.project_by_root(root)? {
            return Ok(existing);
        }
        let project = Project {
            id: format!("proj-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]),
            name: name.to_string(),
            root: root.to_path_buf(),
            remote: remote.map(str::to_string),
            base_ref: base_ref.map(str::to_string),
            created_at_ms: now_ms(),
        };
        self.db().execute(
            "INSERT INTO projects (id, name, root, remote, base_ref, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                project.id,
                project.name,
                project.root.to_string_lossy(),
                project.remote,
                project.base_ref,
                project.created_at_ms as i64
            ],
        )?;
        Ok(project)
    }

    fn project_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
        Ok(Project {
            id: row.get(0)?,
            name: row.get(1)?,
            root: PathBuf::from(row.get::<_, String>(2)?),
            remote: row.get(3)?,
            base_ref: row.get(4)?,
            created_at_ms: row.get::<_, i64>(5)? as u64,
        })
    }

    pub fn project_by_root(&self, root: &Path) -> Result<Option<Project>> {
        Ok(self
            .db()
            .query_row(
                "SELECT id, name, root, remote, base_ref, created_at_ms FROM projects WHERE root = ?1",
                params![root.to_string_lossy()],
                Self::project_from_row,
            )
            .optional()?)
    }

    /// Finds a project by id, name or canonical remote.
    pub fn project(&self, key: &str) -> Result<Option<Project>> {
        Ok(self
            .db()
            .query_row(
                "SELECT id, name, root, remote, base_ref, created_at_ms FROM projects
                 WHERE id = ?1 OR name = ?1 OR remote = ?1 ORDER BY created_at_ms LIMIT 1",
                params![key],
                Self::project_from_row,
            )
            .optional()?)
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT id, name, root, remote, base_ref, created_at_ms FROM projects ORDER BY name",
        )?;
        let rows = statement.query_map([], Self::project_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn remove_project(&self, id: &str) -> Result<bool> {
        Ok(self
            .db()
            .execute("DELETE FROM projects WHERE id = ?1", params![id])?
            > 0)
    }

    pub fn insert_workspace(&self, workspace: &Workspace) -> Result<()> {
        self.db().execute(
            "INSERT INTO workspaces (id, project_id, name, path, branch, base_ref, status, title, issue, pull_request, created_at_ms, pinned)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                workspace.id,
                workspace.project_id,
                workspace.name,
                workspace.path.to_string_lossy(),
                workspace.branch,
                workspace.base_ref,
                workspace.status,
                workspace.title,
                json_column(&workspace.issue),
                json_column(&workspace.pull_request),
                workspace.created_at_ms as i64,
                workspace.pinned
            ],
        )?;
        Ok(())
    }

    pub fn update_workspace(&self, workspace: &Workspace) -> Result<()> {
        self.db().execute(
            "UPDATE workspaces SET name = ?2, branch = ?3, base_ref = ?4, status = ?5, title = ?6, issue = ?7, pull_request = ?8, pinned = ?9
             WHERE id = ?1",
            params![
                workspace.id,
                workspace.name,
                workspace.branch,
                workspace.base_ref,
                workspace.status,
                workspace.title,
                json_column(&workspace.issue),
                json_column(&workspace.pull_request),
                workspace.pinned
            ],
        )?;
        Ok(())
    }

    fn workspace_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Workspace> {
        Ok(Workspace {
            id: row.get(0)?,
            project_id: row.get(1)?,
            name: row.get(2)?,
            path: PathBuf::from(row.get::<_, String>(3)?),
            branch: row.get(4)?,
            base_ref: row.get(5)?,
            status: row.get(6)?,
            title: row.get(7)?,
            issue: parse_json(row.get(8)?),
            pull_request: parse_json(row.get(9)?),
            created_at_ms: row.get::<_, i64>(10)? as u64,
            pinned: row.get(11)?,
        })
    }

    const WORKSPACE_COLUMNS: &str = "id, project_id, name, path, branch, base_ref, status, title, issue, pull_request, created_at_ms, pinned";

    /// Finds a workspace by id, name or path.
    pub fn workspace(&self, key: &str) -> Result<Option<Workspace>> {
        let sql = format!(
            "SELECT {} FROM workspaces WHERE id = ?1 OR name = ?1 OR path = ?1 ORDER BY created_at_ms DESC LIMIT 1",
            Self::WORKSPACE_COLUMNS
        );
        Ok(self
            .db()
            .query_row(&sql, params![key], Self::workspace_from_row)
            .optional()?)
    }

    pub fn workspaces(&self, project_id: Option<&str>) -> Result<Vec<Workspace>> {
        let db = self.db();
        let sql = format!(
            "SELECT {} FROM workspaces WHERE ?1 IS NULL OR project_id = ?1 ORDER BY created_at_ms",
            Self::WORKSPACE_COLUMNS
        );
        let mut statement = db.prepare(&sql)?;
        let rows = statement.query_map(params![project_id], Self::workspace_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn remove_workspace(&self, id: &str) -> Result<bool> {
        Ok(self
            .db()
            .execute("DELETE FROM workspaces WHERE id = ?1", params![id])?
            > 0)
    }

    pub fn add_attachment(
        &self,
        workspace_id: &str,
        kind: &str,
        title: &str,
        text: &str,
        source: Option<Value>,
    ) -> Result<Attachment> {
        let attachment = Attachment {
            id: format!("att-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
            workspace_id: workspace_id.to_string(),
            kind: kind.to_string(),
            title: title.to_string(),
            text: text.to_string(),
            source,
            created_at: now_ms(),
            sent_at: None,
        };
        self.db().execute(
            "INSERT INTO attachments (id, workspace_id, kind, title, text, source, created_at, sent_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
            params![
                attachment.id,
                attachment.workspace_id,
                attachment.kind,
                attachment.title,
                attachment.text,
                json_column(&attachment.source),
                attachment.created_at as i64
            ],
        )?;
        Ok(attachment)
    }

    fn attachment_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Attachment> {
        Ok(Attachment {
            id: row.get(0)?,
            workspace_id: row.get(1)?,
            kind: row.get(2)?,
            title: row.get(3)?,
            text: row.get(4)?,
            source: parse_json(row.get(5)?),
            created_at: row.get::<_, i64>(6)? as u64,
            sent_at: row.get::<_, Option<i64>>(7)?.map(|t| t as u64),
        })
    }

    pub fn attachment(&self, id: &str) -> Result<Option<Attachment>> {
        Ok(self
            .db()
            .query_row(
                "SELECT id, workspace_id, kind, title, text, source, created_at, sent_at
                 FROM attachments WHERE id = ?1",
                params![id],
                Self::attachment_from_row,
            )
            .optional()?)
    }

    pub fn attachments(&self, workspace_id: &str, include_sent: bool) -> Result<Vec<Attachment>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT id, workspace_id, kind, title, text, source, created_at, sent_at
             FROM attachments WHERE workspace_id = ?1 AND (?2 OR sent_at IS NULL)
             ORDER BY created_at, rowid",
        )?;
        let rows = statement.query_map(
            params![workspace_id, include_sent],
            Self::attachment_from_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn remove_attachment(&self, id: &str) -> Result<bool> {
        Ok(self
            .db()
            .execute("DELETE FROM attachments WHERE id = ?1", params![id])?
            > 0)
    }

    pub fn mark_attachments_sent(&self, ids: &[String]) -> Result<usize> {
        let db = self.db();
        let now = now_ms() as i64;
        let mut marked = 0;
        for id in ids {
            marked += db.execute(
                "UPDATE attachments SET sent_at = ?2 WHERE id = ?1 AND sent_at IS NULL",
                params![id, now],
            )?;
        }
        Ok(marked)
    }

    pub fn kv_get(&self, key: &str) -> Result<Option<Value>> {
        let text: Option<String> = self
            .db()
            .query_row("SELECT value FROM kv WHERE key = ?1", params![key], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(parse_json(text))
    }

    pub fn kv_set(&self, key: &str, value: &Value) -> Result<()> {
        self.db().execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value.to_string()],
        )?;
        Ok(())
    }

    pub fn kv_delete(&self, key: &str) -> Result<bool> {
        Ok(self
            .db()
            .execute("DELETE FROM kv WHERE key = ?1", params![key])?
            > 0)
    }

    /// A workspace's review of one pull request (`owner/name#N`), as stored JSON.
    pub fn review_session(&self, workspace_id: &str, pr: &str) -> Result<Option<Value>> {
        let text: Option<String> = self
            .db()
            .query_row(
                "SELECT data FROM review_sessions WHERE workspace_id = ?1 AND pr = ?2",
                params![workspace_id, pr],
                |row| row.get(0),
            )
            .optional()?;
        Ok(parse_json(text))
    }

    /// Read-modify-write of one review session under the store lock; `None` from `f` deletes it.
    pub fn update_review_session<R, E>(
        &self,
        workspace_id: &str,
        pr: &str,
        f: impl FnOnce(Option<Value>) -> Result<(Option<Value>, R), E>,
    ) -> Result<Result<R, E>> {
        let db = self.db();
        let current: Option<String> = db
            .query_row(
                "SELECT data FROM review_sessions WHERE workspace_id = ?1 AND pr = ?2",
                params![workspace_id, pr],
                |row| row.get(0),
            )
            .optional()?;
        let (next, result) = match f(parse_json(current)) {
            Ok(done) => done,
            Err(error) => return Ok(Err(error)),
        };
        match next {
            Some(value) => db.execute(
                "INSERT INTO review_sessions (workspace_id, pr, data, updated_at_ms) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(workspace_id, pr) DO UPDATE SET data = excluded.data, updated_at_ms = excluded.updated_at_ms",
                params![workspace_id, pr, value.to_string(), now_ms() as i64],
            )?,
            None => db.execute(
                "DELETE FROM review_sessions WHERE workspace_id = ?1 AND pr = ?2",
                params![workspace_id, pr],
            )?,
        };
        Ok(Ok(result))
    }

    /// Every stored review session of a workspace, keyed by pull request.
    pub fn review_sessions(&self, workspace_id: &str) -> Result<Vec<(String, Value)>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT pr, data FROM review_sessions WHERE workspace_id = ?1 ORDER BY updated_at_ms",
        )?;
        let rows = statement.query_map(params![workspace_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter_map(|(pr, data)| Some((pr, serde_json::from_str(&data).ok()?)))
            .collect())
    }

    pub fn add_paired_client(&self, label: &str, token_hash: &str) -> Result<PairedClient> {
        let client = PairedClient {
            id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            label: label.to_string(),
            created_at_ms: now_ms(),
            last_seen_ms: now_ms(),
        };
        self.db().execute(
            "INSERT INTO paired_clients (id, label, token_hash, created_at_ms, last_seen_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                client.id,
                client.label,
                token_hash,
                client.created_at_ms as i64,
                client.last_seen_ms as i64
            ],
        )?;
        Ok(client)
    }

    fn paired_client_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PairedClient> {
        Ok(PairedClient {
            id: row.get(0)?,
            label: row.get(1)?,
            created_at_ms: row.get::<_, i64>(2)? as u64,
            last_seen_ms: row.get::<_, i64>(3)? as u64,
        })
    }

    pub fn touch_paired_client(&self, token_hash: &str) -> Result<Option<PairedClient>> {
        let db = self.db();
        db.execute(
            "UPDATE paired_clients SET last_seen_ms = ?1 WHERE token_hash = ?2",
            params![now_ms() as i64, token_hash],
        )?;
        Ok(db
            .query_row(
                "SELECT id, label, created_at_ms, last_seen_ms FROM paired_clients
                 WHERE token_hash = ?1",
                params![token_hash],
                Self::paired_client_from_row,
            )
            .optional()?)
    }

    pub fn paired_clients(&self) -> Result<Vec<PairedClient>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT id, label, created_at_ms, last_seen_ms FROM paired_clients
             ORDER BY created_at_ms",
        )?;
        let rows = statement.query_map([], Self::paired_client_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn remove_paired_client(&self, id: &str) -> Result<bool> {
        Ok(self
            .db()
            .execute("DELETE FROM paired_clients WHERE id = ?1", params![id])?
            > 0)
    }
}

/// Notifications kept; older ones are dropped as new ones arrive.
const NOTIFICATIONS_KEPT: i64 = 500;
/// Dedupe keys older than this are forgotten.
const NOTIFICATION_KEY_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;

impl Store {
    /// Claims `key` unless it was claimed less than `window_ms` ago; true means go ahead.
    pub fn reserve_notification_key(&self, key: &str, now: u64, window_ms: u64) -> Result<bool> {
        let db = self.db();
        let last: Option<i64> = db
            .query_row(
                "SELECT last_at FROM notification_keys WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        if last.is_some_and(|last| now.saturating_sub(last as u64) < window_ms) {
            return Ok(false);
        }
        db.execute(
            "INSERT INTO notification_keys (key, last_at) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET last_at = excluded.last_at",
            params![key, now as i64],
        )?;
        db.execute(
            "DELETE FROM notification_keys WHERE last_at < ?1",
            params![now.saturating_sub(NOTIFICATION_KEY_TTL_MS) as i64],
        )?;
        Ok(true)
    }

    pub fn add_notification(&self, notification: &Notification) -> Result<()> {
        let db = self.db();
        db.execute(
            "INSERT INTO notifications (id, kind, subject, title, body, workspace_id, data, created_at, read)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                notification.id,
                notification.kind,
                notification.subject,
                notification.title,
                notification.body,
                notification.workspace_id,
                json_column(&notification.data),
                notification.created_at as i64,
                notification.read
            ],
        )?;
        db.execute(
            "DELETE FROM notifications WHERE rowid NOT IN
             (SELECT rowid FROM notifications ORDER BY created_at DESC, rowid DESC LIMIT ?1)",
            params![NOTIFICATIONS_KEPT],
        )?;
        Ok(())
    }

    /// Newest first.
    pub fn notifications(&self, unread_only: bool, limit: usize) -> Result<Vec<Notification>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT id, kind, subject, title, body, workspace_id, data, created_at, read
             FROM notifications WHERE NOT (?1 AND read)
             ORDER BY created_at DESC, rowid DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![unread_only, limit as i64], |row| {
            Ok(Notification {
                id: row.get(0)?,
                kind: row.get(1)?,
                subject: row.get(2)?,
                title: row.get(3)?,
                body: row.get(4)?,
                workspace_id: row.get(5)?,
                data: parse_json(row.get(6)?),
                created_at: row.get::<_, i64>(7)? as u64,
                read: row.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn unread_notifications(&self) -> Result<u64> {
        let count: i64 = self.db().query_row(
            "SELECT COUNT(*) FROM notifications WHERE NOT read",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    /// Marks one notification read, or all of them with `None`; returns how many changed.
    pub fn mark_notifications_read(&self, id: Option<&str>) -> Result<usize> {
        Ok(self.db().execute(
            "UPDATE notifications SET read = 1 WHERE NOT read AND (?1 IS NULL OR id = ?1)",
            params![id],
        )?)
    }

    pub fn notification_exists(&self, id: &str) -> Result<bool> {
        Ok(self
            .db()
            .query_row(
                "SELECT 1 FROM notifications WHERE id = ?1",
                params![id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn clear_notifications(&self) -> Result<usize> {
        Ok(self.db().execute("DELETE FROM notifications", [])?)
    }

    pub fn add_account(&self, account: &Account) -> Result<()> {
        self.db().execute(
            "INSERT INTO accounts (id, provider, label, home, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                account.id,
                account.provider,
                account.label,
                account.home.to_string_lossy(),
                account.created_at_ms as i64
            ],
        )?;
        Ok(())
    }

    fn account_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
        Ok(Account {
            id: row.get(0)?,
            provider: row.get(1)?,
            label: row.get(2)?,
            home: PathBuf::from(row.get::<_, String>(3)?),
            created_at_ms: row.get::<_, i64>(4)? as u64,
        })
    }

    /// Finds an account by id or label.
    pub fn account(&self, key: &str) -> Result<Option<Account>> {
        Ok(self
            .db()
            .query_row(
                "SELECT id, provider, label, home, created_at_ms FROM accounts
                 WHERE id = ?1 OR label = ?1 ORDER BY created_at_ms LIMIT 1",
                params![key],
                Self::account_from_row,
            )
            .optional()?)
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT id, provider, label, home, created_at_ms FROM accounts ORDER BY provider, created_at_ms",
        )?;
        let rows = statement.query_map([], Self::account_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn remove_account(&self, id: &str) -> Result<bool> {
        Ok(self
            .db()
            .execute("DELETE FROM accounts WHERE id = ?1", params![id])?
            > 0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Notification {
    pub id: String,
    pub kind: String,
    /// What it is about, e.g. an agent id or `owner/repo#12@sha`; with `kind` it is the dedupe key.
    pub subject: String,
    pub title: String,
    pub body: String,
    pub workspace_id: Option<String>,
    pub data: Option<Value>,
    pub created_at: u64,
    pub read: bool,
}

/// A managed agent login: its own config dir, signed in through the provider's own CLI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Account {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub home: PathBuf,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairedClient {
    pub id: String,
    pub label: String,
    pub created_at_ms: u64,
    pub last_seen_ms: u64,
}

/// A DNS-safe worktree slug, so `<slug>.localhost` dev URLs keep resolving (plan D3).
pub fn workspace_slug(name: &str, max_len: usize) -> String {
    let mut slug = String::new();
    let mut last_dash = true;
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let mut slug: String = slug
        .trim_matches('-')
        .chars()
        .take(max_len.max(1))
        .collect();
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "workspace".to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_and_workspaces_round_trip() {
        let store = Store::in_memory().expect("store");
        let project = store
            .add_project(
                "widgets",
                Path::new("/src/widgets"),
                Some("github.com/acme/widgets"),
                None,
            )
            .expect("project");
        let again = store
            .add_project("widgets", Path::new("/src/widgets"), None, None)
            .expect("same root");
        assert_eq!(project.id, again.id);
        assert_eq!(
            store
                .project("github.com/acme/widgets")
                .expect("q")
                .map(|p| p.id),
            Some(project.id.clone())
        );

        let mut workspace = Workspace {
            id: "ws-1".into(),
            project_id: project.id.clone(),
            name: "abc-12".into(),
            path: PathBuf::from("/w/abc-12"),
            branch: Some("abc-12".into()),
            base_ref: Some("origin/main".into()),
            status: Some("todo".into()),
            title: None,
            issue: Some(serde_json::json!({ "provider": "linear", "id": "ABC-12" })),
            pull_request: None,
            created_at_ms: 1,
            pinned: false,
        };
        store.insert_workspace(&workspace).expect("insert");
        workspace.status = Some("in-review".into());
        workspace.pinned = true;
        store.update_workspace(&workspace).expect("update");
        assert_eq!(
            store.workspace("abc-12").expect("q"),
            Some(workspace.clone())
        );
        assert_eq!(store.workspaces(Some(&project.id)).expect("list").len(), 1);
        assert!(store.remove_project(&project.id).expect("remove"));
        assert!(store.workspaces(None).expect("list").is_empty(), "cascade");
    }

    #[test]
    fn attachments_queue_per_workspace_and_cascade() {
        let store = Store::in_memory().expect("store");
        let project = store
            .add_project("widgets", Path::new("/src/widgets"), None, None)
            .expect("project");
        let workspace = Workspace {
            id: "ws-1".into(),
            project_id: project.id.clone(),
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
        };
        store.insert_workspace(&workspace).expect("insert");
        let first = store
            .add_attachment("ws-1", "text", "Note", "hello", None)
            .expect("add");
        let second = store
            .add_attachment(
                "ws-1",
                "file",
                "a.rs",
                "fn a() {}",
                Some(serde_json::json!({ "path": "a.rs" })),
            )
            .expect("add");
        assert_eq!(
            store.attachment(&second.id).expect("get"),
            Some(second.clone())
        );
        assert_eq!(
            store
                .mark_attachments_sent(std::slice::from_ref(&first.id))
                .expect("mark"),
            1
        );
        let pending = store.attachments("ws-1", false).expect("list");
        assert_eq!(pending, vec![second.clone()]);
        assert_eq!(store.attachments("ws-1", true).expect("list").len(), 2);
        assert!(store.remove_attachment(&second.id).expect("remove"));
        assert!(!store.remove_attachment(&second.id).expect("again"));
        store.remove_workspace("ws-1").expect("remove workspace");
        assert_eq!(store.attachment(&first.id).expect("get"), None, "cascade");
    }

    #[test]
    fn databases_from_before_pinning_gain_the_column() {
        let connection = Connection::open_in_memory().expect("db");
        connection
            .execute_batch(
                "CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL, root TEXT NOT NULL UNIQUE,
                     remote TEXT, base_ref TEXT, created_at_ms INTEGER NOT NULL);
                 CREATE TABLE workspaces (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, name TEXT NOT NULL,
                     path TEXT NOT NULL UNIQUE, branch TEXT, base_ref TEXT, status TEXT, title TEXT,
                     issue TEXT, pull_request TEXT, created_at_ms INTEGER NOT NULL);
                 INSERT INTO projects VALUES ('p', 'widgets', '/src/widgets', NULL, NULL, 1);
                 INSERT INTO workspaces VALUES ('ws-1', 'p', 'a', '/w/a', NULL, NULL, NULL, NULL, NULL, NULL, 1);",
            )
            .expect("old schema");
        let store = Store::init(connection).expect("migrates");
        let workspace = store.workspace("ws-1").expect("q").expect("kept");
        assert!(!workspace.pinned);
        let reopened = Store::init(store.connection.into_inner().expect("connection"))
            .expect("a second open leaves the column alone");
        assert_eq!(reopened.workspaces(None).expect("list").len(), 1);
    }

    #[test]
    fn kv_values_round_trip_and_overwrite() {
        let store = Store::in_memory().expect("store");
        assert_eq!(store.kv_get("rules").expect("get"), None);
        store
            .kv_set("rules", &serde_json::json!({ "ledger": ["a"] }))
            .expect("set");
        store
            .kv_set("rules", &serde_json::json!({ "ledger": ["b"] }))
            .expect("overwrite");
        assert_eq!(
            store.kv_get("rules").expect("get"),
            Some(serde_json::json!({ "ledger": ["b"] }))
        );
    }

    fn notification(id: &str, created_at: u64) -> Notification {
        Notification {
            id: id.into(),
            kind: "agent_turn_done".into(),
            subject: "agent-1".into(),
            title: "Agent finished".into(),
            body: String::new(),
            workspace_id: None,
            data: Some(serde_json::json!({ "agent_id": "agent-1" })),
            created_at,
            read: false,
        }
    }

    #[test]
    fn notifications_list_newest_first_and_mark_read() {
        let store = Store::in_memory().expect("store");
        store.add_notification(&notification("n1", 1)).expect("add");
        store.add_notification(&notification("n2", 2)).expect("add");
        let listed = store.notifications(false, 10).expect("list");
        assert_eq!(
            listed.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            ["n2", "n1"]
        );
        assert_eq!(listed[0].data, notification("n2", 2).data);
        assert_eq!(store.mark_notifications_read(Some("n1")).expect("read"), 1);
        assert_eq!(store.unread_notifications().expect("count"), 1);
        assert_eq!(store.notifications(true, 10).expect("unread")[0].id, "n2");
        assert_eq!(store.notifications(false, 1).expect("limit").len(), 1);
        assert_eq!(store.mark_notifications_read(None).expect("all"), 1);
        assert_eq!(store.unread_notifications().expect("count"), 0);
        assert_eq!(store.clear_notifications().expect("clear"), 2);
        assert!(store.notifications(false, 10).expect("list").is_empty());
    }

    #[test]
    fn notification_keys_hold_for_their_window() {
        let store = Store::in_memory().expect("store");
        assert!(
            store
                .reserve_notification_key("k", 1_000, 5_000)
                .expect("r")
        );
        assert!(
            !store
                .reserve_notification_key("k", 5_999, 5_000)
                .expect("r")
        );
        assert!(
            store
                .reserve_notification_key("other", 5_999, 5_000)
                .expect("r")
        );
        assert!(
            store
                .reserve_notification_key("k", 6_000, 5_000)
                .expect("r")
        );
    }

    #[test]
    fn notifications_keep_only_the_newest() {
        let store = Store::in_memory().expect("store");
        for i in 0..(NOTIFICATIONS_KEPT as u64 + 5) {
            store
                .add_notification(&notification(&format!("n{i}"), i))
                .expect("add");
        }
        let all = store.notifications(false, 10_000).expect("list");
        assert_eq!(all.len() as i64, NOTIFICATIONS_KEPT);
        assert_eq!(all.last().map(|n| n.id.as_str()), Some("n5"));
    }

    #[test]
    fn accounts_round_trip_by_id_or_label() {
        let store = Store::in_memory().expect("store");
        let account = Account {
            id: "acct-1".into(),
            provider: "claude".into(),
            label: "Work".into(),
            home: PathBuf::from("/s/claude-profiles/acct-1/home"),
            created_at_ms: 1,
        };
        store.add_account(&account).expect("add");
        assert_eq!(store.account("Work").expect("q"), Some(account.clone()));
        assert_eq!(store.accounts().expect("list"), vec![account]);
        assert!(store.remove_account("acct-1").expect("remove"));
        assert_eq!(store.account("acct-1").expect("q"), None);
    }

    #[test]
    fn slugs_are_dns_safe_and_bounded() {
        assert_eq!(
            workspace_slug("ABC-123 Fix the login page!", 63),
            "abc-123-fix-the-login-page"
        );
        assert_eq!(workspace_slug("___", 63), "workspace");
        let long = workspace_slug(&"a-".repeat(80), 63);
        assert!(long.len() <= 63 && !long.ends_with('-'));
    }
}
