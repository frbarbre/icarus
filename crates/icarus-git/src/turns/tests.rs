use super::*;
use crate::tests::{repo, run, write};

fn meta(prompt: &str, completed_at: u64) -> TurnMeta {
    TurnMeta {
        prompt: prompt.into(),
        agent_id: "agent-1".into(),
        provider: Some("claude".into()),
        started_at: completed_at - 1,
        completed_at,
    }
}

fn index_bytes(root: &Path) -> Vec<u8> {
    let path = git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )
    .expect("index path");
    std::fs::read(path.trim()).expect("read index")
}

#[test]
fn a_turn_records_what_changed_without_touching_the_real_index() {
    let repo = repo();
    let root = repo.root.as_path();
    write(root, "staged.txt", "staged\n");
    run(root, &["add", "staged.txt"]);
    write(root, "a.txt", "one\ntwo\nthree\nunstaged\n");
    let index_before = index_bytes(root);
    let staged_before = git(root, &["diff", "--cached", "--name-status"]).expect("staged");
    assert_eq!(staged_before, "A\tstaged.txt\n");

    let start = start_turn(root).expect("start");
    write(root, "a.txt", "one\nTWO\nthree\nunstaged\n");
    write(root, "new/file.txt", "new\n");
    std::fs::remove_file(root.join("b.txt")).expect("rm");
    let turn = finish_turn(root, &start, &meta("do it", 1_700_000_000_000))
        .expect("finish")
        .expect("a change was made");

    assert_eq!(index_bytes(root), index_before, "real index untouched");
    assert_eq!(
        git(root, &["diff", "--cached", "--name-status"]).expect("staged"),
        staged_before
    );
    assert_eq!(turn.parent, start);
    assert_eq!(turn.files, 3);
    assert_eq!(turn.meta.prompt, "do it");
    assert!(turn.name.starts_with("001700000000000-"));
    assert!(turn.ref_name.starts_with(TURN_REF_PREFIX));

    let rows = crate::commit_rows(root, Some(&turn.parent), &turn.oid).expect("rows");
    let summary: Vec<(String, crate::ChangeStatus)> =
        rows.iter().map(|r| (r.path.clone(), r.status)).collect();
    assert_eq!(
        summary,
        [
            ("a.txt".to_string(), crate::ChangeStatus::Modified),
            ("b.txt".to_string(), crate::ChangeStatus::Deleted),
            ("new/file.txt".to_string(), crate::ChangeStatus::Added),
        ]
    );
    assert_eq!(resolve_turn(root, "latest").expect("latest"), turn);
    assert_eq!(resolve_turn(root, &turn.oid[..10]).expect("by oid"), turn);
    assert_eq!(resolve_turn(root, &turn.ref_name).expect("by ref"), turn);
}

#[test]
fn empty_turns_are_skipped_and_ignored_files_do_not_count() {
    let repo = repo();
    let root = repo.root.as_path();
    write(root, ".gitignore", "ignored/\n");
    let start = start_turn(root).expect("start");
    write(root, "ignored/cache.bin", "x");
    assert_eq!(
        finish_turn(root, &start, &meta("nothing", 5)).expect("finish"),
        None
    );
    assert!(list_turns(root).expect("list").is_empty());
    assert!(resolve_turn(root, "latest").is_err());
}

#[test]
fn turns_list_newest_first_and_are_pruned_per_worktree() {
    let repo = repo();
    let root = repo.root.as_path();
    for n in 1..=4u64 {
        let start = start_turn(root).expect("start");
        write(root, "counter.txt", &n.to_string());
        finish_turn(root, &start, &meta(&format!("turn {n}"), 10 + n)).expect("finish");
    }
    let prompts: Vec<String> = list_turns(root)
        .expect("list")
        .into_iter()
        .map(|t| t.meta.prompt)
        .collect();
    assert_eq!(prompts, ["turn 4", "turn 3", "turn 2", "turn 1"]);
    prune(root, 2).expect("prune");
    assert_eq!(list_turns(root).expect("list").len(), 2);

    let other = root.parent().expect("parent").join("other");
    crate::create_worktree(&crate::CreateWorktree {
        repo: root,
        path: &other,
        branch: "other",
        base: "origin/main",
        fetch: false,
    })
    .expect("worktree");
    assert!(
        list_turns(&other).expect("list").is_empty(),
        "turn refs are per worktree"
    );
    assert!(is_worktree(&other));
}
