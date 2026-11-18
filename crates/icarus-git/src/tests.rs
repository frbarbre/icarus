use super::*;

pub(crate) struct Repo {
    _dir: tempfile::TempDir,
    pub(crate) root: PathBuf,
}

pub(crate) fn run(dir: &Path, args: &[&str]) {
    git(dir, args).unwrap_or_else(|e| panic!("{e:#}"));
}

pub(crate) fn write(dir: &Path, path: &str, content: &str) {
    let full = dir.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(full, content).expect("write");
}

/// A repo with `main` holding a.txt and b.txt, cloned so `origin/main` exists.
pub(crate) fn repo() -> Repo {
    let dir = tempfile::tempdir().expect("tempdir");
    let upstream = dir.path().join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    run(&upstream, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.email", "t@example.com"),
        ("user.name", "T"),
        ("commit.gpgsign", "false"),
    ] {
        run(&upstream, &["config", key, value]);
    }
    write(&upstream, "a.txt", "one\ntwo\nthree\n");
    write(&upstream, "b.txt", "bee\n");
    run(&upstream, &["add", "."]);
    run(&upstream, &["commit", "-q", "-m", "base"]);
    let root = dir.path().join("clone");
    run(
        dir.path(),
        &[
            "clone",
            "-q",
            upstream.to_str().expect("utf8"),
            root.to_str().expect("utf8"),
        ],
    );
    for (key, value) in [
        ("user.email", "t@example.com"),
        ("user.name", "T"),
        ("commit.gpgsign", "false"),
    ] {
        run(&root, &["config", key, value]);
    }
    Repo { _dir: dir, root }
}

fn row<'a>(changes: &'a Changes, path: &str) -> &'a ChangeRow {
    changes
        .rows
        .iter()
        .find(|r| r.path == path)
        .unwrap_or_else(|| panic!("no row {path}: {:?}", changes.rows))
}

#[test]
fn a_process_git_leaves_holding_its_output_cannot_outlast_the_deadline() {
    let repo = repo();
    let started = std::time::Instant::now();
    let _ = git_timeout(
        &repo.root,
        &["-c", "alias.hold=!sleep 30 &", "hold"],
        std::time::Duration::from_secs(2),
    );
    let took = started.elapsed();
    assert!(took < std::time::Duration::from_secs(5), "took {took:?}");
}

#[test]
fn a_git_run_that_never_finishes_is_killed_with_a_clear_error() {
    let repo = repo();
    let started = std::time::Instant::now();
    let error = git_timeout(
        &repo.root,
        &["-c", "alias.hang=!sleep 30", "hang"],
        std::time::Duration::from_millis(300),
    )
    .expect_err("never finishes");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert!(
        error.to_string().contains("did not finish within"),
        "{error}"
    );
}

#[test]
fn canonical_remote_normalizes_common_url_shapes() {
    for url in [
        "https://github.com/Acme/Widgets.git",
        "git@github.com:acme/widgets.git",
        "ssh://git@github.com/acme/widgets",
        "ssh://git@github.com:22/acme/widgets.git",
    ] {
        assert_eq!(
            canonical_remote(url).as_deref(),
            Some("github.com/acme/widgets"),
            "{url}"
        );
    }
}

#[test]
fn repo_info_finds_root_remote_and_default_base() {
    let repo = repo();
    let info = repo_info(&repo.root).expect("info");
    assert_eq!(
        std::fs::canonicalize(&info.root).ok(),
        std::fs::canonicalize(&repo.root).ok()
    );
    assert_eq!(info.default_base.as_deref(), Some("origin/main"));
    assert!(
        info.remote.is_none(),
        "local path remotes are not canonical"
    );
}

#[test]
fn one_row_per_file_across_committed_staged_unstaged_and_untracked() {
    let repo = repo();
    let wt_path = repo.root.parent().expect("parent").join("wt");
    create_worktree(&CreateWorktree {
        repo: &repo.root,
        path: &wt_path,
        branch: "feature",
        base: "origin/main",
        fetch: false,
    })
    .expect("create worktree");
    let wt = wt_path.as_path();

    write(wt, "new.txt", "x\ny\n");
    write(wt, "a.txt", "one\nTWO\nthree\n");
    run(wt, &["add", "."]);
    run(wt, &["commit", "-q", "-m", "work"]);
    write(wt, "a.txt", "one\nTWO\nthree\nfour\n");
    write(wt, "untracked.txt", "u1\nu2\nu3\n");
    write(wt, "b.txt", "bee\nsting\n");
    run(wt, &["add", "b.txt"]);
    run(wt, &["mv", "new.txt", "renamed.txt"]);

    let changes = changes(wt, &ChangesQuery::default()).expect("changes");
    assert!(
        matches!(
            changes.compare,
            CompareStatus::Ready {
                ahead: 1,
                behind: 0,
                ..
            }
        ),
        "{:?}",
        changes.compare
    );

    let a = row(&changes, "a.txt");
    assert_eq!(a.status, ChangeStatus::Modified);
    assert!(a.committed && a.unstaged);
    assert_eq!(
        (a.added, a.removed),
        (Some(2), Some(1)),
        "counted from merge base to disk"
    );

    let untracked = row(&changes, "untracked.txt");
    assert_eq!(untracked.status, ChangeStatus::Added);
    assert_eq!(untracked.added, Some(3));

    let b = row(&changes, "b.txt");
    assert!(b.staged && !b.committed);

    assert!(changes.rows.iter().any(|r| r.path == "renamed.txt"));
    assert_eq!(
        changes.added,
        changes.rows.iter().filter_map(|r| r.added).sum::<u64>()
    );

    let uncommitted = super::changes(
        wt,
        &ChangesQuery {
            base: None,
            uncommitted_only: true,
        },
    )
    .expect("uncommitted");
    let a = row(&uncommitted, "a.txt");
    assert_eq!(
        (a.added, a.removed),
        (Some(1), Some(0)),
        "only the uncommitted change"
    );
    assert!(uncommitted.rows.iter().all(|r| r.path != "new.txt"));
}

#[test]
fn added_on_branch_then_deleted_on_disk_nets_to_no_row() {
    let repo = repo();
    let wt = repo.root.as_path();
    run(wt, &["checkout", "-q", "-b", "feature"]);
    write(wt, "temp.txt", "t\n");
    run(wt, &["add", "."]);
    run(wt, &["commit", "-q", "-m", "add temp"]);
    std::fs::remove_file(wt.join("temp.txt")).expect("rm");
    let changes = changes(wt, &ChangesQuery::default()).expect("changes");
    assert!(
        changes.rows.iter().all(|r| r.path != "temp.txt"),
        "{:?}",
        changes.rows
    );
}

#[test]
fn committed_modify_then_deleted_on_disk_shows_deleted() {
    let repo = repo();
    let wt = repo.root.as_path();
    run(wt, &["checkout", "-q", "-b", "feature"]);
    write(wt, "b.txt", "changed\n");
    run(wt, &["commit", "-q", "-am", "edit b"]);
    std::fs::remove_file(wt.join("b.txt")).expect("rm");
    let changes = changes(wt, &ChangesQuery::default()).expect("changes");
    assert_eq!(row(&changes, "b.txt").status, ChangeStatus::Deleted);
}

#[test]
fn missing_base_never_guesses_and_falls_back_to_uncommitted() {
    let repo = repo();
    write(&repo.root, "a.txt", "edited\n");
    let changes = changes(
        &repo.root,
        &ChangesQuery {
            base: Some("origin/nope".into()),
            uncommitted_only: false,
        },
    )
    .expect("changes");
    assert_eq!(
        changes.compare,
        CompareStatus::InvalidBase {
            base: "origin/nope".into()
        }
    );
    assert_eq!(row(&changes, "a.txt").status, ChangeStatus::Modified);
}

#[test]
fn local_state_reports_dirtiness_unpushed_commits_and_conflicts() {
    let repo = repo();
    let root = repo.root.as_path();
    let state = local_state(root).expect("state");
    assert!(state.is_default_branch && !state.has_uncommitted_changes);
    assert_eq!(state.upstream.as_deref(), Some("origin/main"));

    run(
        root,
        &["checkout", "-q", "-b", "feature", "--track", "origin/main"],
    );
    write(root, "a.txt", "changed\n");
    run(root, &["commit", "-q", "-am", "work"]);
    let state = local_state(root).expect("state");
    assert!(!state.is_default_branch);
    assert_eq!(state.unpushed_commits, 1);
    assert!(!state.has_uncommitted_changes);

    run(root, &["checkout", "-q", "-b", "other", "main"]);
    write(root, "a.txt", "conflicting\n");
    run(root, &["commit", "-q", "-am", "other"]);
    let merge = git(root, &["merge", "feature"]);
    assert!(merge.is_err(), "merge should stop on the conflict");
    let state = local_state(root).expect("state");
    assert!(state.has_local_conflicts && state.has_uncommitted_changes);
}

#[test]
fn remove_refuses_dirty_worktrees_unless_forced() {
    let repo = repo();
    let wt_path = repo.root.parent().expect("parent").join("wt2");
    create_worktree(&CreateWorktree {
        repo: &repo.root,
        path: &wt_path,
        branch: "w2",
        base: "origin/main",
        fetch: false,
    })
    .expect("create");
    write(&wt_path, "dirty.txt", "x");
    assert!(matches!(
        remove_worktree(&repo.root, &wt_path, false, true),
        Err(RemoveError::Dirty)
    ));
    remove_worktree(&repo.root, &wt_path, true, true).expect("forced remove");
    assert!(!wt_path.exists());
    assert_eq!(list_worktrees(&repo.root).expect("list").len(), 1);
}

#[test]
fn commit_files_list_one_commit_against_its_first_parent() {
    let repo = repo();
    let base = commit_files(&repo.root, "HEAD").expect("root commit");
    assert_eq!(base.parent, None, "a root commit has no parent");
    let paths: Vec<&str> = base.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["a.txt", "b.txt"]);
    assert!(base.files.iter().all(|f| f.status == ChangeStatus::Added));

    write(&repo.root, "a.txt", "one\n2\nthree\nfour\n");
    run(&repo.root, &["mv", "b.txt", "c.txt"]);
    write(&repo.root, "d.txt", "new\n");
    run(&repo.root, &["add", "-A"]);
    run(&repo.root, &["commit", "-q", "-m", "Edit, move, add"]);
    let head = commit_files(&repo.root, "HEAD").expect("commit");
    assert_eq!(head.subject, "Edit, move, add");
    assert_eq!(head.parent.as_deref(), Some(base.oid.as_str()));
    let a = head.files.iter().find(|f| f.path == "a.txt").expect("a");
    assert_eq!(
        (a.status, a.added, a.removed),
        (ChangeStatus::Modified, Some(2), Some(1))
    );
    let moved = head.files.iter().find(|f| f.path == "c.txt").expect("c");
    assert_eq!(moved.status, ChangeStatus::Renamed);
    assert_eq!(moved.old_path.as_deref(), Some("b.txt"));
    assert!(head.files.iter().any(|f| f.path == "d.txt"));
    assert_eq!(head.files.len(), 3);

    assert!(commit_files(&repo.root, "nope").is_err());
}

#[test]
fn branch_commits_list_only_what_the_branch_added_since_its_base() {
    let repo = repo();
    let empty = branch_commits(&repo.root, Some("origin/main".into()), 10).expect("commits");
    assert!(empty.commits.is_empty() && !empty.truncated);
    assert!(matches!(empty.compare, CompareStatus::Ready { .. }));

    run(&repo.root, &["checkout", "-q", "-b", "feature"]);
    for (index, subject) in ["First change", "Second | change"].iter().enumerate() {
        write(&repo.root, "a.txt", &format!("edit {index}\n"));
        run(&repo.root, &["commit", "-q", "-am", subject]);
    }
    let listed = branch_commits(&repo.root, Some("origin/main".into()), 10).expect("commits");
    let subjects: Vec<&str> = listed.commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(
        subjects,
        ["Second | change", "First change"],
        "newest first"
    );
    let newest = &listed.commits[0];
    assert_eq!(
        newest.oid,
        git(&repo.root, &["rev-parse", "HEAD"])
            .expect("head")
            .trim()
    );
    assert!(newest.oid.starts_with(&newest.short) && newest.short.len() >= 7);
    assert_eq!(
        (newest.author.as_str(), newest.author_email.as_str()),
        ("T", "t@example.com")
    );
    assert!(newest.time > 1_600_000_000);

    let capped = branch_commits(&repo.root, Some("origin/main".into()), 1).expect("commits");
    assert_eq!(capped.commits.len(), 1);
    assert!(capped.truncated);

    let missing = branch_commits(&repo.root, Some("origin/nope".into()), 10).expect("commits");
    assert!(missing.commits.is_empty());
    assert_eq!(
        missing.compare,
        CompareStatus::InvalidBase {
            base: "origin/nope".into()
        }
    );
}
