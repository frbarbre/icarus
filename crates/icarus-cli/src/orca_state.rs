//! Reads Orca's `profile-state.db` (read-only) for `icarus settings import-orca-state`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

/// Orca's profile databases on this machine (macOS: `~/Library/Application Support/orca/profiles/*`).
fn default_databases() -> Vec<PathBuf> {
    let Some(base) = dirs::data_dir().map(|d| d.join("orca").join("profiles")) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path().join("profile-state.db"))
        .filter(|path| path.is_file())
        .collect()
}

/// The `worktreeMeta` and `repos` documents, as the server's `settings.import_orca_state` takes them.
pub fn read(from: Option<&Path>) -> Result<Value> {
    let path = match from {
        Some(path) => path.to_owned(),
        None => {
            let found = default_databases();
            match found.as_slice() {
                [one] => one.clone(),
                [] => bail!("no Orca profile-state.db on this machine; pass --from"),
                _ => bail!(
                    "several Orca profiles found; pass --from with one of:\n{}",
                    found
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            }
        }
    };
    // Read-only, so Orca's own database (and its write-ahead log) is never changed.
    let connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", path.display()))?;
    let document = |domain: &str| -> Result<Value> {
        let payload: Option<String> = connection
            .query_row(
                "SELECT payload FROM profile_state_documents WHERE domain = ?1",
                [domain],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
            .with_context(|| format!("{} isn't an Orca profile database", path.display()))?;
        payload
            .map(|text| serde_json::from_str(&text))
            .transpose()
            .with_context(|| format!("Orca's {domain} document isn't JSON"))
            .map(Option::unwrap_or_default)
    };
    Ok(json!({
        "worktree_meta": document("worktreeMeta")?,
        "repos": document("repos")?,
    }))
}

pub fn summary(value: &Value) -> String {
    let dry = value["dry_run"] == true;
    let matched = value["matched"].as_array().cloned().unwrap_or_default();
    let mut lines = vec![format!(
        "{} Orca workspaces: {} matched, {} with no Icarus workspace.",
        value["orca_worktrees"].as_u64().unwrap_or(0),
        matched.len(),
        value["unmatched"].as_u64().unwrap_or(0)
    )];
    for m in &matched {
        let mut changes = Vec::new();
        if let Some(status) = m["status"].as_str() {
            changes.push(format!("status → {status}"));
        }
        if m["title"].is_string() {
            changes.push("title".to_owned());
        }
        if let Some(issue) = m["linear"].as_str() {
            changes.push(format!("Linear {issue}"));
        }
        let notes = m["notes"].as_u64().unwrap_or(0);
        let replies = m["replies"].as_u64().unwrap_or(0);
        if notes + replies > 0 {
            changes.push(format!("{notes} notes, {replies} replies"));
        }
        if changes.is_empty() {
            changes.push("nothing new".to_owned());
        }
        lines.push(format!(
            "  {} ({}): {}",
            m["workspace"].as_str().unwrap_or_default(),
            m["matched_by"].as_str().unwrap_or_default(),
            changes.join(", ")
        ));
    }
    if dry {
        lines.push("Dry run: nothing was changed.".to_owned());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_two_documents_and_tolerates_a_missing_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("profile-state.db");
        let db = Connection::open(&path).expect("create");
        db.execute_batch(
            "CREATE TABLE profile_state_documents (domain TEXT PRIMARY KEY NOT NULL, payload TEXT NOT NULL, domain_version INTEGER NOT NULL, revision INTEGER NOT NULL, updated_at INTEGER NOT NULL, content_hash TEXT NOT NULL);
             INSERT INTO profile_state_documents VALUES ('worktreeMeta', '{\"r::/w\":{\"workspaceStatus\":\"todo\"}}', 1, 1, 0, 'h');",
        )
        .expect("seed");
        drop(db);
        let documents = read(Some(&path)).expect("read");
        assert_eq!(
            documents["worktree_meta"]["r::/w"]["workspaceStatus"],
            "todo"
        );
        assert_eq!(documents["repos"], Value::Null, "no repos document is fine");
        assert!(read(Some(&dir.path().join("missing.db"))).is_err());
    }
}
