use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use icarus_client::Client;
use serde_json::{Value, json};
use tempfile::TempDir;

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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

/// A server with its own settings dir and fixture GitHub.
struct World {
    home: TempDir,
    config: TempDir,
    fixtures: PathBuf,
    _server: ServerProcess,
    client: Arc<Client>,
    events: tokio::sync::mpsc::UnboundedReceiver<icarus_client::Event>,
}

impl World {
    async fn new(settings: Value, fixtures: Value) -> Self {
        let home = tempfile::tempdir().expect("home");
        let config = tempfile::tempdir().expect("config");
        std::fs::write(config.path().join("icarus.json"), settings.to_string()).expect("settings");
        let fixtures_path = home.path().join("gh-fixtures.json");
        std::fs::write(&fixtures_path, fixtures.to_string()).expect("fixtures");
        let child = Command::new(env!("CARGO_BIN_EXE_icarus-server"))
            .args(["--home", home.path().to_str().expect("utf8"), "serve"])
            .env("ICARUS_WORKSPACE_ROOT", home.path().join("workspaces"))
            .env("ICARUS_CONFIG_DIR", config.path())
            .env("ICARUS_GH_FIXTURES", &fixtures_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn server");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(home.path().join("server.sock").exists()
            && home.path().join("server.token").exists())
        {
            assert!(Instant::now() < deadline, "server did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        let (client, events) = Client::connect(home.path(), "test").await.expect("connect");
        Self {
            home,
            config,
            fixtures: fixtures_path,
            _server: ServerProcess(child),
            client,
            events,
        }
    }

    fn set_settings(&self, settings: Value) {
        std::fs::write(self.config.path().join("icarus.json"), settings.to_string())
            .expect("settings");
    }

    async fn request(&self, method: &str, params: Value) -> Value {
        self.client
            .request(method, params)
            .await
            .unwrap_or_else(|e| panic!("{method}: {e:#}"))
    }

    async fn notifications(&self) -> Vec<Value> {
        self.request("notification.list", json!({}))
            .await
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// Waits until `count` notifications of `kind` exist, then returns them.
    async fn wait_for(&self, kind: &str, count: usize) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let found: Vec<Value> = self
                .notifications()
                .await
                .into_iter()
                .filter(|n| n["kind"] == kind)
                .collect();
            if found.len() >= count {
                return found;
            }
            assert!(
                Instant::now() < deadline,
                "no {kind} notification: {:?}",
                self.notifications().await
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn agent(&self, prompt: &str) -> String {
        let work = self.home.path().join("work");
        std::fs::create_dir_all(&work).expect("work dir");
        let info = self
            .request(
                "agent.create",
                json!({ "cwd": work, "title": "Fixer", "prompt": prompt }),
            )
            .await;
        let id = info["id"].as_str().expect("id").to_owned();
        self.request("agent.wait", json!({ "id": id, "timeout_ms": 10000 }))
            .await;
        id
    }

    async fn send(&self, id: &str, text: &str) {
        self.request("agent.send", json!({ "id": id, "text": text }))
            .await;
        self.request("agent.wait", json!({ "id": id, "timeout_ms": 10000 }))
            .await;
    }
}

fn fake_agent_settings(extra: Value) -> Value {
    let mut settings = json!({
        "agents": { "commands": { "claude": [env!("CARGO_BIN_EXE_icarus-fake-agent")] } },
    });
    for (key, value) in extra.as_object().into_iter().flatten() {
        settings[key] = value.clone();
    }
    settings
}

#[tokio::test]
async fn agent_turns_failures_and_questions_become_stored_notifications() {
    let mut world = World::new(fake_agent_settings(json!({})), json!({ "viewer": "me" })).await;
    let id = world.agent("hello").await;

    let done = world.wait_for("agent_turn_done", 1).await;
    assert_eq!(done[0]["title"], "Fixer: finished");
    assert_eq!(done[0]["body"], "echo: hello");
    assert_eq!(done[0]["data"]["agent_id"], id.as_str());
    assert_eq!(done[0]["read"], false);
    assert!(done[0]["workspace_id"].is_null());

    let deadline = Instant::now() + Duration::from_secs(10);
    let pushed = loop {
        let event = tokio::time::timeout_at(deadline.into(), world.events.recv())
            .await
            .expect("notification event in time")
            .expect("event stream open");
        if event.topic == "notification" {
            break event.payload;
        }
    };
    assert_eq!(pushed["id"], done[0]["id"]);
    assert_eq!(pushed["sound"], true);

    world.send(&id, "fail the build").await;
    let failed = world.wait_for("agent_failed", 1).await;
    assert_eq!(failed[0]["title"], "Fixer: turn failed");

    world.send(&id, "ask Which base branch?").await;
    let waiting = world.wait_for("agent_waiting", 1).await;
    assert_eq!(waiting[0]["title"], "Fixer: has a question");
    assert_eq!(waiting[0]["body"], "Which base branch?");
    assert_eq!(
        world.wait_for("agent_turn_done", 1).await.len(),
        1,
        "a second turn inside the cooldown is not news again"
    );

    world.set_settings(fake_agent_settings(
        json!({ "notifications": { "agent_failed": false, "sound": false } }),
    ));
    let kinds = world.request("notification.kinds", json!({})).await;
    assert_eq!(kinds["kinds"]["agent_failed"], false);
    assert_eq!(kinds["sound"], false);
    let other = world.agent("fail again").await;
    world.wait_for("agent_turn_done", 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        world
            .notifications()
            .await
            .iter()
            .all(|n| n["data"]["agent_id"] != other.as_str()),
        "a kind switched off is not stored"
    );

    let first = done[0]["id"].as_str().expect("id");
    let read = world
        .request("notification.read", json!({ "id": first }))
        .await;
    assert_eq!(read["updated"], 1);
    let unread = world
        .request("notification.list", json!({ "unread_only": true }))
        .await;
    assert!(
        unread
            .as_array()
            .expect("list")
            .iter()
            .all(|n| n["id"] != first)
    );
    assert!(
        world
            .client
            .request("notification.read", json!({ "id": "ntf-missing" }))
            .await
            .is_err()
    );
    world
        .request("notification.read", json!({ "all": true }))
        .await;
    let unread = world
        .request("notification.list", json!({ "unread_only": true }))
        .await;
    assert_eq!(unread, json!([]));
    let limited = world
        .request("notification.list", json!({ "limit": 1 }))
        .await;
    assert_eq!(limited.as_array().map(Vec::len), Some(1));
    world.request("notification.clear", json!({})).await;
    assert!(world.notifications().await.is_empty());
}

impl World {
    /// The next `agent.state` push for `id`, which must come within a second.
    async fn next_state(&mut self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let event = tokio::time::timeout_at(deadline.into(), self.events.recv())
                .await
                .expect("agent.state pushed promptly")
                .expect("event stream open");
            if event.topic == "agent.state" && event.payload["id"] == id {
                return event.payload;
            }
        }
    }

    async fn states_until(&mut self, id: &str, last: (&str, u64)) -> Vec<(String, u64)> {
        let mut seen = Vec::new();
        loop {
            let state = self.next_state(id).await;
            let shown = (
                state["state"].as_str().unwrap_or_default().to_owned(),
                state["background_tasks"].as_u64().unwrap_or_default(),
            );
            let done = shown.0 == last.0 && shown.1 == last.1;
            seen.push(shown);
            if done {
                return seen;
            }
        }
    }
}

#[tokio::test]
async fn every_agent_state_change_is_pushed_as_it_happens() {
    let mut world = World::new(fake_agent_settings(json!({})), json!({ "viewer": "me" })).await;
    let work = world.home.path().join("work");
    std::fs::create_dir_all(&work).expect("work dir");
    let info = world
        .request("agent.create", json!({ "cwd": work, "title": "Fixer" }))
        .await;
    let id = info["id"].as_str().expect("id").to_owned();
    world.request("agent.wait", json!({ "id": id })).await;

    world
        .request("agent.send", json!({ "id": id, "text": "permit Bash" }))
        .await;
    let seen = world.states_until(&id, ("waiting", 0)).await;
    assert!(seen.contains(&("working".to_owned(), 0)), "{seen:?}");
    let open = world.request("agent.info", json!({ "id": id })).await;
    let request = &open["permission_requests"][0];
    assert_eq!(request["tool_name"], "Bash");
    let waiting = world.wait_for("agent_waiting", 1).await;
    assert_eq!(waiting[0]["title"], "Fixer: needs permission");

    world
        .request(
            "agent.answer",
            json!({ "id": id, "request_id": request["request_id"], "decision": "allow" }),
        )
        .await;
    assert_eq!(
        world.states_until(&id, ("idle", 0)).await,
        vec![("working".to_owned(), 0), ("idle".to_owned(), 0)]
    );
    let reply = world.request("agent.read", json!({ "id": id })).await;
    assert!(reply.to_string().contains("permit allow"), "{reply}");

    world
        .request(
            "agent.send",
            json!({ "id": id, "text": "background:300 watch" }),
        )
        .await;
    assert_eq!(
        world.states_until(&id, ("idle", 0)).await,
        vec![
            ("working".to_owned(), 0),
            ("working".to_owned(), 1),
            ("idle".to_owned(), 1),
            ("idle".to_owned(), 0),
        ]
    );
}

fn pull(number: u64, branch: &str, head: &str, extra: Value) -> Value {
    let mut pull = json!({
        "id": format!("PR_{number}"),
        "number": number,
        "title": format!("Pull request {number}"),
        "url": format!("https://github.com/acme/widgets/pull/{number}"),
        "state": "OPEN",
        "isDraft": false,
        "headRefName": branch,
        "baseRefName": "main",
        "headRefOid": head,
        "headRef": { "name": branch },
        "baseRef": { "name": "main" },
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "CLEAN",
        "author": { "login": "me" },
        "repository": { "name": "widgets", "owner": { "login": "acme" }, "autoMergeAllowed": true },
    });
    for (key, value) in extra.as_object().into_iter().flatten() {
        pull[key] = value.clone();
    }
    pull
}

fn failing_checks() -> Value {
    json!({ "nodes": [ { "commit": { "statusCheckRollup": { "state": "FAILURE", "contexts": { "nodes": [
        { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "FAILURE",
          "detailsUrl": "https://github.com/acme/widgets/actions/runs/77/job/1" }
    ] } } } } ] })
}

#[tokio::test]
async fn the_github_poll_notifies_ci_failures_merges_and_review_requests_once() {
    let repos = tempfile::tempdir().expect("repos");
    let repo = repos.path().join("widgets");
    std::fs::create_dir_all(&repo).expect("repo");
    git(
        repos.path(),
        &["init", "-q", "--bare", "-b", "main", "origin.git"],
    );
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "me@example.com"]);
    git(&repo, &["config", "user.name", "Me"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("readme.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widgets.git",
        ],
    );
    let settings = json!({ "rules": {
        "projects": ["widgets"],
        "pr": { "enabled": true, "reviewInbox": { "enabled": true, "agent": "claude" } },
    } });
    let world = World::new(settings, json!({ "viewer": "me" })).await;
    let project = world
        .request("project.add", json!({ "path": repo, "name": "widgets" }))
        .await;
    let origin = repos.path().join("origin.git");
    git(
        &repo,
        &[
            "remote",
            "set-url",
            "origin",
            origin.to_str().expect("utf8"),
        ],
    );
    git(&repo, &["push", "-q", "origin", "main"]);
    let mut heads = Vec::new();
    let mut ids = Vec::new();
    for name in ["feat-red", "feat-done"] {
        let ws = world
            .request(
                "workspace.create",
                json!({ "project": project["id"], "name": name, "agent": false }),
            )
            .await;
        heads.push(git(
            Path::new(ws["path"].as_str().expect("path")),
            &["rev-parse", "HEAD"],
        ));
        ids.push(ws["id"].as_str().expect("id").to_owned());
    }
    std::fs::write(
        &world.fixtures,
        json!({
            "viewer": "me",
            "pulls": [
                pull(11, "feat-red", &heads[0], json!({ "commits": failing_checks(), "mergeStateStatus": "UNSTABLE" })),
                pull(12, "feat-done", &heads[1], json!({ "state": "MERGED" })),
            ],
            "inbox": [pull(20, "their-branch", "abc123", json!({ "author": { "login": "alice" }, "title": "Add search" }))],
        })
        .to_string(),
    )
    .expect("fixtures");

    let tick = world.request("rules.tick", json!({})).await;
    assert_eq!(tick["dry_run"], true, "{tick}");
    assert_eq!(tick["notifications"], 3, "{tick}");
    let ci = world.wait_for("ci_failed", 1).await;
    assert_eq!(ci[0]["title"], "CI failed on #11");
    assert_eq!(ci[0]["body"], "Pull request 11: build failing");
    assert_eq!(ci[0]["workspace_id"], ids[0].as_str());
    assert_eq!(
        ci[0]["data"]["url"],
        "https://github.com/acme/widgets/pull/11"
    );
    let merged = world.wait_for("pr_merged", 1).await;
    assert_eq!(merged[0]["title"], "#12 merged");
    assert_eq!(merged[0]["workspace_id"], ids[1].as_str());
    let review = world.wait_for("review_requested", 1).await;
    assert_eq!(review[0]["title"], "Review requested: #20");
    assert_eq!(review[0]["body"], "Add search by alice");

    let again = world.request("rules.tick", json!({})).await;
    assert_eq!(again["notifications"], 0, "the same state is news once");
    assert_eq!(world.notifications().await.len(), 3);
}
