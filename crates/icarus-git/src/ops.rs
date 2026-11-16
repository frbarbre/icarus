//! Working-tree operations behind Source Control: discard, stage, commit, push, pull and sync.
//!
//! Errors carry git's own message so the user sees exactly what git said.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::{Area, ChangeStatus, local_state, status_entries};

/// A failed git operation; `message` is git's stderr (or ours, for refusals before git runs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpError {
    pub message: String,
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OpError {}

fn refuse(message: impl Into<String>) -> OpError {
    OpError {
        message: message.into(),
    }
}

/// Runs git without ever waiting on a terminal, an editor or a credential prompt.
fn run(worktree: &Path, args: &[&str]) -> Result<String, OpError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .env("GIT_EDITOR", "true")
        .output()
        .map_err(|error| refuse(format!("could not run git: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let message = if stderr.is_empty() { stdout } else { stderr };
        return Err(refuse(if message.is_empty() {
            format!("git {} failed", args.first().copied().unwrap_or_default())
        } else {
            message
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A worktree-relative path that can't climb out of the worktree.
pub fn checked_path(worktree: &Path, path: &str) -> Result<String, OpError> {
    let candidate = Path::new(path);
    let relative = if candidate.is_absolute() {
        // Either side may be spelled through a symlink (macOS /var vs /private/var).
        let roots = [
            Some(worktree.to_path_buf()),
            std::fs::canonicalize(worktree).ok(),
        ];
        let candidates = [
            Some(candidate.to_path_buf()),
            candidate
                .parent()
                .and_then(|parent| std::fs::canonicalize(parent).ok())
                .zip(candidate.file_name())
                .map(|(parent, name)| parent.join(name)),
        ];
        roots
            .iter()
            .flatten()
            .find_map(|root| {
                candidates
                    .iter()
                    .flatten()
                    .find_map(|c| c.strip_prefix(root).ok().map(Path::to_path_buf))
            })
            .ok_or_else(|| refuse(format!("{path} is outside the worktree")))?
    } else {
        candidate.to_path_buf()
    };
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(refuse(format!("{path} is outside the worktree")));
    }
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

/// What discarding a row does, which also names its button ("Discard changes", …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscardKind {
    /// An untracked or newly added file: it is deleted.
    DeleteUntracked,
    /// A file deleted on disk: it comes back from HEAD.
    Restore,
    /// Any other uncommitted edit: the file goes back to HEAD.
    Discard,
}

impl DiscardKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::DeleteUntracked => "Delete untracked file",
            Self::Restore => "Restore file",
            Self::Discard => "Discard changes",
        }
    }
}

/// The discard a path offers, or `None` when it has nothing discardable (committed only, conflicted).
pub fn discard_kind(worktree: &Path, path: &str) -> Result<Option<DiscardKind>, OpError> {
    let entries = status_entries(worktree).map_err(|e| refuse(format!("{e:#}")))?;
    let local: Vec<_> = entries.iter().filter(|e| e.path == path).collect();
    if local.is_empty() || local.iter().any(|e| e.conflicted) {
        return Ok(None);
    }
    if local.iter().any(|e| e.area == Some(Area::Untracked))
        || (!in_head(worktree, path) && !local.iter().any(|e| e.status == ChangeStatus::Deleted))
    {
        return Ok(Some(DiscardKind::DeleteUntracked));
    }
    if local
        .iter()
        .any(|e| e.status == ChangeStatus::Deleted && e.area == Some(Area::Unstaged))
        && !worktree.join(path).exists()
    {
        return Ok(Some(DiscardKind::Restore));
    }
    Ok(Some(DiscardKind::Discard))
}

fn in_head(worktree: &Path, path: &str) -> bool {
    run(worktree, &["cat-file", "-e", &format!("HEAD:{path}")]).is_ok()
}

/// Throws away only the uncommitted change of each path; what's committed on the branch stays.
pub fn discard(worktree: &Path, paths: &[String]) -> Result<Vec<(String, DiscardKind)>, OpError> {
    let checked: Vec<String> = paths
        .iter()
        .map(|p| checked_path(worktree, p))
        .collect::<Result<_, _>>()?;
    let entries = status_entries(worktree).map_err(|e| refuse(format!("{e:#}")))?;
    let mut done = Vec::new();
    for path in checked {
        let kind = discard_kind(worktree, &path)?
            .ok_or_else(|| refuse(format!("{path} has no uncommitted change to discard")))?;
        // A staged rename also leaves its old path to bring back.
        let renamed_from: BTreeSet<String> = entries
            .iter()
            .filter(|e| e.path == path)
            .filter_map(|e| e.old_path.clone())
            .collect();
        match kind {
            DiscardKind::DeleteUntracked => {
                if run(worktree, &["ls-files", "--error-unmatch", "--", &path]).is_ok() {
                    run(
                        worktree,
                        &["rm", "--cached", "--force", "--quiet", "--", &path],
                    )?;
                }
                let full = worktree.join(&path);
                if full.is_dir() {
                    std::fs::remove_dir_all(&full)
                } else {
                    std::fs::remove_file(&full)
                }
                .map_err(|error| refuse(format!("could not delete {path}: {error}")))?;
            }
            DiscardKind::Restore | DiscardKind::Discard => {
                run(
                    worktree,
                    &[
                        "restore",
                        "--source=HEAD",
                        "--staged",
                        "--worktree",
                        "--",
                        &path,
                    ],
                )?;
            }
        }
        for old in renamed_from {
            if in_head(worktree, &old) {
                run(
                    worktree,
                    &[
                        "restore",
                        "--source=HEAD",
                        "--staged",
                        "--worktree",
                        "--",
                        &old,
                    ],
                )?;
            }
        }
        done.push((path, kind));
    }
    Ok(done)
}

pub fn stage(worktree: &Path, paths: &[String]) -> Result<(), OpError> {
    let checked: Vec<String> = paths
        .iter()
        .map(|p| checked_path(worktree, p))
        .collect::<Result<_, _>>()?;
    let mut args = vec!["add", "--all", "--"];
    args.extend(checked.iter().map(String::as_str));
    run(worktree, &args).map(|_| ())
}

pub fn unstage(worktree: &Path, paths: &[String]) -> Result<(), OpError> {
    let checked: Vec<String> = paths
        .iter()
        .map(|p| checked_path(worktree, p))
        .collect::<Result<_, _>>()?;
    let has_head = run(worktree, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok();
    let mut args: Vec<&str> = if has_head {
        vec!["restore", "--staged", "--"]
    } else {
        vec!["rm", "--cached", "--quiet", "-r", "--"]
    };
    args.extend(checked.iter().map(String::as_str));
    run(worktree, &args).map(|_| ())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRequest {
    pub message: String,
    #[serde(default)]
    pub amend: bool,
    /// Stage everything first, even when something is already staged.
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitOutcome {
    pub oid: String,
    pub summary: String,
    /// True when nothing was staged, so every change was committed.
    pub committed_all: bool,
}

/// Commits what's staged; with nothing staged it commits every change, as the upstream commit box did.
pub fn commit(worktree: &Path, request: &CommitRequest) -> Result<CommitOutcome, OpError> {
    let message = request.message.trim();
    if message.is_empty() && !request.amend {
        return Err(refuse("Write a commit message first."));
    }
    let entries = status_entries(worktree).map_err(|e| refuse(format!("{e:#}")))?;
    if entries.iter().any(|e| e.conflicted) {
        return Err(refuse("Resolve the merge conflicts before committing."));
    }
    let staged = entries.iter().any(|e| e.area == Some(Area::Staged));
    let committed_all = request.all || !staged;
    if committed_all {
        if entries.is_empty() && !request.amend {
            return Err(refuse("Nothing to commit."));
        }
        run(worktree, &["add", "--all"])?;
    }
    let mut args = vec!["commit", "--quiet"];
    if request.amend {
        args.push("--amend");
    }
    if message.is_empty() {
        args.push("--no-edit");
    } else {
        args.extend(["-m", message]);
    }
    run(worktree, &args)?;
    let oid = run(worktree, &["rev-parse", "HEAD"])?.trim().to_string();
    let summary = run(worktree, &["log", "-1", "--format=%s"])?
        .trim()
        .to_string();
    Ok(CommitOutcome {
        oid,
        summary,
        committed_all,
    })
}

/// Where a branch pushes to and how far it is from there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncState {
    pub branch: Option<String>,
    pub upstream: Option<String>,
    /// The upstream is a same-named branch, i.e. this branch has been pushed. A workspace branch
    /// starts out tracking its base (e.g. `origin/main`), which doesn't count.
    pub published: bool,
    pub ahead: u32,
    pub behind: u32,
    pub has_uncommitted_changes: bool,
    pub has_conflicts: bool,
}

pub fn sync_state(worktree: &Path) -> Result<SyncState, OpError> {
    let state = local_state(worktree).map_err(|e| refuse(format!("{e:#}")))?;
    let published = matches!(
        (&state.branch, &state.upstream),
        (Some(branch), Some(upstream)) if upstream.split_once('/').is_some_and(|(_, name)| name == branch)
    );
    Ok(SyncState {
        branch: state.branch,
        upstream: state.upstream,
        published,
        ahead: state.unpushed_commits,
        behind: state.behind_upstream,
        has_uncommitted_changes: state.has_uncommitted_changes,
        has_conflicts: state.has_local_conflicts,
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushRequest {
    #[serde(default)]
    pub force_with_lease: bool,
    /// Publish a branch that has no upstream yet; also done automatically when there is none.
    #[serde(default)]
    pub set_upstream: bool,
}

fn push_remote(worktree: &Path) -> String {
    let branch = run(worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|b| b.trim().to_string())
        .unwrap_or_default();
    run(worktree, &["config", &format!("branch.{branch}.remote")])
        .map(|r| r.trim().to_string())
        .ok()
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| "origin".to_string())
}

pub fn push(worktree: &Path, request: PushRequest) -> Result<SyncState, OpError> {
    let state = sync_state(worktree)?;
    if state.branch.is_none() {
        return Err(refuse("HEAD is detached; check out a branch to push."));
    }
    let mut args = vec!["push".to_string()];
    if request.force_with_lease {
        args.push("--force-with-lease".to_string());
    }
    if request.set_upstream || !state.published {
        args.extend([
            "--set-upstream".to_string(),
            push_remote(worktree),
            "HEAD".to_string(),
        ]);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run(worktree, &args)?;
    sync_state(worktree)
}

pub fn pull(worktree: &Path) -> Result<SyncState, OpError> {
    let state = sync_state(worktree)?;
    if !state.published {
        return Err(refuse(
            "This branch has no upstream to pull from; push it first.",
        ));
    }
    // Without pull.rebase configured, git refuses to reconcile diverged branches; merge then.
    let configured = run(worktree, &["config", "--get", "pull.rebase"]).is_ok();
    let args: &[&str] = if configured {
        &["pull", "--no-edit"]
    } else {
        &["pull", "--no-edit", "--no-rebase"]
    };
    run(worktree, args)?;
    sync_state(worktree)
}

/// Pull, then push what's left to push.
pub fn sync(worktree: &Path) -> Result<SyncState, OpError> {
    let state = sync_state(worktree)?;
    if state.published {
        pull(worktree)?;
    }
    push(worktree, PushRequest::default())
}

/// Absolute path of a checked worktree-relative path, for callers that touch files themselves.
pub fn resolve(worktree: &Path, path: &str) -> Result<PathBuf, OpError> {
    Ok(worktree.join(checked_path(worktree, path)?))
}
