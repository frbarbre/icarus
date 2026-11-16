//! `workspace.adopt`: registering a worktree made outside Icarus, and what it refuses.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use icarus_client::Client;
use serde_json::json;

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
    let child = Command::new(env!("CARGO_BIN_EXE_icarus-server"))
        .args(["--home", home.to_str().expect("utf8 home"), "serve"])
        .env("ICARUS_WORKSPACE_ROOT", home.join("workspaces"))
        .env("ICARUS_CONFIG_DIR", home.join("config"))
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
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

async fn next_event(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<icarus_client::Event>,
    topic: &str,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline.into(), events.recv())
            .await
            .unwrap_or_else(|_| panic!("no {topic} event in time"))
            .expect("event stream open");
        if event.topic == topic {
            return event.payload;
        }
    }
}

#[tokio::test]
async fn an_existing_worktree_is_adopted_in_place_and_refusals_are_clear() {
    let home = tempfile::tempdir().expect("home");
    let dirs = tempfile::tempdir().expect("dirs");
    let dirs = std::fs::canonicalize(dirs.path()).expect("canonical");
    let repo = dirs.join("widgets");
    std::fs::create_dir_all(&repo).expect("repo dir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "T"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("readme.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let outside = dirs.join("elsewhere").join("fix-login");
    let outside_str = outside.to_str().expect("utf8");
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "fix/login", outside_str],
    );
    std::fs::write(outside.join("work.txt"), "in progress\n").expect("write");

    let _server = start_server(home.path());
    let (client, mut events) = Client::connect(home.path(), "test").await.expect("connect");

    let unregistered = client
        .request("workspace.adopt", json!({ "path": outside }))
        .await
        .expect_err("no project yet");
    let message = format!("{unregistered:#}");
    assert!(message.contains("icarus project add"), "{message}");

    let project = client
        .request("project.add", json!({ "path": repo }))
        .await
        .expect("add project");

    let adopted = client
        .request("workspace.adopt", json!({ "path": outside }))
        .await
        .expect("adopt");
    assert_eq!(adopted["name"], "fix-login");
    assert_eq!(adopted["branch"], "fix/login");
    assert_eq!(adopted["project_id"], project["id"]);
    assert_eq!(adopted["agents"], json!([]), "no agent is started");
    let changed = next_event(&mut events, "workspace.changed").await;
    assert_eq!(changed["workspaces"], json!([adopted["id"]]));

    let listed = client
        .request("workspace.list", json!({ "project": project["id"] }))
        .await
        .expect("list");
    let listed = listed.as_array().expect("array");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["branch"], "fix/login");
    assert_eq!(
        PathBuf::from(listed[0]["path"].as_str().expect("path")),
        outside
    );
    assert!(outside.join("work.txt").exists(), "nothing moved");
    assert!(
        !home.path().join("workspaces").join("widgets").exists(),
        "nothing under the workspace root"
    );

    let again = client
        .request(
            "workspace.adopt",
            json!({ "path": outside, "name": "other" }),
        )
        .await
        .expect_err("already a workspace");
    assert!(
        format!("{again:#}").contains("already the workspace"),
        "{again:#}"
    );

    let twin = dirs.join("more").join("fix-login");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "twin",
            twin.to_str().expect("utf8"),
        ],
    );
    let clash = client
        .request("workspace.adopt", json!({ "path": twin }))
        .await
        .expect_err("name taken");
    assert!(format!("{clash:#}").contains("--name"), "{clash:#}");
    let renamed = client
        .request(
            "workspace.adopt",
            json!({ "path": twin, "name": "Twin Fix", "project": project["name"] }),
        )
        .await
        .expect("adopt under another name");
    assert_eq!(renamed["name"], "twin-fix");

    let main = client
        .request("workspace.adopt", json!({ "path": repo }))
        .await
        .expect_err("the main checkout");
    assert!(format!("{main:#}").contains("main checkout"), "{main:#}");

    let plain = dirs.join("plain");
    std::fs::create_dir_all(&plain).expect("plain dir");
    let not_git = client
        .request("workspace.adopt", json!({ "path": plain }))
        .await
        .expect_err("not a worktree");
    assert!(
        format!("{not_git:#}").contains("not_a_worktree"),
        "{not_git:#}"
    );

    let detached = dirs.join("detached");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            detached.to_str().expect("utf8"),
        ],
    );
    let headless = client
        .request("workspace.adopt", json!({ "path": detached }))
        .await
        .expect_err("detached HEAD");
    assert!(
        format!("{headless:#}").contains("detached HEAD"),
        "{headless:#}"
    );

    let hidden = client
        .request(
            "workspace.remove",
            json!({ "id": adopted["id"], "force": true }),
        )
        .await
        .expect("remove the adopted workspace");
    assert_eq!(hidden["worktree_deleted"], false);
    assert!(
        outside.join("work.txt").exists(),
        "a worktree Icarus didn't create is only hidden"
    );
    client
        .request(
            "workspace.remove",
            json!({ "id": adopted["id"], "force": true, "delete_worktree": true, "delete_branch": true }),
        )
        .await
        .expect("delete the adopted worktree when asked");
    assert!(!outside.exists(), "deleted when asked");
    assert_eq!(
        git(&repo, &["branch", "--list", "fix/login"]),
        "",
        "and its branch, when asked"
    );
}

#[tokio::test]
async fn adopting_a_discovered_worktree_keeps_its_record_and_shows_it() {
    let home = tempfile::tempdir().expect("home");
    let dirs = tempfile::tempdir().expect("dirs");
    let dirs = std::fs::canonicalize(dirs.path()).expect("canonical");
    let repo = dirs.join("widgets");
    std::fs::create_dir_all(&repo).expect("repo dir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "T"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("readme.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let outside = dirs.join("elsewhere").join("spike");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "spike",
            outside.to_str().expect("utf8"),
        ],
    );
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).expect("config");
    std::fs::write(
        config.join("icarus.json"),
        r#"{"workspace":{"external":"hide"}}"#,
    )
    .expect("settings");

    let _server = start_server(home.path());
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
    let project = client
        .request("project.add", json!({ "path": repo }))
        .await
        .expect("add project");
    let hidden = client
        .request(
            "workspace.list",
            json!({ "project": project["id"], "refresh": true, "include_hidden": true }),
        )
        .await
        .expect("list");
    let found = hidden[0].clone();
    assert_eq!(found["hidden"], true);
    let adopted = client
        .request("workspace.adopt", json!({ "path": outside }))
        .await
        .expect("adopt");
    assert_eq!(adopted["id"], found["id"]);
    assert_eq!(adopted["origin"], "adopted");
    assert_eq!(adopted["hidden"], false);
    let listed = client
        .request(
            "workspace.list",
            json!({ "project": project["id"], "refresh": true }),
        )
        .await
        .expect("list");
    assert_eq!(listed.as_array().map(Vec::len), Some(1));
}
