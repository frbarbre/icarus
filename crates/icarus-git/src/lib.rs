//! Git operations for Icarus: repos, worktrees and the single Changes list.
//!
//! Everything shells out to `git`, which is what users and agents run too, so behaviour matches.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub mod filters;
pub mod ops;
pub mod review_base;
pub mod turns;

pub fn git(repo: &Path, args: &[&str]) -> Result<String> {
    git_env(repo, args, &[])
}

/// A git run slower than this is logged with its arguments and duration.
const SLOW_GIT: std::time::Duration = std::time::Duration::from_millis(300);

fn log_slow(repo: &Path, args: &[&str], started: std::time::Instant) {
    let took = started.elapsed();
    if took > SLOW_GIT {
        tracing::info!(
            "git {} in {} took {took:.1?}",
            args.join(" "),
            repo.display()
        );
    }
}

pub fn git_env(repo: &Path, args: &[&str], env: &[(&str, &std::ffi::OsStr)]) -> Result<String> {
    let started = std::time::Instant::now();
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    log_slow(repo, args, started);
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Like [`git`], but kills git once `timeout` passes (a folder macOS hasn't let us into blocks it forever).
pub fn git_timeout(repo: &Path, args: &[&str], timeout: std::time::Duration) -> Result<String> {
    use std::io::Read;
    use std::os::unix::process::CommandExt as _;
    use std::process::Stdio;

    let started = std::time::Instant::now();
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "core.quotePath=false"])
        .args(args)
        // Its own process group, so a timeout also ends whatever git started.
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            let _ = sender.send(bytes);
        });
        receiver
    };
    let kill_group = |pid: u32| {
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .status();
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = std::time::Instant::now() + timeout;
    let timed_out = |child: &mut std::process::Child| {
        kill_group(child.id());
        let _ = child.kill();
        let _ = child.wait();
        anyhow::anyhow!(
            "git {} in {} did not finish within {:.1?} (is the folder blocked by macOS privacy settings?)",
            args.join(" "),
            repo.display(),
            timeout
        )
    };
    // Git closes stdout as it exits, so waiting on the pipe answers without polling.
    let stdout =
        match stdout.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(bytes) => bytes,
            Err(_) => return Err(timed_out(&mut child)),
        };
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            return Err(timed_out(&mut child));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    log_slow(repo, args, started);
    let stderr = stderr
        .recv_timeout(
            deadline
                .saturating_duration_since(std::time::Instant::now())
                .max(std::time::Duration::from_millis(200)),
        )
        .unwrap_or_default();
    if !status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// How long a read-only git run may take before it is killed; `ICARUS_GIT_TIMEOUT_MS` overrides it.
pub fn read_timeout() -> std::time::Duration {
    static TIMEOUT: std::sync::LazyLock<std::time::Duration> = std::sync::LazyLock::new(|| {
        std::env::var("ICARUS_GIT_TIMEOUT_MS")
            .ok()
            .and_then(|ms| ms.parse().ok())
            .map(std::time::Duration::from_millis)
            .unwrap_or(std::time::Duration::from_secs(20))
    });
    *TIMEOUT
}

/// A read-only git run under [`read_timeout`].
pub fn git_read(repo: &Path, args: &[&str]) -> Result<String> {
    git_timeout(repo, args, read_timeout())
}

fn git_ok(repo: &Path, args: &[&str]) -> Option<String> {
    git_read(repo, args).ok().map(|s| s.trim().to_string())
}

/// `https://github.com/acme/widgets.git` and `git@github.com:acme/widgets` both become `github.com/acme/widgets`.
pub fn canonical_remote(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let rest = if let Some(rest) = url.split_once("://").map(|(_, r)| r) {
        rest.rsplit_once('@')
            .map(|(_, r)| r)
            .unwrap_or(rest)
            .to_string()
    } else if let Some((user_host, path)) = url.split_once(':') {
        let host = user_host
            .rsplit_once('@')
            .map(|(_, h)| h)
            .unwrap_or(user_host);
        format!("{host}/{path}")
    } else {
        return None;
    };
    let rest = rest.split_once(':').map_or(rest.clone(), |(host, tail)| {
        // Drop an explicit port (`host:22/owner/repo`) but keep scp-style paths handled above.
        match tail.split_once('/') {
            Some((port, path)) if port.chars().all(|c| c.is_ascii_digit()) => {
                format!("{host}/{path}")
            }
            _ => rest.clone(),
        }
    });
    Some(rest.to_ascii_lowercase())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RepoInfo {
    /// The main checkout that owns all linked worktrees.
    pub root: PathBuf,
    pub remote: Option<String>,
    pub default_base: Option<String>,
}

pub fn repo_info(path: &Path) -> Result<RepoInfo> {
    let common = git_read(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = PathBuf::from(common.trim());
    let root = if common.file_name().is_some_and(|n| n == ".git") {
        common
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(common.clone())
    } else {
        PathBuf::from(git_read(path, &["rev-parse", "--show-toplevel"])?.trim())
    };
    let remote = git_ok(&root, &["remote", "get-url", "origin"]).and_then(|u| canonical_remote(&u));
    let default_base = default_base_ref(&root);
    Ok(RepoInfo {
        root,
        remote,
        default_base,
    })
}

/// `origin/HEAD`'s target, else the first existing of origin/main, origin/master, main, master.
pub fn default_base_ref(repo: &Path) -> Option<String> {
    if let Some(target) = git_ok(
        repo,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    ) && let Some(name) = target.strip_prefix("refs/remotes/")
    {
        return Some(name.to_string());
    }
    let candidates = [
        ("refs/remotes/origin/main", "origin/main"),
        ("refs/remotes/origin/master", "origin/master"),
        ("refs/heads/main", "main"),
        ("refs/heads/master", "master"),
    ];
    let existing = git_ok(repo, &["for-each-ref", "--format=%(refname)"])?;
    candidates
        .iter()
        .find(|(full, _)| existing.lines().any(|l| l == *full))
        .map(|(_, short)| short.to_string())
}

/// Resolves a base name the way Orca does: names with `/` prefer remotes, bare names are branches.
pub fn qualify_ref(repo: &Path, name: &str) -> Option<String> {
    let candidates: Vec<String> = if name.len() == 40 && name.bytes().all(|b| b.is_ascii_hexdigit())
    {
        vec![name.to_ascii_lowercase()]
    } else if name.starts_with("refs/") {
        vec![name.to_string()]
    } else if name.contains('/') {
        vec![format!("refs/remotes/{name}"), format!("refs/heads/{name}")]
    } else {
        vec![format!("refs/heads/{name}")]
    };
    candidates.into_iter().find(|r| {
        git_ok(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{r}^{{commit}}"),
            ],
        )
        .is_some()
    })
}

/// What the Change Base Ref picker offers for a repository.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BaseRefs {
    /// What a workspace without a base of its own compares against.
    pub default_base: Option<String>,
    pub remote_count: usize,
    /// Short names (`origin/main`, `main`), most recently committed first.
    pub refs: Vec<String>,
}

/// Branches matching `query` the way Orca's base ref search matches them: one word matches
/// inside any segment of the name; `a/b` matches segment by segment, or as a branch prefix.
pub fn search_base_refs(repo: &Path, query: &str, limit: usize) -> Result<BaseRefs> {
    let remotes: Vec<String> = git_ok(repo, &["remote"])
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|remote| !remote.is_empty())
        .map(str::to_string)
        .collect();
    let listing = git_read(
        repo,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)%00%(refname:short)",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    let query: String = query
        .trim()
        .chars()
        .filter(|c| !matches!(c, '*' | '?' | '[' | ']' | '\\'))
        .collect();
    let tokens: Vec<&str> = query.split('/').filter(|token| !token.is_empty()).collect();
    let prefix = tokens.join("/");
    let mut segmented = Vec::new();
    let mut rooted = Vec::new();
    for line in listing.lines() {
        let Some((full, short)) = line.split_once('\0') else {
            continue;
        };
        let (path, branch) = if let Some(path) = full.strip_prefix("refs/heads/") {
            (path, path)
        } else if let Some(path) = full.strip_prefix("refs/remotes/") {
            let configured = remotes
                .iter()
                .filter(|remote| path.starts_with(&format!("{remote}/")))
                .max_by_key(|remote| remote.len());
            let branch = match configured {
                Some(remote) => &path[remote.len() + 1..],
                None => path.split_once('/').map_or(path, |(_, branch)| branch),
            };
            // A remote's HEAD only points at one of its branches.
            if branch == "HEAD" && (configured.is_some() || path.matches('/').count() == 1) {
                continue;
            }
            (path, branch)
        } else {
            continue;
        };
        let segments: Vec<&str> = path.split('/').collect();
        if tokens.len() <= 1 {
            let token = tokens.first().copied().unwrap_or("");
            if segments.iter().any(|segment| segment.contains(token)) {
                segmented.push(short.to_string());
            }
            continue;
        }
        if segments.len() == tokens.len()
            && segments
                .iter()
                .zip(&tokens)
                .all(|(segment, token)| segment.contains(token))
        {
            segmented.push(short.to_string());
        }
        if branch.starts_with(&prefix) {
            rooted.push(short.to_string());
        }
    }
    let mut refs: Vec<String> = Vec::new();
    for index in 0..segmented.len().max(rooted.len()) {
        for name in [segmented.get(index), rooted.get(index)]
            .into_iter()
            .flatten()
        {
            if refs.len() < limit && !refs.contains(name) {
                refs.push(name.clone());
            }
        }
    }
    Ok(BaseRefs {
        default_base: default_base_ref(repo),
        remote_count: remotes.len(),
        refs,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub main: bool,
    #[serde(default)]
    pub bare: bool,
    /// The lock reason (empty when locked without one).
    #[serde(default)]
    pub locked: Option<String>,
    /// Git would prune it: its folder is gone.
    #[serde(default)]
    pub prunable: bool,
}

pub fn list_worktrees(repo: &Path) -> Result<Vec<Worktree>> {
    Ok(parse_worktree_list(&git_read(
        repo,
        &["worktree", "list", "--porcelain"],
    )?))
}

pub fn list_worktrees_timeout(repo: &Path, timeout: std::time::Duration) -> Result<Vec<Worktree>> {
    Ok(parse_worktree_list(&git_timeout(
        repo,
        &["worktree", "list", "--porcelain"],
        timeout,
    )?))
}

fn parse_worktree_list(output: &str) -> Vec<Worktree> {
    let mut worktrees = Vec::new();
    let mut current: Option<Worktree> = None;
    for line in output.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let Some(worktree) = current.take() {
                worktrees.push(worktree);
            }
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "worktree" => {
                current = Some(Worktree {
                    path: PathBuf::from(value),
                    head: None,
                    branch: None,
                    detached: false,
                    main: worktrees.is_empty(),
                    bare: false,
                    locked: None,
                    prunable: false,
                });
            }
            "bare" => {
                if let Some(w) = current.as_mut() {
                    w.bare = true;
                }
            }
            "locked" => {
                if let Some(w) = current.as_mut() {
                    w.locked = Some(value.to_string());
                }
            }
            "prunable" => {
                if let Some(w) = current.as_mut() {
                    w.prunable = true;
                }
            }
            "HEAD" => {
                if let Some(w) = current.as_mut() {
                    w.head = Some(value.to_string());
                }
            }
            "branch" => {
                if let Some(w) = current.as_mut() {
                    w.branch = Some(value.trim_start_matches("refs/heads/").to_string());
                }
            }
            "detached" => {
                if let Some(w) = current.as_mut() {
                    w.detached = true;
                }
            }
            _ => {}
        }
    }
    worktrees
}

#[derive(Debug, Clone)]
pub struct CreateWorktree<'a> {
    pub repo: &'a Path,
    pub path: &'a Path,
    pub branch: &'a str,
    /// Start point, e.g. `origin/main`. Ignored when the branch already exists.
    pub base: &'a str,
    pub fetch: bool,
}

pub fn create_worktree(request: &CreateWorktree<'_>) -> Result<Worktree> {
    if request.fetch
        && let Some((remote, branch)) = request.base.split_once('/')
    {
        let _ = git(request.repo, &["fetch", "--quiet", remote, branch]);
    }
    if let Some(parent) = request.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let path = request.path.to_string_lossy();
    let branch_exists = git_ok(
        request.repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{}", request.branch),
        ],
    )
    .is_some();
    if branch_exists {
        git(request.repo, &["worktree", "add", &path, request.branch])?;
    } else {
        git(
            request.repo,
            &["worktree", "add", "-b", request.branch, &path, request.base],
        )?;
    }
    list_worktrees(request.repo)?
        .into_iter()
        .find(|w| same_path(&w.path, request.path))
        .context("created worktree not listed")
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

#[derive(Debug, thiserror::Error)]
pub enum RemoveError {
    #[error("worktree has uncommitted changes; pass force to delete anyway")]
    Dirty,
    #[error(transparent)]
    Git(#[from] anyhow::Error),
}

pub fn remove_worktree(
    repo: &Path,
    path: &Path,
    force: bool,
    delete_branch: bool,
) -> Result<(), RemoveError> {
    if !force && is_dirty(path)? {
        return Err(RemoveError::Dirty);
    }
    let branch = list_worktrees(repo)?
        .into_iter()
        .find(|w| same_path(&w.path, path))
        .and_then(|w| w.branch);
    let path_str = path.to_string_lossy();
    if force {
        git(repo, &["worktree", "remove", "--force", &path_str])?;
    } else {
        git(repo, &["worktree", "remove", &path_str])?;
    }
    if delete_branch && let Some(branch) = branch {
        let flag = if force { "-D" } else { "-d" };
        let _ = git(repo, &["branch", flag, &branch]);
    }
    Ok(())
}

pub fn is_dirty(path: &Path) -> Result<bool> {
    Ok(
        !git_read(path, &["status", "--porcelain", "--untracked-files=normal"])?
            .trim()
            .is_empty(),
    )
}

/// What the next-action button needs to know about a worktree's local state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LocalState {
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    pub has_uncommitted_changes: bool,
    pub upstream: Option<String>,
    pub unpushed_commits: u32,
    pub behind_upstream: u32,
    /// A merge, rebase or cherry-pick stopped with conflicts.
    pub has_local_conflicts: bool,
    pub is_default_branch: bool,
}

pub fn local_state(worktree: &Path) -> Result<LocalState> {
    let branch = git_ok(worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
    let head_oid = git_ok(
        worktree,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
    );
    let status = shared_status(worktree, "normal")?;
    let records: Vec<&str> = status.split('\0').filter(|r| !r.is_empty()).collect();
    let has_local_conflicts = records.iter().any(|r| r.starts_with("u "));
    let has_uncommitted_changes = !records.is_empty();
    let upstream = git_ok(
        worktree,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    );
    let (behind_upstream, unpushed_commits) = match upstream {
        Some(_) => {
            let counts = git_ok(
                worktree,
                &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
            )
            .unwrap_or_default();
            let mut parts = counts
                .split_whitespace()
                .map(|v| v.parse::<u32>().unwrap_or(0));
            (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
        }
        None => (0, 0),
    };
    let default_branch =
        default_base_ref(worktree).map(|base| base.rsplit('/').next().unwrap_or(&base).to_string());
    // As Orca: `main` and `master` count as the default even when the remote's HEAD is unknown.
    let is_default_branch = matches!((&branch, &default_branch), (Some(b), Some(d)) if b == d)
        || matches!(branch.as_deref(), Some("main" | "master"));
    Ok(LocalState {
        branch,
        head_oid,
        has_uncommitted_changes,
        upstream,
        unpushed_commits,
        behind_upstream,
        has_local_conflicts,
        is_default_branch,
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
}

impl ChangeStatus {
    fn from_letter(letter: char) -> Self {
        match letter {
            'A' => Self::Added,
            'D' => Self::Deleted,
            'R' => Self::Renamed,
            'C' => Self::Copied,
            _ => Self::Modified,
        }
    }

    pub fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
            Self::Renamed => 'R',
            Self::Copied => 'C',
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Area {
    Staged,
    Unstaged,
    Untracked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub path: String,
    pub status: ChangeStatus,
    pub old_path: Option<String>,
    pub added: Option<u64>,
    pub removed: Option<u64>,
    pub area: Option<Area>,
    pub conflicted: bool,
}

/// One row of the Changes list: everything that differs from the merge base, committed or not.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChangeRow {
    pub path: String,
    pub status: ChangeStatus,
    pub old_path: Option<String>,
    pub added: Option<u64>,
    pub removed: Option<u64>,
    pub committed: bool,
    pub staged: bool,
    pub unstaged: bool,
    pub untracked: bool,
    pub conflicted: bool,
    #[serde(default)]
    pub binary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum CompareStatus {
    Ready {
        base: String,
        merge_base: String,
        ahead: u64,
        behind: u64,
    },
    NoBase,
    InvalidBase {
        base: String,
    },
    NoMergeBase {
        base: String,
    },
    UnbornHead,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Changes {
    pub compare: CompareStatus,
    pub rows: Vec<ChangeRow>,
    pub added: u64,
    pub removed: u64,
}

fn parse_raw_numstat(output: &str) -> Vec<Entry> {
    // `git diff --raw --numstat -z`: raw records first, then numstat records, NUL separated.
    let mut fields = output.split('\0').peekable();
    let mut entries: Vec<Entry> = Vec::new();
    while let Some(field) = fields.peek() {
        if !field.starts_with(':') {
            break;
        }
        let header = fields.next().unwrap_or_default();
        let letter = header
            .split_whitespace()
            .last()
            .and_then(|s| s.chars().next())
            .unwrap_or('M');
        let status = ChangeStatus::from_letter(letter);
        let first = fields.next().unwrap_or_default().to_string();
        let (path, old_path) = if matches!(status, ChangeStatus::Renamed | ChangeStatus::Copied) {
            (fields.next().unwrap_or_default().to_string(), Some(first))
        } else {
            (first, None)
        };
        entries.push(Entry {
            path,
            status,
            old_path,
            added: None,
            removed: None,
            area: None,
            conflicted: false,
        });
    }
    let mut counts: BTreeMap<String, (Option<u64>, Option<u64>)> = BTreeMap::new();
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        let mut parts = field.splitn(3, '\t');
        let added = parts.next().and_then(|v| v.parse().ok());
        let removed = parts.next().and_then(|v| v.parse().ok());
        let path = match parts.next() {
            Some("") | None => {
                // Renames print an empty path, then old and new paths as separate fields.
                let _old = fields.next();
                fields.next().unwrap_or_default().to_string()
            }
            Some(path) => path.to_string(),
        };
        counts.insert(path, (added, removed));
    }
    for entry in &mut entries {
        if let Some((added, removed)) = counts.get(&entry.path) {
            entry.added = *added;
            entry.removed = *removed;
        }
    }
    entries
}

type LineCounts = BTreeMap<String, (u64, u64)>;

/// Per-path line counts from the merge base to the working tree, including untracked files.
fn line_counts(
    worktree: &Path,
    merge_base: &str,
    untracked: &[String],
) -> Result<(LineCounts, std::collections::BTreeSet<String>)> {
    let output = git_read(
        worktree,
        &[
            "-c",
            "diff.autoRefreshIndex=false",
            "diff",
            "-z",
            "--numstat",
            "-M",
            merge_base,
            "--",
        ],
    )?;
    let mut counts = BTreeMap::new();
    let mut binary = std::collections::BTreeSet::new();
    let mut fields = output.split('\0');
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        let mut parts = field.splitn(3, '\t');
        let added = parts.next().and_then(|v| v.parse().ok());
        let removed = parts.next().and_then(|v| v.parse().ok());
        let path = match parts.next() {
            Some("") | None => {
                let _old = fields.next();
                fields.next().unwrap_or_default().to_string()
            }
            Some(path) => path.to_string(),
        };
        if let (Some(a), Some(r)) = (added, removed) {
            counts.insert(path, (a, r));
        } else {
            binary.insert(path);
        }
    }
    for path in untracked {
        if let Ok(content) = std::fs::read(worktree.join(path)) {
            if content.contains(&0) {
                binary.insert(path.clone());
                continue;
            }
            let lines = content
                .split(|b| *b == b'\n')
                .filter(|l| !l.is_empty())
                .count() as u64;
            counts.insert(path.clone(), (lines, 0));
        }
    }
    Ok((counts, binary))
}

type StatusSlot = std::sync::Arc<std::sync::Mutex<Option<(std::time::Instant, String)>>>;

static STATUS_RUNS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<(PathBuf, &'static str), StatusSlot>>,
> = std::sync::LazyLock::new(Default::default);

/// `git status --porcelain=v2 -z` shared by concurrent callers: one waiting behind a run reuses the next
/// run that started after it asked, so the answer is never older than the request. Many clients poll
/// the same worktrees, and in a large repo each run costs hundreds of milliseconds.
pub fn shared_status(worktree: &Path, untracked: &'static str) -> Result<String> {
    let asked = std::time::Instant::now();
    let slot = STATUS_RUNS
        .lock()
        .map_err(|_| anyhow::anyhow!("git status cache poisoned"))?
        .entry((worktree.to_path_buf(), untracked))
        .or_default()
        .clone();
    let mut last = slot
        .lock()
        .map_err(|_| anyhow::anyhow!("git status slot poisoned"))?;
    if let Some((started, output)) = last.as_ref()
        && *started >= asked
    {
        return Ok(output.clone());
    }
    let started = std::time::Instant::now();
    let flag = format!("--untracked-files={untracked}");
    // A background read must never hold index.lock while a commit or stage runs.
    let output = git_read(
        worktree,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "-z",
            &flag,
        ],
    )?;
    *last = Some((started, output.clone()));
    Ok(output)
}

pub fn status_entries(worktree: &Path) -> Result<Vec<Entry>> {
    let output = shared_status(worktree, "all")?;
    let mut entries = Vec::new();
    let mut fields = output.split('\0');
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        let kind = record.chars().next().unwrap_or(' ');
        match kind {
            '?' => entries.push(Entry {
                path: record[2..].to_string(),
                status: ChangeStatus::Added,
                old_path: None,
                added: None,
                removed: None,
                area: Some(Area::Untracked),
                conflicted: false,
            }),
            '1' | '2' => {
                let parts: Vec<&str> = record
                    .splitn(if kind == '1' { 9 } else { 10 }, ' ')
                    .collect();
                let xy: Vec<char> = parts.get(1).copied().unwrap_or("..").chars().collect();
                let path = parts.last().copied().unwrap_or_default().to_string();
                let old_path = if kind == '2' {
                    fields.next().map(str::to_string)
                } else {
                    None
                };
                for (index, area) in [(0, Area::Staged), (1, Area::Unstaged)] {
                    let letter = xy.get(index).copied().unwrap_or('.');
                    if letter != '.' {
                        entries.push(Entry {
                            path: path.clone(),
                            status: ChangeStatus::from_letter(letter),
                            old_path: old_path.clone(),
                            added: None,
                            removed: None,
                            area: Some(area),
                            conflicted: false,
                        });
                    }
                }
            }
            'u' => {
                let path = record
                    .splitn(11, ' ')
                    .last()
                    .unwrap_or_default()
                    .to_string();
                entries.push(Entry {
                    path,
                    status: ChangeStatus::Modified,
                    old_path: None,
                    added: None,
                    removed: None,
                    area: Some(Area::Unstaged),
                    conflicted: true,
                });
            }
            _ => {}
        }
    }
    Ok(entries)
}

/// Merges branch and working-tree entries into one row per file (Orca's `buildAllChangeEntries`).
pub fn merge_rows(
    branch: &[Entry],
    local: &[Entry],
    uncommitted_only: bool,
    line_counts: Option<&BTreeMap<String, (u64, u64)>>,
) -> Vec<ChangeRow> {
    let mut by_path: BTreeMap<String, (Option<&Entry>, Vec<&Entry>)> = BTreeMap::new();
    for entry in branch {
        by_path.entry(entry.path.clone()).or_default().0 = Some(entry);
    }
    for entry in local {
        by_path.entry(entry.path.clone()).or_default().1.push(entry);
    }
    let mut rows = Vec::new();
    for (path, (branch_entry, locals)) in by_path {
        if uncommitted_only && locals.is_empty() {
            continue;
        }
        let deleted_locally = locals.iter().any(|e| e.status == ChangeStatus::Deleted)
            && !locals.iter().any(|e| e.area == Some(Area::Untracked));
        let (from_status, from_old) = match (branch_entry, uncommitted_only) {
            (Some(b), false) => (b.status, b.old_path.clone()),
            _ => {
                if locals
                    .iter()
                    .any(|e| e.area == Some(Area::Untracked) || e.status == ChangeStatus::Added)
                {
                    (ChangeStatus::Added, None)
                } else if let Some(r) = locals
                    .iter()
                    .find(|e| matches!(e.status, ChangeStatus::Renamed | ChangeStatus::Copied))
                {
                    (r.status, r.old_path.clone())
                } else {
                    (ChangeStatus::Modified, None)
                }
            }
        };
        if deleted_locally && from_status == ChangeStatus::Added {
            continue;
        }
        let status = if deleted_locally {
            ChangeStatus::Deleted
        } else {
            from_status
        };
        let old_path = if deleted_locally { None } else { from_old };
        let (added, removed) = if uncommitted_only {
            match locals.as_slice() {
                [single] => (single.added, single.removed),
                _ => (None, None),
            }
        } else if let Some((a, r)) = line_counts.and_then(|c| c.get(&path)) {
            (Some(*a), Some(*r))
        } else {
            let mut sources: Vec<&Entry> = locals.clone();
            if let Some(b) = branch_entry {
                sources.push(b);
            }
            match sources.as_slice() {
                [single] => (single.added, single.removed),
                _ => (None, None),
            }
        };
        rows.push(ChangeRow {
            committed: branch_entry.is_some(),
            staged: locals.iter().any(|e| e.area == Some(Area::Staged)),
            unstaged: locals.iter().any(|e| e.area == Some(Area::Unstaged)),
            untracked: locals.iter().any(|e| e.area == Some(Area::Untracked)),
            conflicted: locals.iter().any(|e| e.conflicted),
            binary: false,
            path,
            status,
            old_path,
            added,
            removed,
        });
    }
    rows
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChangesQuery {
    /// Base ref name; `None` uses the repo's default base.
    pub base: Option<String>,
    pub uncommitted_only: bool,
}

/// The Changes list for a worktree: every file that differs from the merge base, committed or not.
pub fn changes(worktree: &Path, query: &ChangesQuery) -> Result<Changes> {
    let local = status_entries(worktree)?;
    let untracked: Vec<String> = local
        .iter()
        .filter(|e| e.area == Some(Area::Untracked))
        .map(|e| e.path.clone())
        .collect();
    let head = git_ok(
        worktree,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
    );
    let compare = compare_status(worktree, head.as_deref(), query.base.clone());
    let rows = match &compare {
        CompareStatus::Ready { merge_base, .. } if !query.uncommitted_only => {
            let head = head.clone().unwrap_or_default();
            let raw = git_read(
                worktree,
                &[
                    "diff",
                    "--raw",
                    "--numstat",
                    "-z",
                    "-M",
                    "-C",
                    merge_base,
                    &head,
                    "--",
                ],
            )?;
            let branch = parse_raw_numstat(&raw);
            let (counts, binary) = line_counts(worktree, merge_base, &untracked)?;
            let mut rows = merge_rows(&branch, &local, false, Some(&counts));
            for row in &mut rows {
                row.binary = binary.contains(&row.path);
            }
            rows
        }
        _ => {
            let counts = match head {
                Some(ref head) => line_counts(worktree, head, &untracked).ok(),
                None => None,
            };
            let mut rows = merge_rows(&[], &local, true, None);
            if let Some((counts, binary)) = counts {
                for row in &mut rows {
                    if let Some((a, r)) = counts.get(&row.path) {
                        row.added = Some(*a);
                        row.removed = Some(*r);
                    }
                    row.binary = binary.contains(&row.path);
                }
            }
            rows
        }
    };
    let added = rows.iter().filter_map(|r| r.added).sum();
    let removed = rows.iter().filter_map(|r| r.removed).sum();
    Ok(Changes {
        compare,
        rows,
        added,
        removed,
    })
}

/// How HEAD compares with `base` (the repo's default base when `None`), never guessing a base.
fn compare_status(worktree: &Path, head: Option<&str>, base: Option<String>) -> CompareStatus {
    let base_name = base.or_else(|| default_base_ref(worktree));
    match (head, &base_name) {
        (None, _) => CompareStatus::UnbornHead,
        (_, None) => CompareStatus::NoBase,
        (Some(head), Some(base)) => match qualify_ref(worktree, base) {
            None => CompareStatus::InvalidBase { base: base.clone() },
            Some(qualified) => match if review_base::is_review_base_ref(&qualified) {
                git_ok(worktree, &["rev-parse", &qualified])
            } else {
                git_ok(worktree, &["merge-base", &qualified, head])
            } {
                None => CompareStatus::NoMergeBase { base: base.clone() },
                Some(merge_base) => {
                    let counts = git_ok(
                        worktree,
                        &[
                            "rev-list",
                            "--left-right",
                            "--count",
                            &format!("{qualified}...{head}"),
                        ],
                    )
                    .unwrap_or_default();
                    let mut parts = counts
                        .split_whitespace()
                        .map(|v| v.parse::<u64>().unwrap_or(0));
                    let behind = parts.next().unwrap_or(0);
                    let ahead = parts.next().unwrap_or(0);
                    CompareStatus::Ready {
                        base: base.clone(),
                        merge_base,
                        ahead,
                        behind,
                    }
                }
            },
        },
    }
}

/// One commit of a branch's history (`git.commits`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommitInfo {
    pub oid: String,
    pub short: String,
    pub subject: String,
    pub author: String,
    pub author_email: String,
    /// Seconds since the epoch.
    pub time: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BranchCommits {
    pub compare: CompareStatus,
    /// Newest first: the commits on HEAD since its merge base with the base.
    pub commits: Vec<CommitInfo>,
    /// More commits exist than `limit` allowed.
    pub truncated: bool,
}

/// The commits on HEAD since its merge base with `base`; none while the compare isn't ready.
pub fn branch_commits(
    worktree: &Path,
    base: Option<String>,
    limit: usize,
) -> Result<BranchCommits> {
    let head = git_ok(
        worktree,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
    );
    let compare = compare_status(worktree, head.as_deref(), base);
    let CompareStatus::Ready { merge_base, .. } = &compare else {
        return Ok(BranchCommits {
            compare,
            commits: Vec::new(),
            truncated: false,
        });
    };
    let count = (limit + 1).to_string();
    let output = git_read(
        worktree,
        &[
            "log",
            "-z",
            "--max-count",
            &count,
            "--format=%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%at",
            &format!("{merge_base}..HEAD"),
            "--",
        ],
    )?;
    let mut commits: Vec<CommitInfo> = output
        .split('\0')
        .filter_map(|record| {
            let mut fields = record.trim_start_matches('\n').split('\x1f');
            let oid = fields.next().filter(|oid| !oid.is_empty())?.to_string();
            Some(CommitInfo {
                oid,
                short: fields.next().unwrap_or_default().to_string(),
                subject: fields.next().unwrap_or_default().to_string(),
                author: fields.next().unwrap_or_default().to_string(),
                author_email: fields.next().unwrap_or_default().to_string(),
                time: fields
                    .next()
                    .and_then(|time| time.trim().parse().ok())
                    .unwrap_or_default(),
            })
        })
        .collect();
    let truncated = commits.len() > limit;
    commits.truncate(limit);
    Ok(BranchCommits {
        compare,
        commits,
        truncated,
    })
}

pub fn commit_rows(worktree: &Path, from: Option<&str>, to: &str) -> Result<Vec<ChangeRow>> {
    let from = match from {
        Some(from) => from.to_string(),
        None => git(worktree, &["hash-object", "-t", "tree", "--stdin"])?
            .trim()
            .to_string(),
    };
    let raw = git_read(
        worktree,
        &[
            "diff",
            "--raw",
            "--numstat",
            "-z",
            "-M",
            "-C",
            &from,
            to,
            "--",
        ],
    )?;
    Ok(parse_raw_numstat(&raw)
        .into_iter()
        .map(|entry| ChangeRow {
            binary: entry.added.is_none() && entry.removed.is_none(),
            path: entry.path,
            status: entry.status,
            old_path: entry.old_path,
            added: entry.added,
            removed: entry.removed,
            committed: true,
            staged: false,
            unstaged: false,
            untracked: false,
            conflicted: false,
        })
        .collect())
}

/// The files one commit changed against its first parent (`git.commit_files`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommitFiles {
    pub oid: String,
    /// `None` for a root commit, which is compared with the empty tree.
    pub parent: Option<String>,
    pub subject: String,
    pub files: Vec<CommitFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommitFile {
    pub path: String,
    pub status: ChangeStatus,
    pub old_path: Option<String>,
    pub added: Option<u64>,
    pub removed: Option<u64>,
    pub binary: bool,
}

pub fn commit_files(worktree: &Path, sha: &str) -> Result<CommitFiles> {
    let oid = resolve_commit(worktree, sha).ok_or_else(|| anyhow::anyhow!("no commit {sha}"))?;
    let parent = first_parent(worktree, &oid);
    let files = commit_rows(worktree, parent.as_deref(), &oid)?
        .into_iter()
        .map(|row| CommitFile {
            path: row.path,
            status: row.status,
            old_path: row.old_path,
            added: row.added,
            removed: row.removed,
            binary: row.binary,
        })
        .collect();
    let subject = git_read(worktree, &["show", "-s", "--format=%s", &oid])?
        .trim()
        .to_string();
    Ok(CommitFiles {
        oid,
        parent,
        subject,
        files,
    })
}

pub(crate) fn resolve_commit(worktree: &Path, name: &str) -> Option<String> {
    git_ok(
        worktree,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ],
    )
}

pub(crate) fn first_parent(worktree: &Path, oid: &str) -> Option<String> {
    git_ok(
        worktree,
        &["rev-parse", "--verify", "--quiet", &format!("{oid}^1")],
    )
}

#[cfg(test)]
mod tests;
