//! The base a review diff compares against: the reviewed commit, or that commit replayed onto the branch's current base.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{git, git_ok};

/// Refs under this prefix are exact bases: the Changes list diffs `base..HEAD`, not from a merge base.
pub const REVIEW_BASE_PREFIX: &str = "refs/icarus/review-base/";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewBase {
    /// The reviewed commit is still on the branch, over the same base.
    Reviewed { base: String },
    /// The branch moved onto a newer base; `base_ref` is the reviewed version replayed onto it.
    Interdiff { base_ref: String, conflicted: bool },
    /// The commit is not in the repository, or shares no history with the target.
    Missing,
}

impl ReviewBase {
    pub fn base_ref(&self) -> Option<&str> {
        match self {
            Self::Reviewed { base } => Some(base),
            Self::Interdiff { base_ref, .. } => Some(base_ref),
            Self::Missing => None,
        }
    }
}

pub fn is_review_base_ref(name: &str) -> bool {
    name.starts_with(REVIEW_BASE_PREFIX)
}

pub fn commit_exists(worktree: &Path, commit: &str) -> bool {
    git_ok(
        worktree,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{commit}^{{commit}}"),
        ],
    )
    .is_some()
}

fn worktree_key(worktree: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in worktree.to_string_lossy().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn merge_tree(
    worktree: &Path,
    merge_base: &str,
    ours: &str,
    theirs: &str,
) -> Result<(String, bool)> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args([
            "merge-tree",
            "--write-tree",
            "--merge-base",
            merge_base,
            ours,
            theirs,
        ])
        .output()
        .context("run git merge-tree")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let tree = stdout.lines().next().unwrap_or_default().trim().to_owned();
    // Why exit 1 is fine: on conflict merge-tree still writes a tree, with conflict markers in the files.
    match output.status.code() {
        Some(0) if !tree.is_empty() => Ok((tree, false)),
        Some(1) if !tree.is_empty() => Ok((tree, true)),
        _ => bail!(
            "git merge-tree failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// Resolves the base for "what changed since `reviewed`" against `target_ref` (e.g. `refs/remotes/origin/main`).
pub fn resolve_review_base(
    worktree: &Path,
    reviewed: &str,
    target_ref: &str,
) -> Result<ReviewBase> {
    let Some(reviewed) = git_ok(
        worktree,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{reviewed}^{{commit}}"),
        ],
    ) else {
        return Ok(ReviewBase::Missing);
    };
    let (Some(reviewed_base), Some(current_base)) = (
        git_ok(worktree, &["merge-base", &reviewed, target_ref]),
        git_ok(worktree, &["merge-base", "HEAD", target_ref]),
    ) else {
        return Ok(ReviewBase::Missing);
    };
    let on_branch = git_ok(
        worktree,
        &["merge-base", "--is-ancestor", &reviewed, "HEAD"],
    )
    .is_some();
    if reviewed_base == current_base && on_branch {
        return Ok(ReviewBase::Reviewed { base: reviewed });
    }
    let (tree, conflicted) = merge_tree(worktree, &reviewed_base, &current_base, &reviewed)?;
    let message = format!(
        "Reviewed {} replayed onto {}",
        &reviewed[..12.min(reviewed.len())],
        &current_base[..12.min(current_base.len())]
    );
    let commit = git(
        worktree,
        &[
            "-c",
            "user.name=Icarus",
            "-c",
            "user.email=icarus@localhost",
            "commit-tree",
            &tree,
            "-p",
            &current_base,
            "-m",
            &message,
        ],
    )?
    .trim()
    .to_owned();
    let prefix = format!("{REVIEW_BASE_PREFIX}{}-", worktree_key(worktree));
    // Why a new name per rebuild: anything caching by ref name must not serve the old replay.
    let name = format!("{prefix}{}", &commit[..12.min(commit.len())]);
    git(worktree, &["update-ref", &name, &commit])?;
    let stale = git(
        worktree,
        &[
            "for-each-ref",
            "--format=%(refname)",
            REVIEW_BASE_PREFIX.trim_end_matches('/'),
        ],
    )
    .unwrap_or_default();
    for old in stale
        .lines()
        .filter(|r| r.starts_with(&prefix) && *r != name)
    {
        let _ = git(worktree, &["update-ref", "-d", old]);
    }
    Ok(ReviewBase::Interdiff {
        base_ref: name,
        conflicted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Repo {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl Repo {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("repo");
            std::fs::create_dir_all(&path).expect("mkdir");
            let repo = Self { _dir: dir, path };
            repo.git(&["init", "-q", "-b", "main"]);
            repo.git(&["config", "user.email", "me@example.com"]);
            repo.git(&["config", "user.name", "Me"]);
            repo.git(&["config", "commit.gpgsign", "false"]);
            repo.commit("a.txt", "one\n", "init");
            repo
        }

        fn git(&self, args: &[&str]) -> String {
            git(&self.path, args).expect("git").trim().to_owned()
        }

        fn commit(&self, file: &str, text: &str, message: &str) -> String {
            std::fs::write(self.path.join(file), text).expect("write");
            self.git(&["add", "."]);
            self.git(&["commit", "-q", "-m", message]);
            self.git(&["rev-parse", "HEAD"])
        }

        fn diff_names(&self, base: &str) -> Vec<String> {
            self.git(&["diff", "--name-only", base, "HEAD"])
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    #[test]
    fn keeps_the_reviewed_commit_when_the_branch_only_grew() {
        let repo = Repo::new();
        repo.git(&["checkout", "-q", "-b", "feature"]);
        let reviewed = repo.commit("b.txt", "b\n", "first");
        repo.commit("c.txt", "c\n", "second");
        let base = resolve_review_base(&repo.path, &reviewed, "main").expect("resolve");
        assert_eq!(base, ReviewBase::Reviewed { base: reviewed });
    }

    #[test]
    fn a_rebase_is_replayed_so_only_new_work_shows() {
        let repo = Repo::new();
        repo.git(&["checkout", "-q", "-b", "feature"]);
        let reviewed = repo.commit("b.txt", "b\n", "first");
        repo.git(&["checkout", "-q", "main"]);
        repo.commit("upstream.txt", "u\n", "upstream");
        repo.git(&["checkout", "-q", "feature"]);
        repo.git(&["rebase", "-q", "main"]);
        repo.commit("c.txt", "c\n", "after review");
        let base = resolve_review_base(&repo.path, &reviewed, "main").expect("resolve");
        let ReviewBase::Interdiff {
            base_ref,
            conflicted,
        } = base
        else {
            panic!("expected an interdiff, got {base:?}");
        };
        assert!(!conflicted);
        assert!(is_review_base_ref(&base_ref));
        assert_eq!(repo.diff_names(&base_ref), vec!["c.txt".to_owned()]);
        let changes = crate::changes(
            &repo.path,
            &crate::ChangesQuery {
                base: Some(base_ref),
                uncommitted_only: false,
            },
        )
        .expect("changes");
        let paths: Vec<&str> = changes.rows.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["c.txt"], "an exact base, not a merge base");
    }

    #[test]
    fn a_raw_commit_works_as_a_changes_base() {
        let repo = Repo::new();
        repo.git(&["checkout", "-q", "-b", "feature"]);
        let reviewed = repo.commit("b.txt", "b\n", "first");
        repo.commit("c.txt", "c\n", "second");
        let changes = crate::changes(
            &repo.path,
            &crate::ChangesQuery {
                base: Some(reviewed),
                uncommitted_only: false,
            },
        )
        .expect("changes");
        let paths: Vec<&str> = changes.rows.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["c.txt"]);
    }

    #[test]
    fn a_merge_of_the_target_is_left_out() {
        let repo = Repo::new();
        repo.git(&["checkout", "-q", "-b", "feature"]);
        let reviewed = repo.commit("b.txt", "b\n", "first");
        repo.git(&["checkout", "-q", "main"]);
        repo.commit("upstream.txt", "u\n", "upstream");
        repo.git(&["checkout", "-q", "feature"]);
        repo.git(&["merge", "-q", "--no-edit", "main"]);
        let base = resolve_review_base(&repo.path, &reviewed, "main").expect("resolve");
        let base_ref = base.base_ref().expect("a base").to_owned();
        assert!(repo.diff_names(&base_ref).is_empty());
    }

    #[test]
    fn a_rebuilt_base_replaces_the_previous_ref() {
        let repo = Repo::new();
        repo.git(&["checkout", "-q", "-b", "feature"]);
        let reviewed = repo.commit("b.txt", "b\n", "first");
        repo.git(&["checkout", "-q", "main"]);
        repo.commit("upstream.txt", "u\n", "upstream");
        repo.git(&["checkout", "-q", "feature"]);
        repo.git(&["rebase", "-q", "main"]);
        let first = resolve_review_base(&repo.path, &reviewed, "main").expect("first");
        repo.git(&["checkout", "-q", "main"]);
        repo.commit("upstream2.txt", "u2\n", "upstream again");
        repo.git(&["checkout", "-q", "feature"]);
        repo.git(&["rebase", "-q", "main"]);
        let second = resolve_review_base(&repo.path, &reviewed, "main").expect("second");
        assert_ne!(first.base_ref(), second.base_ref());
        let refs = repo.git(&[
            "for-each-ref",
            "--format=%(refname)",
            "refs/icarus/review-base",
        ]);
        assert_eq!(refs.lines().count(), 1, "{refs}");
        assert_eq!(refs.trim(), second.base_ref().expect("ref"));
    }

    #[test]
    fn a_conflicting_replay_is_flagged() {
        let repo = Repo::new();
        repo.git(&["checkout", "-q", "-b", "feature"]);
        let reviewed = repo.commit("a.txt", "feature\n", "first");
        repo.git(&["checkout", "-q", "main"]);
        repo.commit("a.txt", "upstream\n", "upstream");
        repo.git(&["checkout", "-q", "feature"]);
        repo.git(&["reset", "-q", "--hard", "main"]);
        repo.commit("a.txt", "resolved\n", "redo on main");
        let base = resolve_review_base(&repo.path, &reviewed, "main").expect("resolve");
        assert!(matches!(
            base,
            ReviewBase::Interdiff {
                conflicted: true,
                ..
            }
        ));
    }

    #[test]
    fn a_missing_commit_is_reported() {
        let repo = Repo::new();
        let base = resolve_review_base(&repo.path, &"1".repeat(40), "main").expect("resolve");
        assert_eq!(base, ReviewBase::Missing);
        assert!(!commit_exists(&repo.path, &"1".repeat(40)));
        assert!(commit_exists(&repo.path, "HEAD"));
    }
}
