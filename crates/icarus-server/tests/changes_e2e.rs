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
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn index_bytes(worktree: &Path) -> Vec<u8> {
    let path = git(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    );
    std::fs::read(path.trim()).expect("index")
}

fn paths(listing: &Value) -> Vec<String> {
    listing["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| r["path"].as_str().expect("path").to_string())
        .collect()
}

async fn turn(client: &Client, agent: &str, text: &str) {
    client
        .request("agent.send", json!({ "id": agent, "text": text }))
        .await
        .expect("send");
    client
        .request("agent.wait", json!({ "id": agent, "timeout_ms": 10000 }))
        .await
        .expect("turn ends");
}

#[tokio::test]
async fn agent_turns_are_snapshotted_and_filters_apply_in_every_scope() {
    let home = tempfile::tempdir().expect("tempdir");
    let repo = tempfile::tempdir().expect("repo");
    git(repo.path(), &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.email", "t@example.com"),
        ("user.name", "T"),
        ("commit.gpgsign", "false"),
    ] {
        git(repo.path(), &["config", key, value]);
    }
    std::fs::write(repo.path().join("readme.md"), "hello\n").expect("write");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    let overlay = json!({ "changes": { "facets": [ { "id": "area", "label": "Areas",
        "values": [ { "id": "backend", "label": "Backend" }, { "id": "frontend", "label": "Frontend" } ],
        "rules": [ { "value": "backend", "when": { "path": "server/**" } },
                   { "value": "frontend", "when": { "path": "web/**" } } ] } ] } });
    std::fs::write(repo.path().join("icarus.local.json"), overlay.to_string()).expect("overlay");

    let mut server = start_server(home.path());
    let fake = env!("CARGO_BIN_EXE_icarus-fake-agent");
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
    let project = client
        .request("project.add", json!({ "path": repo.path() }))
        .await
        .expect("project");
    let workspace = client
        .request(
            "workspace.create",
            json!({ "project": project["id"], "name": "turns", "agent": false }),
        )
        .await
        .expect("workspace");
    let id = workspace["id"].as_str().expect("id").to_string();
    let path = PathBuf::from(workspace["path"].as_str().expect("path"));
    let agent = client
        .request(
            "agent.create",
            json!({ "cwd": path, "workspace": id, "command": [fake] }),
        )
        .await
        .expect("agent");
    let agent = agent["id"].as_str().expect("agent id").to_string();

    std::fs::write(path.join("staged.txt"), "staged\n").expect("write");
    git(&path, &["add", "staged.txt"]);
    let index_before = index_bytes(&path);

    turn(&client, &agent, "write:server/api.go package main").await;
    turn(&client, &agent, "just talk").await;
    turn(&client, &agent, "write:web/app.test.ts t").await;
    assert_eq!(
        index_bytes(&path),
        index_before,
        "the real index is untouched"
    );

    let turns = client
        .request("workspace.turns", json!({ "id": id }))
        .await
        .expect("turns");
    let turns = turns.as_array().expect("turns");
    assert_eq!(turns.len(), 2, "the empty turn is skipped: {turns:?}");
    assert_eq!(turns[0]["meta"]["prompt"], "write:web/app.test.ts t");
    assert_eq!(turns[0]["meta"]["agentId"], agent.as_str());
    assert_eq!(turns[1]["files"], 1);
    assert!(
        turns[0]["ref_name"]
            .as_str()
            .is_some_and(|r| r.starts_with("refs/worktree/icarus-turns/"))
    );

    let latest = client
        .request(
            "workspace.changes",
            json!({ "id": id, "scope": "turn:latest" }),
        )
        .await
        .expect("latest turn");
    assert_eq!(paths(&latest), ["web/app.test.ts"]);
    assert_eq!(latest["rows"][0]["facets"]["type"], "test");
    assert_eq!(latest["rows"][0]["facets"]["area"], "frontend");
    assert_eq!(latest["rows"][0]["discardable"], false);
    let older = client
        .request(
            "workspace.changes",
            json!({ "id": id, "scope": format!("turn:{}", turns[1]["name"].as_str().expect("name")) }),
        )
        .await
        .expect("older turn");
    assert_eq!(paths(&older), ["server/api.go"]);

    let hidden_once = client
        .request(
            "workspace.changes",
            json!({ "id": id, "scope": "turn:latest", "hide": { "type": ["test"] } }),
        )
        .await
        .expect("hidden");
    assert!(paths(&hidden_once).is_empty());
    assert_eq!(hidden_once["unfiltered"]["files"], 1);
    assert_eq!(hidden_once["totals"]["files"], 0);

    let base = client
        .request("workspace.changes", json!({ "id": id, "text": "SERVER" }))
        .await
        .expect("base");
    assert_eq!(base["compare"]["status"], "ready");
    assert_eq!(paths(&base), ["server/api.go"]);
    assert_eq!(base["totals"]["added"], 1);

    let explained = client
        .request(
            "workspace.changes.explain",
            json!({ "id": id, "path": "web/app.test.ts" }),
        )
        .await
        .expect("explain");
    assert_eq!(explained["facets"][0]["value"], "test");
    assert_eq!(explained["facets"][0]["rule_id"], "test-heuristic");
    assert_eq!(explained["facets"][1]["value"], "frontend");

    let filter = client
        .request(
            "workspace.changes.filter",
            json!({ "id": id, "hide": { "type": ["test"], "area": ["backend"] } }),
        )
        .await
        .expect("filter");
    assert_eq!(
        filter["hidden"],
        json!({ "type": ["test"], "area": ["backend"] })
    );

    drop(client);
    drop(server);
    server = start_server(home.path());
    let (client, _events) = Client::connect(home.path(), "test")
        .await
        .expect("reconnect");
    let uncommitted = client
        .request(
            "workspace.changes",
            json!({ "id": id, "scope": "uncommitted" }),
        )
        .await
        .expect("uncommitted");
    assert_eq!(
        paths(&uncommitted),
        ["staged.txt"],
        "hidden values persist across restarts"
    );
    assert_eq!(uncommitted["facets"][0]["active"], true);
    let shown = client
        .request(
            "workspace.changes.filter",
            json!({ "id": id, "show": { "type": ["test"] } }),
        )
        .await
        .expect("show");
    assert_eq!(shown["hidden"], json!({ "area": ["backend"] }));
    let reset = client
        .request(
            "workspace.changes.filter",
            json!({ "id": id, "reset": true }),
        )
        .await
        .expect("reset");
    assert_eq!(reset["hidden"], json!({}));
    let bad = client
        .request(
            "workspace.changes",
            json!({ "id": id, "scope": "range:a..b" }),
        )
        .await;
    assert!(bad.is_err());
    drop(server);
}
