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
             CREATE INDEX IF NOT EXISTS attachments_by_workspace ON attachments(workspace_id, created_at);",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
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
            "INSERT INTO workspaces (id, project_id, name, path, branch, base_ref, status, title, issue, pull_request, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                workspace.created_at_ms as i64
            ],
        )?;
        Ok(())
    }

    pub fn update_workspace(&self, workspace: &Workspace) -> Result<()> {
        self.db().execute(
            "UPDATE workspaces SET name = ?2, branch = ?3, base_ref = ?4, status = ?5, title = ?6, issue = ?7, pull_request = ?8
             WHERE id = ?1",
            params![
                workspace.id,
                workspace.name,
                workspace.branch,
                workspace.base_ref,
                workspace.status,
                workspace.title,
                json_column(&workspace.issue),
                json_column(&workspace.pull_request)
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
        })
    }

    const WORKSPACE_COLUMNS: &str = "id, project_id, name, path, branch, base_ref, status, title, issue, pull_request, created_at_ms";

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
        };
        store.insert_workspace(&workspace).expect("insert");
        workspace.status = Some("in-review".into());
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
