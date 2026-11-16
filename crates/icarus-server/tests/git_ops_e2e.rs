use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use icarus_client::Client;
use serde_json::{Value, json};

struct ServerProcess {
    child: Child,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_server(home: &Path) -> ServerProcess {
    let _ = std::fs::remove_file(home.join("server.token"));
    let child = Command::new(env!("CARGO_BIN_EXE_icarus-server"))
        .args(["--home", home.to_str().expect("utf8 home"), "serve"])
        .env("ICARUS_WORKSPACE_ROOT", home.join("workspaces"))
        .env("ICARUS_CONFIG_DIR", home.join("config"))
        .env("ICARUS_GH_FIXTURES", home.join("no-gh"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(home.join("server.sock").exists() && home.join("server.token").exists()) {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    ServerProcess { child }
}

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

/// A project cloned from a bare remote, so workspace branches can be pushed and pulled.
fn project(root: &Path) -> (PathBuf, PathBuf) {
    let remote = root.join("remote.git");
    let repo = root.join("repo");
    git(
        root,
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
        root,
        &[
            "clone",
            "-q",
            remote.to_str().expect("utf8"),
            repo.to_str().expect("utf8"),
        ],
    );
    for (key, value) in [
        ("user.email", "t@example.com"),
        ("user.name", "T"),
        ("commit.gpgsign", "false"),
    ] {
        git(&repo, &["config", key, value]);
    }
    std::fs::write(repo.join("a.txt"), "one\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    git(&repo, &["push", "-q", "-u", "origin", "main"]);
    (remote, repo)
}

async fn next_git_event(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<icarus_client::Event>,
) -> Value {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("event in time")
            .expect("open stream");
        if event.topic == "git.changed" {
            return event.payload;
        }
    }
}

#[tokio::test]
async fn source_control_actions_run_in_the_worktree_and_announce_themselves() {
    let root = tempfile::tempdir().expect("root");
    let (remote, repo) = project(root.path());
    let home = tempfile::tempdir().expect("home");
    let _server = start_server(home.path());
    let (client, mut events) = Client::connect(home.path(), "icarus-app")
        .await
        .expect("connect");
    let project = client
        .request("project.add", json!({ "path": repo }))
        .await
        .expect("project");
    let workspace = client
        .request(
            "workspace.create",
            json!({ "project": project["id"], "name": "feature", "agent": false }),
        )
        .await
        .expect("workspace");
    let id = workspace["id"].clone();
    let path = PathBuf::from(workspace["path"].as_str().expect("path"));
    for (key, value) in [("user.email", "t@example.com"), ("user.name", "T")] {
        git(&path, &["config", key, value]);
    }

    let state = client
        .request("git.status", json!({ "workspace": id }))
        .await
        .expect("status");
    assert_eq!(
        state["published"], false,
        "a fresh workspace branch tracks its base, which isn't its own upstream"
    );
    let not_yet = client
        .request("git.pull", json!({ "workspace": id }))
        .await
        .expect_err("nothing of its own to pull");
    assert!(
        format!("{not_yet:#}").contains("push it first"),
        "{not_yet:#}"
    );

    std::fs::write(path.join("a.txt"), "two\n").expect("write");
    std::fs::write(path.join("scratch.txt"), "x\n").expect("write");
    let kind = client
        .request(
            "git.discard_kind",
            json!({ "workspace": id, "path": "scratch.txt" }),
        )
        .await
        .expect("kind");
    assert_eq!(kind["label"], "Delete untracked file");

    let discarded = client
        .request(
            "git.discard",
            json!({ "cwd": path, "paths": [path.join("scratch.txt")] }),
        )
        .await
        .expect("discard by absolute path from inside the worktree");
    assert_eq!(discarded["discarded"][0]["kind"], "delete_untracked");
    assert!(!path.join("scratch.txt").exists());
    assert_eq!(next_git_event(&mut events).await["op"], "discard");

    let refused = client
        .request(
            "git.discard",
            json!({ "workspace": id, "paths": ["../escape"] }),
        )
        .await
        .expect_err("outside the worktree");
    assert!(
        format!("{refused:#}").contains("outside the worktree"),
        "{refused:#}"
    );

    client
        .request("git.stage", json!({ "workspace": id, "paths": ["a.txt"] }))
        .await
        .expect("stage");
    assert_eq!(git(&path, &["diff", "--cached", "--name-only"]), "a.txt");
    client
        .request(
            "git.unstage",
            json!({ "workspace": id, "paths": ["a.txt"] }),
        )
        .await
        .expect("unstage");
    assert_eq!(git(&path, &["diff", "--cached", "--name-only"]), "");

    let empty = client
        .request("git.commit", json!({ "workspace": id, "message": "  " }))
        .await
        .expect_err("no message");
    assert!(format!("{empty:#}").contains("Write a commit message first."));
    let committed = client
        .request(
            "git.commit",
            json!({ "workspace": id, "message": "change a" }),
        )
        .await
        .expect("commit");
    assert_eq!(committed["summary"], "change a");
    assert_eq!(committed["committed_all"], true);
    assert_eq!(git(&path, &["status", "--porcelain"]), "");

    let history = client
        .request("git.commits", json!({ "workspace": id }))
        .await
        .expect("commits since the base");
    assert_eq!(history["compare"]["status"], "ready");
    let commits = history["commits"].as_array().expect("commits");
    assert_eq!(commits.len(), 1, "the base's own commit isn't listed");
    assert_eq!(commits[0]["subject"], "change a");
    assert_eq!(commits[0]["oid"], git(&path, &["rev-parse", "HEAD"]));
    assert_eq!(commits[0]["author"], "T");
    let files = client
        .request(
            "git.commit_files",
            json!({ "workspace": id, "sha": commits[0]["oid"] }),
        )
        .await
        .expect("a commit's files");
    assert_eq!(files["subject"], "change a");
    assert_eq!(files["parent"], git(&path, &["rev-parse", "HEAD^"]));
    assert_eq!(
        files["files"],
        json!([{ "path": "a.txt", "status": "modified", "old_path": null, "added": 1, "removed": 1, "binary": false }])
    );
    let unknown = client
        .request(
            "git.commit_files",
            json!({ "workspace": id, "sha": "nope" }),
        )
        .await
        .expect_err("an unknown commit");
    assert!(format!("{unknown:#}").contains("no commit nope"));
    let missing = client
        .request(
            "git.commits",
            json!({ "workspace": id, "base": "origin/nope" }),
        )
        .await
        .expect("an unknown base lists nothing rather than failing");
    assert_eq!(missing["compare"]["status"], "invalid_base");
    assert_eq!(missing["commits"], json!([]));

    let pushed = client
        .request("git.push", json!({ "workspace": id }))
        .await
        .expect("push publishes");
    let branch = pushed["branch"].as_str().expect("branch").to_string();
    assert_eq!(pushed["upstream"], format!("origin/{branch}"));
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%s", &branch]),
        "change a"
    );

    // Someone else adds to the branch; pull and sync bring it in.
    let other = root.path().join("other");
    git(
        root.path(),
        &[
            "clone",
            "-q",
            "-b",
            &branch,
            remote.to_str().expect("utf8"),
            other.to_str().expect("utf8"),
        ],
    );
    for (key, value) in [
        ("user.email", "o@example.com"),
        ("user.name", "O"),
        ("commit.gpgsign", "false"),
    ] {
        git(&other, &["config", key, value]);
    }
    std::fs::write(other.join("b.txt"), "theirs\n").expect("write");
    git(&other, &["add", "b.txt"]);
    git(&other, &["commit", "-q", "-m", "theirs"]);
    git(&other, &["push", "-q"]);
    let pulled = client
        .request("git.pull", json!({ "workspace": id }))
        .await
        .expect("pull");
    assert_eq!(
        (pulled["ahead"].as_u64(), pulled["behind"].as_u64()),
        (Some(0), Some(0))
    );
    assert!(path.join("b.txt").exists());

    std::fs::write(path.join("c.txt"), "c\n").expect("write");
    client
        .request("git.commit", json!({ "workspace": id, "message": "add c" }))
        .await
        .expect("commit c");
    let synced = client
        .request("git.sync", json!({ "workspace": id }))
        .await
        .expect("sync");
    assert_eq!(synced["ahead"], 0);

    let rejected = {
        git(&other, &["pull", "-q", "--no-edit"]);
        git(
            &other,
            &["commit", "-q", "--allow-empty", "-m", "theirs again"],
        );
        git(&other, &["push", "-q"]);
        git(&path, &["commit", "-q", "--amend", "-m", "rewritten"]);
        client
            .request("git.push", json!({ "workspace": id }))
            .await
            .expect_err("non-fast-forward")
    };
    assert!(
        format!("{rejected:#}").contains("rejected"),
        "git's own words: {rejected:#}"
    );
    let leased = client
        .request(
            "git.push",
            json!({ "workspace": id, "force_with_lease": true }),
        )
        .await
        .expect_err("our lease is stale: the remote moved since we last fetched");
    assert!(
        format!("{leased:#}").contains("stale info") || format!("{leased:#}").contains("rejected"),
        "{leased:#}"
    );
}
