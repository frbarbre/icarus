use std::path::Path;
use std::process::Command;

use icarus_git::ops::{self, CommitRequest, DiscardKind, PushRequest};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn configure(dir: &Path) {
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}

/// A repo with one commit on `main`, cloned from a bare remote so push and pull have somewhere to go.
fn repo_with_remote() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = tempfile::tempdir().expect("tempdir");
    let remote = root.path().join("remote.git");
    let work = root.path().join("work");
    git(
        root.path(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            remote.to_str().expect("utf8"),
        ],
    );
    git(
        root.path(),
        &[
            "clone",
            "-q",
            remote.to_str().expect("utf8"),
            work.to_str().expect("utf8"),
        ],
    );
    configure(&work);
    std::fs::write(work.join("a.txt"), "one\n").expect("write");
    std::fs::write(work.join("b.txt"), "bee\n").expect("write");
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    git(&work, &["push", "-q", "-u", "origin", "main"]);
    (root, remote, work)
}

fn status(dir: &Path) -> String {
    git(dir, &["status", "--porcelain"])
}

#[test]
fn discard_throws_away_only_the_uncommitted_change() {
    let (_root, _remote, work) = repo_with_remote();
    git(&work, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(work.join("a.txt"), "one\ncommitted\n").expect("write");
    git(&work, &["commit", "-qam", "branch change"]);
    std::fs::write(work.join("a.txt"), "one\ncommitted\nlocal\n").expect("write");
    std::fs::write(work.join("new.txt"), "new\n").expect("write");
    std::fs::remove_file(work.join("b.txt")).expect("remove");

    let kind = |path: &str| ops::discard_kind(&work, path).expect("kind");
    assert_eq!(kind("a.txt"), Some(DiscardKind::Discard));
    assert_eq!(kind("new.txt"), Some(DiscardKind::DeleteUntracked));
    assert_eq!(kind("b.txt"), Some(DiscardKind::Restore));
    assert_eq!(DiscardKind::Restore.label(), "Restore file");

    let done =
        ops::discard(&work, &["a.txt".into(), "new.txt".into(), "b.txt".into()]).expect("discard");
    assert_eq!(done.len(), 3);
    assert_eq!(status(&work), "", "every uncommitted change is gone");
    assert_eq!(
        std::fs::read_to_string(work.join("a.txt")).expect("read"),
        "one\ncommitted\n",
        "the committed branch change stays"
    );
    assert!(!work.join("new.txt").exists());
    assert!(work.join("b.txt").exists());
    assert!(ops::discard_kind(&work, "a.txt").expect("kind").is_none());
}

#[test]
fn discard_handles_staged_new_files_and_refuses_escapes_and_clean_paths() {
    let (_root, _remote, work) = repo_with_remote();
    std::fs::write(work.join("staged.txt"), "s\n").expect("write");
    git(&work, &["add", "staged.txt"]);
    assert_eq!(
        ops::discard_kind(&work, "staged.txt").expect("kind"),
        Some(DiscardKind::DeleteUntracked)
    );
    ops::discard(&work, &["staged.txt".into()]).expect("discard staged new file");
    assert!(!work.join("staged.txt").exists());
    assert_eq!(status(&work), "");

    for escape in ["../outside", "/etc/passwd", "a/../../x", ""] {
        let error = ops::discard(&work, &[escape.into()]).expect_err(escape);
        assert!(
            error.message.contains("outside the worktree"),
            "{escape}: {error}"
        );
    }
    let error = ops::discard(&work, &["a.txt".into()]).expect_err("nothing to discard");
    assert!(error.message.contains("no uncommitted change"), "{error}");
}

#[test]
fn stage_and_unstage_round_trip() {
    let (_root, _remote, work) = repo_with_remote();
    std::fs::write(work.join("a.txt"), "two\n").expect("write");
    std::fs::write(work.join("c.txt"), "c\n").expect("write");
    ops::stage(&work, &["a.txt".into(), "c.txt".into()]).expect("stage");
    assert_eq!(
        git(&work, &["diff", "--cached", "--name-only"]),
        "a.txt\nc.txt"
    );
    ops::unstage(&work, &["a.txt".into()]).expect("unstage");
    assert_eq!(git(&work, &["diff", "--cached", "--name-only"]), "c.txt");
}

#[test]
fn commit_takes_what_is_staged_or_everything_when_nothing_is() {
    let (_root, _remote, work) = repo_with_remote();
    let error = ops::commit(&work, &CommitRequest::default()).expect_err("no message");
    assert_eq!(error.message, "Write a commit message first.");
    let error = ops::commit(
        &work,
        &CommitRequest {
            message: "empty".into(),
            ..Default::default()
        },
    )
    .expect_err("clean tree");
    assert_eq!(error.message, "Nothing to commit.");

    std::fs::write(work.join("a.txt"), "two\n").expect("write");
    std::fs::write(work.join("new.txt"), "n\n").expect("write");
    ops::stage(&work, &["a.txt".into()]).expect("stage");
    let outcome = ops::commit(
        &work,
        &CommitRequest {
            message: "only staged".into(),
            ..Default::default()
        },
    )
    .expect("commit staged");
    assert!(!outcome.committed_all);
    assert_eq!(outcome.summary, "only staged");
    assert_eq!(status(&work), "?? new.txt", "the untracked file stays out");

    let outcome = ops::commit(
        &work,
        &CommitRequest {
            message: "the rest".into(),
            ..Default::default()
        },
    )
    .expect("commit all");
    assert!(outcome.committed_all, "nothing staged commits everything");
    assert_eq!(status(&work), "");

    std::fs::write(work.join("a.txt"), "three\n").expect("write");
    let amended = ops::commit(
        &work,
        &CommitRequest {
            amend: true,
            ..Default::default()
        },
    )
    .expect("amend keeps the message");
    assert_eq!(amended.summary, "the rest");
    assert_eq!(git(&work, &["rev-list", "--count", "HEAD"]), "3");
}

#[test]
fn commit_hooks_run_and_their_message_is_returned_verbatim() {
    let (_root, _remote, work) = repo_with_remote();
    let hook = work.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\necho 'lint failed: a.txt' >&2\nexit 1\n").expect("hook");
    let mut permissions = std::fs::metadata(&hook).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&hook, permissions).expect("chmod");
    std::fs::write(work.join("a.txt"), "two\n").expect("write");
    let error = ops::commit(
        &work,
        &CommitRequest {
            message: "blocked".into(),
            ..Default::default()
        },
    )
    .expect_err("hook refuses");
    assert_eq!(error.message, "lint failed: a.txt");
}

#[test]
fn push_publishes_and_force_with_lease_refuses_when_the_remote_moved() {
    let (root, remote, work) = repo_with_remote();
    git(&work, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(work.join("a.txt"), "feature\n").expect("write");
    git(&work, &["commit", "-qam", "feature"]);
    let state = ops::push(&work, PushRequest::default()).expect("first push sets upstream");
    assert_eq!(state.upstream.as_deref(), Some("origin/feature"));
    assert_eq!((state.ahead, state.behind), (0, 0));

    // Someone else pushes to the same branch.
    let other = root.path().join("other");
    git(
        root.path(),
        &[
            "clone",
            "-q",
            "-b",
            "feature",
            remote.to_str().expect("utf8"),
            other.to_str().expect("utf8"),
        ],
    );
    configure(&other);
    std::fs::write(other.join("b.txt"), "theirs\n").expect("write");
    git(&other, &["commit", "-qam", "theirs"]);
    git(&other, &["push", "-q"]);

    git(&work, &["commit", "-q", "--amend", "-m", "rewritten"]);
    let error = ops::push(&work, PushRequest::default()).expect_err("non-fast-forward");
    assert!(error.message.contains("rejected"), "{error}");
    git(&work, &["fetch", "-q"]);
    // The lease is the tracking ref we just fetched, so it allows the force; a stale lease must not.
    git(
        &other,
        &["commit", "-q", "--allow-empty", "-m", "theirs again"],
    );
    git(&other, &["push", "-q"]);
    let error = ops::push(
        &work,
        PushRequest {
            force_with_lease: true,
            ..Default::default()
        },
    )
    .expect_err("the remote moved after our fetch");
    assert!(
        error.message.contains("stale info") || error.message.contains("rejected"),
        "{error}"
    );
    git(&work, &["fetch", "-q"]);
    let state = ops::push(
        &work,
        PushRequest {
            force_with_lease: true,
            ..Default::default()
        },
    )
    .expect("force with a fresh lease");
    assert_eq!(state.ahead, 0);
}

#[test]
fn pull_and_sync_bring_in_remote_commits_then_push_local_ones() {
    let (root, remote, work) = repo_with_remote();
    let other = root.path().join("other");
    git(
        root.path(),
        &[
            "clone",
            "-q",
            remote.to_str().expect("utf8"),
            other.to_str().expect("utf8"),
        ],
    );
    configure(&other);
    std::fs::write(other.join("b.txt"), "theirs\n").expect("write");
    git(&other, &["commit", "-qam", "theirs"]);
    git(&other, &["push", "-q"]);

    git(&work, &["fetch", "-q"]);
    assert_eq!(ops::sync_state(&work).expect("state").behind, 1);
    let state = ops::pull(&work).expect("pull");
    assert_eq!((state.ahead, state.behind), (0, 0));
    assert_eq!(
        std::fs::read_to_string(work.join("b.txt")).expect("read"),
        "theirs\n"
    );

    std::fs::write(work.join("a.txt"), "mine\n").expect("write");
    git(&work, &["commit", "-qam", "mine"]);
    std::fs::write(other.join("c.txt"), "c\n").expect("write");
    git(&other, &["add", "c.txt"]);
    git(&other, &["commit", "-qm", "theirs 2"]);
    git(&other, &["push", "-q"]);
    let state = ops::sync(&work).expect("sync");
    assert_eq!((state.ahead, state.behind), (0, 0));
    git(&other, &["pull", "-q", "--no-edit"]);
    assert_eq!(
        std::fs::read_to_string(other.join("a.txt")).expect("read"),
        "mine\n"
    );
}

#[test]
fn pull_without_an_upstream_explains_itself() {
    let (_root, _remote, work) = repo_with_remote();
    git(&work, &["checkout", "-q", "-b", "local-only"]);
    let error = ops::pull(&work).expect_err("no upstream");
    assert!(error.message.contains("no upstream"), "{error}");
}
