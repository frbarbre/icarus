//! The slow, cancellable part of `workspace.create`: pull request start points, fetches, `git worktree
//! add`, and Orca's `worktree.sharedDirectories` / `.worktreeinclude` paths.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use icarus_protocol::RpcError;
use serde_json::Value;
use tokio::sync::Notify;

use crate::server::rpc;

#[derive(Default)]
pub(crate) struct CancelFlag {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancelFlag {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    async fn cancelled(&self) {
        if !self.cancelled.load(Ordering::SeqCst) {
            self.notify.notified().await;
        }
    }
}

/// Creates in flight by their client-chosen id, so `workspace.create_cancel` can reach them.
#[derive(Default)]
pub(crate) struct Creates {
    running: Mutex<HashMap<String, Arc<CancelFlag>>>,
}

impl Creates {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<CancelFlag>>> {
        self.running.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A cancel that arrives first (requests on one connection can overtake) is kept for the create.
    pub(crate) fn begin(&self, id: &str) -> Arc<CancelFlag> {
        Arc::clone(self.lock().entry(id.to_owned()).or_default())
    }

    pub(crate) fn finish(&self, id: &str) {
        self.lock().remove(id);
    }

    pub(crate) fn cancel(&self, id: &str) -> bool {
        let mut running = self.lock();
        let known = running.contains_key(id);
        running.entry(id.to_owned()).or_default().cancel();
        known
    }
}

pub(crate) fn cancelled_error() -> RpcError {
    rpc("cancelled", "the workspace create was cancelled")
}

/// Runs `work` unless `flag` is cancelled first; dropping `work` kills its git processes.
pub(crate) async fn or_cancelled<T>(
    flag: &CancelFlag,
    work: impl Future<Output = Result<T, RpcError>>,
) -> Result<T, RpcError> {
    tokio::select! {
        biased;
        () = flag.cancelled() => Err(cancelled_error()),
        result = work => result,
    }
}

/// Kills a child's whole process group when dropped unfinished, so a cancelled fetch takes its
/// transport helpers (ssh, `git-remote-https`) with it.
struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(group) = self
            .0
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(rustix::process::Pid::from_raw)
            && let Err(error) =
                rustix::process::kill_process_group(group, rustix::process::Signal::KILL)
            && error != rustix::io::Errno::SRCH
        {
            tracing::warn!("stopping a cancelled git: {error}");
        }
    }
}

async fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let child = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("run git {}: {e}", args.join(" ")))?;
    let mut guard = GroupGuard(child.id());
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("run git {}: {e}", args.join(" ")))?;
    guard.0 = None;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn git_ok(repo: &Path, args: &[&str]) -> bool {
    git(repo, args).await.is_ok()
}

/// A pull request number from `123`, `#123` or a PR URL; a URL also names its repository.
pub(crate) fn parse_pr(value: &Value) -> Result<(Option<(String, String)>, u64), RpcError> {
    let invalid = || {
        rpc(
            "invalid_params",
            "pr must be a pull request number or URL (https://github.com/owner/repo/pull/123)",
        )
    };
    if let Some(number) = value.as_u64() {
        return Ok((None, number));
    }
    let text = value.as_str().ok_or_else(invalid)?.trim();
    if let Ok(number) = text.trim_start_matches('#').parse::<u64>() {
        return Ok((None, number));
    }
    let (repository, rest) = text.split_once("/pull/").ok_or_else(invalid)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let number = digits.parse::<u64>().map_err(|_| invalid())?;
    let mut parts = repository.trim_end_matches('/').rsplit('/');
    let name = parts.next().filter(|s| !s.is_empty()).ok_or_else(invalid)?;
    let owner = parts.next().filter(|s| !s.is_empty()).ok_or_else(invalid)?;
    Ok((Some((owner.to_owned(), name.to_owned())), number))
}

/// The pull request a workspace starts from.
#[derive(Debug, Clone)]
pub(crate) struct PrHead {
    pub number: u64,
    pub head_branch: String,
    pub cross_repository: bool,
}

pub(crate) fn pull_ref(number: u64) -> String {
    format!("refs/icarus/pull/{number}")
}

#[derive(Debug, Clone)]
pub(crate) struct WorktreePlan {
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    /// Where a new branch starts: the base, or the fetched pull request head.
    pub start: String,
    /// Fetched first, when it names a remote branch (`origin/main`).
    pub fetch: Option<String>,
    pub pr: Option<PrHead>,
    /// Whether the branch existed before, so a cancelled or failed create only deletes its own.
    pub branch_existed: bool,
}

pub(crate) async fn branch_exists(repo: &Path, branch: &str) -> bool {
    git_ok(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await
}

/// Fetches what the plan needs and adds the worktree.
pub(crate) async fn add_worktree(plan: &WorktreePlan) -> Result<(), RpcError> {
    let failed = |e: String| rpc("git_failed", e);
    if let Some(fetch) = &plan.fetch
        && let Some((remote, branch)) = fetch.split_once('/')
        && let Err(error) = git(
            &plan.repo,
            &["fetch", "--quiet", "--no-tags", remote, branch],
        )
        .await
    {
        tracing::warn!("fetching {fetch}: {error}");
    }
    if let Some(pr) = &plan.pr {
        let refspec = format!("+refs/pull/{}/head:{}", pr.number, pull_ref(pr.number));
        git(
            &plan.repo,
            &["fetch", "--quiet", "--no-tags", "origin", &refspec],
        )
        .await
        .map_err(|e| {
            rpc(
                "git_failed",
                format!("could not fetch pull request #{}: {e}", pr.number),
            )
        })?;
        if !pr.cross_repository
            && let Err(error) = git(
                &plan.repo,
                &["fetch", "--quiet", "--no-tags", "origin", &pr.head_branch],
            )
            .await
        {
            tracing::warn!("fetching origin/{}: {error}", pr.head_branch);
        }
    }
    if let Some(parent) = plan.path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| failed(e.to_string()))?;
    }
    let path = plan.path.to_string_lossy();
    if plan.branch_existed {
        git(&plan.repo, &["worktree", "add", &path, &plan.branch])
            .await
            .map_err(failed)?;
    } else {
        git(
            &plan.repo,
            &["worktree", "add", "-b", &plan.branch, &path, &plan.start],
        )
        .await
        .map_err(failed)?;
    }
    if let Some(pr) = &plan.pr
        && !pr.cross_repository
        && !plan.branch_existed
    {
        let upstream = format!("origin/{}", pr.head_branch);
        if git_ok(
            &plan.repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/remotes/{upstream}"),
            ],
        )
        .await
            && let Err(error) = git(
                &plan.repo,
                &[
                    "branch",
                    &format!("--set-upstream-to={upstream}"),
                    &plan.branch,
                ],
            )
            .await
        {
            tracing::warn!("tracking {upstream}: {error}");
        }
    }
    Ok(())
}

/// Undoes a create that was cancelled or failed after `git worktree add` may have started.
pub(crate) async fn discard_worktree(plan: &WorktreePlan) {
    let path = plan.path.to_string_lossy();
    if plan.path.exists() {
        if let Err(error) = git(&plan.repo, &["worktree", "remove", "--force", &path]).await {
            tracing::warn!("removing the cancelled worktree: {error}");
        }
        if plan.path.exists()
            && let Err(error) = std::fs::remove_dir_all(&plan.path)
        {
            tracing::warn!("removing {}: {error}", plan.path.display());
        }
    }
    if let Err(error) = git(&plan.repo, &["worktree", "prune"]).await {
        tracing::warn!("pruning worktrees: {error}");
    }
    if !plan.branch_existed
        && branch_exists(&plan.repo, &plan.branch).await
        && let Err(error) = git(&plan.repo, &["branch", "-D", &plan.branch]).await
    {
        tracing::warn!("deleting the cancelled branch {}: {error}", plan.branch);
    }
}

/// The candidates git ignores in `root`; only those are shared or copied, as Orca does.
async fn ignored(root: &Path, candidates: &[String]) -> Vec<String> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let mut args = vec!["check-ignore", "--"];
    args.extend(candidates.iter().map(String::as_str));
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(&args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await;
    match output {
        // Exit 1 means none of them is ignored.
        Ok(output) => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.trim_end_matches('/').to_owned())
            .filter(|line| candidates.contains(line))
            .collect(),
        Err(error) => {
            tracing::warn!("git check-ignore in {}: {error}", root.display());
            Vec::new()
        }
    }
}

/// Copies `source` to `target`, as an APFS clone or reflink where the filesystem has them.
async fn clone_path(source: &Path, target: &Path) -> Result<(), String> {
    let attempts: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["-c", "-R", "-p"], &["-R", "-p"]]
    } else {
        &[&["-a", "--reflink=auto"], &["-R", "-p"]]
    };
    let mut last = String::new();
    for flags in attempts {
        let output = tokio::process::Command::new("cp")
            .args(*flags)
            .arg(source)
            .arg(target)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if output.status.success() {
            return Ok(());
        }
        last = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        // A failed clone can leave a partial copy behind.
        if target.is_dir() {
            std::fs::remove_dir_all(target).map_err(|e| e.to_string())?;
        } else if target.symlink_metadata().is_ok() {
            std::fs::remove_file(target).map_err(|e| e.to_string())?;
        }
    }
    Err(last)
}

/// Orca's `worktree.sharedDirectories` (linked, so one install serves every worktree) and
/// `.worktreeinclude` (private copies), taken from the project root; each failure is a warning.
pub(crate) async fn materialize_shared_paths(
    root: &Path,
    worktree: &Path,
    shared_directories: &[String],
    includes: &[String],
) -> Vec<String> {
    let mut warnings = Vec::new();
    let existing_directories: Vec<String> = shared_directories
        .iter()
        .filter(|path| root.join(path).is_dir())
        .cloned()
        .collect();
    let shared = ignored(root, &existing_directories).await;
    for path in &existing_directories {
        if !shared.contains(path) {
            warnings.push(format!(
                "{path} isn't shared: only gitignored directories can be"
            ));
        }
    }
    for path in &shared {
        let target = worktree.join(path);
        if target.symlink_metadata().is_ok() {
            continue;
        }
        if let Some(parent) = target.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            warnings.push(format!("{path}: {error}"));
            continue;
        }
        if let Err(error) = std::os::unix::fs::symlink(root.join(path), &target) {
            warnings.push(format!("linking {path}: {error}"));
        }
    }
    let existing_includes: Vec<String> = includes
        .iter()
        .filter(|path| !shared.contains(path) && root.join(path).symlink_metadata().is_ok())
        .cloned()
        .collect();
    for path in ignored(root, &existing_includes).await {
        let target = worktree.join(&path);
        if target.symlink_metadata().is_ok() {
            continue;
        }
        if let Some(parent) = target.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            warnings.push(format!("{path}: {error}"));
            continue;
        }
        if let Err(error) = clone_path(&root.join(&path), &target).await {
            warnings.push(format!("copying {path}: {error}"));
        }
    }
    warnings
}

/// Removes the shared-directory links from a worktree about to be removed, so they don't count as
/// untracked files; returns the ones removed, to put back if the removal is refused.
pub(crate) fn unlink_shared_paths(root: &Path, worktree: &Path, shared: &[String]) -> Vec<String> {
    let mut removed = Vec::new();
    for path in shared {
        let target = worktree.join(path);
        let is_our_link = std::fs::read_link(&target).is_ok_and(|link| link == root.join(path));
        if is_our_link {
            match std::fs::remove_file(&target) {
                Ok(()) => removed.push(path.clone()),
                Err(error) => tracing::warn!("unlinking {}: {error}", target.display()),
            }
        }
    }
    removed
}

pub(crate) fn relink_shared_paths(root: &Path, worktree: &Path, paths: &[String]) {
    for path in paths {
        if let Err(error) = std::os::unix::fs::symlink(root.join(path), worktree.join(path)) {
            tracing::warn!("restoring the {path} link: {error}");
        }
    }
}

pub(crate) fn read_worktree_include(root: &Path) -> Vec<String> {
    match std::fs::read_to_string(root.join(".worktreeinclude")) {
        Ok(text) if text.len() <= 256 * 1024 => crate::hooks::parse_worktree_include(&text),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pr_numbers_and_urls_parse() {
        assert_eq!(parse_pr(&json!(12)).expect("number"), (None, 12));
        assert_eq!(parse_pr(&json!("#34")).expect("hash"), (None, 34));
        assert_eq!(
            parse_pr(&json!("https://github.com/acme/widgets/pull/56/files")).expect("url"),
            (Some(("acme".into(), "widgets".into())), 56)
        );
        assert!(parse_pr(&json!("feature/x")).is_err());
        assert!(parse_pr(&json!(true)).is_err());
    }

    #[tokio::test]
    async fn a_cancel_before_the_create_starts_still_reaches_it() {
        let creates = Creates::default();
        assert!(!creates.cancel("c1"));
        let flag = creates.begin("c1");
        let result = or_cancelled(&flag, std::future::pending::<Result<(), RpcError>>()).await;
        assert_eq!(result.map_err(|e| e.code), Err("cancelled".to_owned()));
        creates.finish("c1");
    }
}
