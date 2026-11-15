use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use icarus_client::{Client, Event};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedReceiver;

const HEAD: &str = "1111111111111111111111111111111111111111";
const BASE_SHA: &str = "2222222222222222222222222222222222222222";

struct ServerProcess {
    child: Child,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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

struct World {
    home: TempDir,
    _config: TempDir,
    _repos: TempDir,
    fixtures: PathBuf,
    _server: ServerProcess,
    client: Arc<Client>,
    project: String,
}

impl World {
    async fn new(fixtures: Value) -> Self {
        let home = tempfile::tempdir().expect("home");
        let config = tempfile::tempdir().expect("config");
        let repos = tempfile::tempdir().expect("repos");
        let origin = repos.path().join("origin.git");
        let repo = repos.path().join("widgets");
        std::fs::create_dir_all(&repo).expect("repo dir");
        git(
            repos.path(),
            &["init", "-q", "--bare", "-b", "main", "origin.git"],
        );
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "me@example.com"]);
        git(&repo, &["config", "user.name", "Me"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("readme.md"), "one\ntwo\nthree\nfour\n").expect("write");
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
        let settings_path = config.path().join("icarus").join("settings.json");
        std::fs::create_dir_all(settings_path.parent().expect("parent")).expect("settings dir");
        std::fs::write(&settings_path, "{}").expect("settings");
        let fixtures_path = home.path().join("gh-fixtures.json");
        std::fs::write(&fixtures_path, fixtures.to_string()).expect("fixtures");

        let child = Command::new(env!("CARGO_BIN_EXE_icarus-server"))
            .args(["--home", home.path().to_str().expect("utf8"), "serve"])
            .env("ICARUS_WORKSPACE_ROOT", home.path().join("workspaces"))
            .env("XDG_CONFIG_HOME", config.path())
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
        let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
        let project = client
            .request("project.add", json!({ "path": repo, "name": "widgets" }))
            .await
            .expect("project");
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
        Self {
            project: project["id"].as_str().expect("id").to_owned(),
            home,
            _config: config,
            _repos: repos,
            fixtures: fixtures_path,
            _server: ServerProcess { child },
            client,
        }
    }

    fn set_fixtures(&self, fixtures: Value) {
        std::fs::write(&self.fixtures, fixtures.to_string()).expect("fixtures");
    }

    fn count(&self, route: &str) -> usize {
        let log = PathBuf::from(format!("{}.requests.jsonl", self.fixtures.display()));
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|r| r["route"] == route)
            .count()
    }

    async fn app(&self) -> (Arc<Client>, UnboundedReceiver<Event>) {
        Client::connect(self.home.path(), "icarus-app")
            .await
            .expect("app")
    }

    async fn request(&self, method: &str, params: Value) -> Value {
        self.client
            .request(method, params)
            .await
            .unwrap_or_else(|e| panic!("{method}: {e:#}"))
    }

    async fn workspace(&self, name: &str) -> (String, PathBuf) {
        let ws = self
            .request(
                "workspace.create",
                json!({ "project": self.project, "name": name, "agent": false }),
            )
            .await;
        (
            ws["id"].as_str().expect("id").to_owned(),
            PathBuf::from(ws["path"].as_str().expect("path")),
        )
    }
}

async fn next_event(events: &mut UnboundedReceiver<Event>, topic: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = tokio::time::timeout(remaining, events.recv())
            .await
            .unwrap_or_else(|_| panic!("no {topic} event"))
            .expect("event stream open");
        if event.topic == topic {
            return event.payload;
        }
    }
}

fn pull(number: u64, branch: &str, extra: Value) -> Value {
    let mut pull = json!({
        "id": format!("PR_{number}"),
        "number": number,
        "title": format!("Pull request {number}"),
        "url": format!("https://github.com/acme/widgets/pull/{number}"),
        "state": "OPEN",
        "isDraft": false,
        "updatedAt": "2026-11-15T12:00:00Z",
        "headRefName": branch,
        "baseRefName": "main",
        "headRefOid": HEAD,
        "headRef": { "name": branch },
        "baseRef": { "name": "main" },
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "UNSTABLE",
        "author": { "login": "me" },
        "repository": { "name": "widgets", "owner": { "login": "acme" }, "autoMergeAllowed": true },
        "commits": { "nodes": [ { "commit": { "statusCheckRollup": { "state": "FAILURE", "contexts": { "nodes": [
            { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "FAILURE",
              "detailsUrl": "https://github.com/acme/widgets/actions/runs/77/job/1",
              "startedAt": "2026-11-15T11:50:00Z", "completedAt": "2026-11-15T11:55:00Z" },
            { "__typename": "StatusContext", "context": "ci/legacy", "state": "PENDING",
              "targetUrl": "https://ci.example.com/9", "createdAt": "2026-11-15T11:51:00Z" }
        ] } } } } ] },
    });
    for (key, value) in extra.as_object().into_iter().flatten() {
        pull[key] = value.clone();
    }
    pull
}

fn threads() -> Value {
    json!([
        { "id": "PRRT_1", "isResolved": false, "isOutdated": false, "path": "src/a.rs", "line": 4,
          "comments": { "nodes": [
            { "id": "C1", "body": "Off by one here.", "url": "https://github.com/acme/widgets/pull/5#discussion_r1",
              "author": { "__typename": "User", "login": "monalisa" } },
            { "id": "C2", "body": "LGTM after fix", "url": "https://github.com/acme/widgets/pull/5#discussion_r2",
              "author": { "__typename": "Bot", "login": "coderabbitai" } } ] } },
        { "id": "PRRT_2", "isResolved": true, "isOutdated": false, "path": "src/b.rs", "line": 9,
          "comments": { "nodes": [
            { "id": "C3", "body": "Rename.", "url": "https://github.com/acme/widgets/pull/5#discussion_r3",
              "author": { "__typename": "User", "login": "hubot" } } ] } },
        { "id": "PRRT_3", "isResolved": false, "isOutdated": false, "path": "src/c.rs", "line": 1,
          "comments": { "nodes": [
            { "id": "C4", "body": "Bot nit.", "url": "https://github.com/acme/widgets/pull/5#discussion_r4",
              "author": { "__typename": "Bot", "login": "coderabbitai" } } ] } }
    ])
}

#[tokio::test]
async fn added_attachments_reach_the_app_and_read_back_as_context_blocks() {
    let world = World::new(json!({ "viewer": "me" })).await;
    let (id, path) = world.workspace("ctx").await;

    let offline = world
        .request(
            "attachment.add",
            json!({ "workspace": id, "kind": "text", "title": "Offline", "text": "kept" }),
        )
        .await;
    assert_eq!(offline["delivered"], 0, "no app connected: stored only");

    let (_app, mut app_events) = world.app().await;
    let added = world
        .request(
            "attachment.add",
            json!({ "workspace": id, "kind": "pr-comment", "title": "@monalisa on a.rs:3",
                    "text": "Off by one.", "source": { "url": "https://github.com/acme/widgets/pull/5#r1" } }),
        )
        .await;
    assert_eq!(added["delivered"], 1);
    let attachment_id = added["id"].as_str().expect("id").to_owned();
    let event = next_event(&mut app_events, "composer.attach").await;
    assert_eq!(
        event,
        json!({
            "workspace_path": path,
            "attachment": { "id": attachment_id, "kind": "pr-comment", "title": "@monalisa on a.rs:3" },
        })
    );

    let block = world
        .request("attachment.get", json!({ "id": attachment_id }))
        .await;
    assert_eq!(
        block["text"],
        "<attachment kind=\"pr-comment\" title=\"@monalisa on a.rs:3\" source=\"https://github.com/acme/widgets/pull/5#r1\">\nOff by one.\n</attachment>"
    );

    let file = world
        .request(
            "attach.file",
            json!({ "workspace": id, "path": "readme.md", "start_line": 2, "end_line": 3 }),
        )
        .await;
    assert_eq!(file["title"], "readme.md:2-3");
    assert_eq!(file["text"], "two\nthree");
    let note = world
        .request(
            "attach.text",
            json!({ "workspace": id, "title": "Plan", "text": "Do it" }),
        )
        .await;
    assert_eq!(note["kind"], "text");

    let listed = world
        .request("attachment.list", json!({ "workspace": id }))
        .await;
    assert_eq!(listed.as_array().map(Vec::len), Some(4));
    world
        .request(
            "attachment.mark_sent",
            json!({ "ids": [offline["id"], note["id"]] }),
        )
        .await;
    world
        .request("attachment.remove", json!({ "id": file["id"] }))
        .await;
    let pending = world
        .request("attachment.list", json!({ "workspace": id }))
        .await;
    let ids: Vec<&Value> = pending
        .as_array()
        .expect("list")
        .iter()
        .map(|a| &a["id"])
        .collect();
    assert_eq!(ids, vec![&json!(attachment_id)]);
    let all = world
        .request(
            "attachment.list",
            json!({ "workspace": id, "include_sent": true }),
        )
        .await;
    assert_eq!(all.as_array().map(Vec::len), Some(3));
    assert!(all[0]["sent_at"].is_u64());
}

#[tokio::test]
async fn pr_comment_and_check_builders_use_github_data() {
    let world = World::new(json!({ "viewer": "me" })).await;
    let (id, path) = world.workspace("feat-a").await;
    let long_log = format!("{}\nerror: assertion failed\n", "noise line\n".repeat(2000));
    world.set_fixtures(json!({
        "viewer": "me",
        "pulls": [pull(5, "feat-a", json!({}))],
        "review_threads": { "5": threads() },
        "run_logs": { "77": long_log },
    }));
    let (_app, mut app_events) = world.app().await;

    let unresolved = world
        .request(
            "attach.pr_comments",
            json!({ "workspace": id, "unresolved": true, "humans_only": true }),
        )
        .await;
    let attachments = unresolved["attachments"].as_array().expect("attachments");
    assert_eq!(attachments.len(), 1, "{unresolved}");
    assert_eq!(unresolved["delivered"], 1);
    let comment = &attachments[0];
    assert_eq!(comment["kind"], "pr-comment");
    assert_eq!(comment["title"], "@monalisa on src/a.rs:4");
    assert!(!comment["text"].as_str().expect("text").contains("LGTM"));
    assert_eq!(comment["source"]["thread_id"], "PRRT_1");
    let event = next_event(&mut app_events, "composer.attach").await;
    assert_eq!(event["workspace_path"], json!(path));
    assert_eq!(event["attachment"]["id"], comment["id"]);
    let block = world
        .request("attachment.get", json!({ "id": comment["id"] }))
        .await;
    let block = block["text"].as_str().expect("block");
    assert!(block.starts_with("<attachment kind=\"pr-comment\" title=\"@monalisa on src/a.rs:4\" source=\"https://github.com/acme/widgets/pull/5#discussion_r1\">"));
    assert!(block.contains("@monalisa:\nOff by one here."));

    let by_id = world
        .request(
            "attach.pr_comments",
            json!({ "workspace": id, "ids": ["C3"] }),
        )
        .await;
    assert_eq!(by_id["attachments"].as_array().map(Vec::len), Some(1));
    assert_eq!(by_id["attachments"][0]["source"]["is_resolved"], true);
    let everything = world
        .request("attach.pr_comments", json!({ "workspace": id }))
        .await;
    assert_eq!(everything["attachments"].as_array().map(Vec::len), Some(3));

    let check = world
        .request("attach.check", json!({ "workspace": id, "name": "build" }))
        .await;
    assert_eq!(check["kind"], "check");
    assert_eq!(check["title"], "build (failure)");
    assert_eq!(check["source"]["run_id"], 77);
    let text = check["text"].as_str().expect("text");
    assert!(text.contains("Conclusion: failure"), "{text}");
    assert!(text.contains("Details: https://github.com/acme/widgets/actions/runs/77/job/1"));
    assert!(text.contains("Started: 2026-11-15T11:50:00Z"));
    assert!(text.trim_end().ends_with("error: assertion failed"));
    assert!(text.contains("truncated"));
    assert!(
        text.len() < 9 * 1024,
        "log excerpt is capped: {}",
        text.len()
    );
    assert_eq!(world.count("run view 77 --log-failed"), 1);

    let missing = world
        .client
        .request("attach.check", json!({ "workspace": id, "name": "nope" }))
        .await;
    assert!(missing.is_err());
}

#[tokio::test]
async fn turn_attachments_carry_the_turn_diff() {
    let world = World::new(json!({ "viewer": "me" })).await;
    let (id, path) = world.workspace("turny").await;
    let start = icarus_git::turns::start_turn(&path).expect("start");
    std::fs::write(path.join("readme.md"), "one\ntwo\nTHREE\nfour\n").expect("edit");
    icarus_git::turns::finish_turn(
        &path,
        &start,
        &icarus_git::turns::TurnMeta {
            prompt: "Shout the third line".into(),
            agent_id: "agent-1".into(),
            provider: None,
            started_at: 1,
            completed_at: 2,
        },
    )
    .expect("finish")
    .expect("turn");

    let turn = world
        .request("attach.turn", json!({ "workspace": id, "turn": "latest" }))
        .await;
    assert_eq!(turn["kind"], "turn");
    assert_eq!(turn["title"], "Turn: Shout the third line");
    let text = turn["text"].as_str().expect("text");
    assert!(text.contains("-three\n+THREE"), "{text}");
    let block = world
        .request("attachment.get", json!({ "id": turn["id"] }))
        .await;
    assert!(
        block["text"]
            .as_str()
            .is_some_and(|b| b.contains("source=\"refs/worktree/icarus-turns/"))
    );
}

#[tokio::test]
async fn pr_status_sends_check_times_updated_at_and_rich_stack_entries() {
    let world = World::new(json!({ "viewer": "me" })).await;
    let (id, _) = world.workspace("feat-s").await;
    let entry = |position: u64, number: u64, title: &str, branch: &str, extra: Value| {
        let mut pr = json!({
            "number": number, "title": title, "url": format!("https://github.com/acme/widgets/pull/{number}"),
            "headRefName": branch, "state": "OPEN", "isDraft": false, "headRefOid": HEAD,
            "mergeable": "MERGEABLE", "reviewDecision": null, "mergeStateStatus": "CLEAN",
            "isInMergeQueue": false,
        });
        for (key, value) in extra.as_object().into_iter().flatten() {
            pr[key] = value.clone();
        }
        json!({ "position": position, "pullRequest": pr })
    };
    world.set_fixtures(json!({
        "viewer": "me",
        "pulls": [pull(5, "feat-s", json!({}))],
        "stacks": { "5": { "head": { "sha": HEAD }, "stack": {
            "number": 9, "position": 2, "size": 2, "base": { "ref": "main", "sha": BASE_SHA } } } },
        "routes": { "graphql StackDetails": { "body": { "data": { "repository": {
            "baseTip": { "target": { "oid": BASE_SHA } },
            "pullRequest": { "stack": { "number": 9, "size": 2, "baseRefName": "main", "entries": { "nodes": [
                entry(1, 4, "Base work", "feat-base", json!({ "isDraft": true, "isInMergeQueue": true,
                    "commits": { "nodes": [ { "commit": { "statusCheckRollup": { "state": "SUCCESS" } } } ] } })),
                entry(2, 5, "Pull request 5", "feat-s", json!({ "mergeStateStatus": "UNSTABLE",
                    "commits": { "nodes": [ { "commit": { "statusCheckRollup": { "state": "FAILURE" } } } ] } })),
            ] } } } } } } } },
    }));

    let status = world.request("pr.status", json!({ "workspace": id })).await;
    let pr = &status["pull_request"];
    assert_eq!(pr["updated_at"], "2026-11-15T12:00:00Z");
    let build = &pr["checks"][0];
    assert_eq!(build["started_at"], "2026-11-15T11:50:00Z");
    assert_eq!(build["completed_at"], "2026-11-15T11:55:00Z");
    assert_eq!(pr["checks"][1]["started_at"], "2026-11-15T11:51:00Z");
    assert!(pr["checks"][1]["completed_at"].is_null());

    let stack = &status["stack"];
    assert_eq!(stack["number"], 9);
    let entries = stack["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 2, "{stack}");
    let base = &entries[0];
    assert_eq!(base["title"], "Base work");
    assert_eq!(base["head_ref_name"], "feat-base");
    assert_eq!(base["state"], "draft");
    assert_eq!(base["is_draft"], true);
    assert_eq!(base["is_in_merge_queue"], true);
    assert_eq!(base["in_merge_queue"], true);
    assert_eq!(base["checks_rollup"], "success");
    assert_eq!(base["mergeable"], "MERGEABLE");
    assert_eq!(entries[1]["merge_state_status"], "UNSTABLE");
    assert_eq!(entries[1]["checks_rollup"], "failure");
    assert_eq!(entries[1]["checks_status"], "failure");

    world.request("pr.status", json!({ "workspace": id })).await;
    assert_eq!(world.count("graphql PrSnapshot"), 1, "cached within 15 s");
    world
        .request("pr.status", json!({ "workspace": id, "refresh": true }))
        .await;
    assert_eq!(
        world.count("graphql PrSnapshot"),
        2,
        "refresh bypasses the cache"
    );
    assert_eq!(world.count("graphql StackDetails"), 2);
}
