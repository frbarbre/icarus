use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{git, git_env, git_ok};

pub const TURN_REF_PREFIX: &str = "refs/worktree/icarus-turns/";
pub const KEEP_TURNS: usize = 50;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnMeta {
    pub prompt: String,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub started_at: u64,
    pub completed_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Turn {
    pub name: String,
    pub ref_name: String,
    pub oid: String,
    pub parent: String,
    pub meta: TurnMeta,
    pub files: usize,
    pub added: u64,
    pub removed: u64,
}

struct TempIndex(PathBuf);

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(self.0.with_extension("index.lock"));
    }
}

fn identity() -> [(&'static str, &'static OsStr); 4] {
    [
        ("GIT_AUTHOR_NAME", OsStr::new("Icarus")),
        ("GIT_AUTHOR_EMAIL", OsStr::new("icarus@localhost")),
        ("GIT_COMMITTER_NAME", OsStr::new("Icarus")),
        ("GIT_COMMITTER_EMAIL", OsStr::new("icarus@localhost")),
    ]
}

// A temporary index keeps the user's real index, and so their staging, untouched.
pub fn snapshot_tree(worktree: &Path) -> Result<String> {
    let real = git(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?;
    let real = PathBuf::from(real.trim());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let temp =
        TempIndex(real.with_file_name(format!("icarus-turn-{}-{nanos}.index", std::process::id())));
    // Seeding from the real index keeps its stat cache, so `add -A` rehashes only what changed.
    if real.exists() {
        std::fs::copy(&real, &temp.0).context("copy index")?;
    }
    let env = [("GIT_INDEX_FILE", temp.0.as_os_str())];
    git_env(worktree, &["add", "-A", "--", "."], &env)?;
    Ok(git_env(worktree, &["write-tree"], &env)?.trim().to_string())
}

fn commit_tree(worktree: &Path, tree: &str, parent: Option<&str>, message: &str) -> Result<String> {
    let mut args = vec!["commit-tree", tree, "-m", message];
    if let Some(parent) = parent {
        args.extend(["-p", parent]);
    }
    Ok(git_env(worktree, &args, &identity())?.trim().to_string())
}

pub fn start_turn(worktree: &Path) -> Result<String> {
    let tree = snapshot_tree(worktree)?;
    commit_tree(worktree, &tree, None, "icarus turn start")
}

pub fn finish_turn(worktree: &Path, start: &str, meta: &TurnMeta) -> Result<Option<Turn>> {
    let start_tree = git(worktree, &["rev-parse", &format!("{start}^{{tree}}")])?;
    let tree = snapshot_tree(worktree)?;
    if tree == start_tree.trim() {
        return Ok(None);
    }
    let message = serde_json::to_string(meta)?;
    let oid = commit_tree(worktree, &tree, Some(start), &message)?;
    let short = oid.get(..8).unwrap_or(&oid);
    let ref_name = format!("{TURN_REF_PREFIX}{:015}-{short}", meta.completed_at);
    git(worktree, &["update-ref", &ref_name, &oid])?;
    prune(worktree, KEEP_TURNS)?;
    let name = ref_name.trim_start_matches(TURN_REF_PREFIX).to_string();
    resolve_turn(worktree, &name)
        .map(Some)
        .with_context(|| format!("read back {ref_name}"))
}

fn turn_refs(worktree: &Path) -> Result<Vec<(String, String)>> {
    let output = git(
        worktree,
        &[
            "for-each-ref",
            "--sort=-refname",
            "--format=%(refname) %(objectname)",
            TURN_REF_PREFIX,
        ],
    )?;
    Ok(output
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(r, o)| (r.to_string(), o.to_string()))
        .collect())
}

pub fn prune(worktree: &Path, keep: usize) -> Result<()> {
    for (ref_name, _) in turn_refs(worktree)?.into_iter().skip(keep) {
        git(worktree, &["update-ref", "-d", &ref_name])?;
    }
    Ok(())
}

fn read_turn(worktree: &Path, ref_name: &str, oid: &str) -> Result<Turn> {
    let parent = crate::first_parent(worktree, oid).context("turn without a start snapshot")?;
    let message = git(worktree, &["show", "-s", "--format=%B", oid])?;
    let meta: TurnMeta = serde_json::from_str(message.trim()).unwrap_or_default();
    let rows = crate::commit_rows(worktree, Some(&parent), oid)?;
    Ok(Turn {
        name: ref_name.trim_start_matches(TURN_REF_PREFIX).to_string(),
        ref_name: ref_name.to_string(),
        oid: oid.to_string(),
        parent,
        meta,
        files: rows.len(),
        added: rows.iter().filter_map(|r| r.added).sum(),
        removed: rows.iter().filter_map(|r| r.removed).sum(),
    })
}

pub fn list_turns(worktree: &Path) -> Result<Vec<Turn>> {
    turn_refs(worktree)?
        .iter()
        .map(|(r, o)| read_turn(worktree, r, o))
        .collect()
}

pub fn resolve_turn(worktree: &Path, name: &str) -> Result<Turn> {
    let refs = turn_refs(worktree)?;
    let found = if name == "latest" {
        refs.first()
    } else {
        refs.iter().find(|(r, o)| {
            r == name
                || r.trim_start_matches(TURN_REF_PREFIX) == name
                || (name.len() >= 7 && o.starts_with(name))
        })
    };
    match found {
        Some((r, o)) => read_turn(worktree, r, o),
        None if name == "latest" => bail!("no agent turns in this workspace yet"),
        None => bail!("no agent turn {name}"),
    }
}

pub fn is_worktree(path: &Path) -> bool {
    git_ok(path, &["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true")
}

#[cfg(test)]
mod tests;
