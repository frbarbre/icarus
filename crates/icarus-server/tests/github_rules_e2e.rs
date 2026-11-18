use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use icarus_client::Client;
use serde_json::{Value, json};
use tempfile::TempDir;

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

/// A server with fixture GitHub, an isolated settings directory, and a project whose remote is github.com/acme/widgets.
struct World {
    home: TempDir,
    _config: TempDir,
    _repos: TempDir,
    repo: PathBuf,
    fixtures: PathBuf,
    settings: PathBuf,
    _server: ServerProcess,
    client: Arc<Client>,
    events: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<icarus_client::Event>>,
    project: String,
}

impl World {
    async fn new(settings: Value, fixtures: Value) -> Self {
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

        let settings_path = config.path().join("icarus").join("icarus.json");
        std::fs::create_dir_all(settings_path.parent().expect("parent")).expect("settings dir");
        std::fs::write(&settings_path, settings.to_string()).expect("settings");
        let fixtures_path = home.path().join("gh-fixtures.json");
        std::fs::write(&fixtures_path, fixtures.to_string()).expect("fixtures");

        let _ = std::fs::remove_file(home.path().join("server.token"));
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
        let (client, events) = Client::connect(home.path(), "test").await.expect("connect");
        let project = client
            .request("project.add", json!({ "path": repo, "name": "widgets" }))
            .await
            .expect("project");
        assert_eq!(project["remote"], "github.com/acme/widgets");
        // Why after adding: the project keeps its GitHub identity while fetches go to a local bare repo.
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
            repo,
            fixtures: fixtures_path,
            settings: settings_path,
            _server: ServerProcess { child },
            client,
            events: tokio::sync::Mutex::new(events),
        }
    }

    /// The first `workspace.next_action` event for `workspace` whose step is `kind`.
    async fn next_action_event(&self, workspace: &str, kind: &str, within: Duration) -> Value {
        let mut events = self.events.lock().await;
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let event = tokio::time::timeout_at(deadline, events.recv())
                .await
                .unwrap_or_else(|_| panic!("no {kind} next action for {workspace} in {within:?}"))
                .expect("events");
            if event.topic == "workspace.next_action"
                && event.payload["workspace"] == workspace
                && event.payload["action"]["kind"] == kind
            {
                return event.payload;
            }
        }
    }

    fn snapshot_queries(&self) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|r| r["route"] == "graphql PrSnapshot")
            .map(|r| r["variables"].clone())
            .collect()
    }

    fn set_fixtures(&self, fixtures: Value) {
        std::fs::write(&self.fixtures, fixtures.to_string()).expect("fixtures");
    }

    fn set_settings(&self, settings: Value) {
        std::fs::write(&self.settings, settings.to_string()).expect("settings");
    }

    fn requests(&self) -> Vec<Value> {
        let log = PathBuf::from(format!("{}.requests.jsonl", self.fixtures.display()));
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn count(&self, route: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r["route"] == route)
            .count()
    }

    async fn request(&self, method: &str, params: Value) -> Value {
        self.client
            .request(method, params)
            .await
            .unwrap_or_else(|e| panic!("{method}: {e:#}"))
    }

    async fn workspace(&self, name: &str) -> (String, PathBuf, String) {
        let ws = self
            .request(
                "workspace.create",
                json!({ "project": self.project, "name": name, "agent": false }),
            )
            .await;
        let path = PathBuf::from(ws["path"].as_str().expect("path"));
        let head = git(&path, &["rev-parse", "HEAD"]);
        (ws["id"].as_str().expect("id").to_owned(), path, head)
    }

    async fn next_action(&self, id: &str) -> Value {
        self.request("workspace.next_action", json!({ "id": id }))
            .await
    }

    /// Every workspace but the repository's main checkout, which is always listed.
    async fn workspaces(&self) -> Vec<Value> {
        self.request("workspace.list", json!({}))
            .await
            .as_array()
            .into_iter()
            .flatten()
            .filter(|workspace| workspace["primary"] != true)
            .cloned()
            .collect()
    }
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

fn fake_agent_settings(extra: Value) -> Value {
    let mut settings = json!({
        "agents": { "commands": { "claude": [env!("CARGO_BIN_EXE_icarus-fake-agent")] } },
    });
    for (key, value) in extra.as_object().into_iter().flatten() {
        settings[key] = value.clone();
    }
    settings
}

async fn wait_for_agent(world: &World, id: &str) -> String {
    world
        .request("agent.wait", json!({ "id": id, "timeout_ms": 10000 }))
        .await;
    world.request("agent.read", json!({ "id": id })).await["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn pr_status_maps_the_branch_pull_request_in_one_batched_snapshot() {
    let world = World::new(json!({}), json!({ "viewer": "me" })).await;
    let (with_pr, _, head) = world.workspace("feat-a").await;
    let (without_pr, _, _) = world.workspace("feat-b").await;
    world.set_fixtures(json!({
        "viewer": "me",
        "pulls": [pull(5, "feat-a", &head, json!({
            "isDraft": true,
            "reviewDecision": "REVIEW_REQUIRED",
            "author": { "login": "me", "avatarUrl": "https://avatars.example/me" },
            "reviewRequests": { "nodes": [{ "asCodeOwner": false, "requestedReviewer":
                { "__typename": "User", "login": "rev", "avatarUrl": "https://avatars.example/rev" } }] },
        }))],
    }));
    assert!(
        world
            .workspaces()
            .await
            .iter()
            .all(|w| w["pr_author"].is_null())
    );
    assert_eq!(
        world.count("graphql PrSnapshot"),
        0,
        "listing never asks GitHub"
    );

    let status = world
        .request("pr.status", json!({ "workspace": with_pr }))
        .await;
    assert_eq!(status["known"], true);
    let pr = &status["pull_request"];
    assert_eq!(pr["number"], 5);
    assert_eq!(pr["author_avatar_url"], "https://avatars.example/me");
    assert_eq!(
        pr["requested_reviewers"],
        json!([{ "kind": "user", "login": "rev", "avatar_url": "https://avatars.example/rev" }])
    );
    let listed = world.workspaces().await;
    let author_of = |id: &str| {
        listed
            .iter()
            .find(|w| w["id"] == id)
            .map(|w| w["pr_author"].clone())
    };
    assert_eq!(
        author_of(&with_pr),
        Some(json!({ "login": "me", "avatar_url": "https://avatars.example/me" }))
    );
    assert_eq!(author_of(&without_pr), Some(Value::Null));
    let info = world
        .request("workspace.info", json!({ "id": with_pr }))
        .await;
    assert_eq!(info["pr_author"]["login"], "me");
    assert_eq!(
        info["pr"]["number"], 5,
        "cards get the PR summary from the cache"
    );
    assert_eq!(info["pr"]["state"], "draft");
    assert_eq!(
        info["pr"]["title"], "Pull request 5",
        "the sidebar filter matches it"
    );
    assert!(info["pr"]["additions"].is_u64());
    let summary_of = |id: &str| {
        listed
            .iter()
            .find(|w| w["id"] == id)
            .map(|w| w["pr"].clone())
    };
    assert_eq!(summary_of(&without_pr), Some(Value::Null));
    assert_eq!(pr["is_draft"], true);
    assert_eq!(pr["head_ref_name"], "feat-a");
    assert_eq!(pr["head_ref_oid"], head.as_str());
    assert_eq!(pr["review_decision"], "REVIEW_REQUIRED");
    assert_eq!(pr["repo"], json!({ "owner": "acme", "name": "widgets" }));
    assert_eq!(pr["merge_queue_required"], false);
    assert!(status["stack"].is_null());
    let methods = status["merge_metadata"]["allowed_methods"]
        .as_array()
        .expect("methods");
    assert!(methods.contains(&json!("SQUASH")), "{status}");

    let none = world
        .request("pr.status", json!({ "workspace": without_pr }))
        .await;
    assert_eq!(none["known"], true);
    assert!(none["pull_request"].is_null());
    assert_eq!(
        world.count("graphql PrSnapshot"),
        1,
        "both branches come from one cached snapshot: {:?}",
        world
            .requests()
            .iter()
            .filter(|r| r["route"] == "graphql PrSnapshot")
            .collect::<Vec<_>>()
    );

    let restarted = World::new(json!({}), json!({ "routes": { "graphql PrSnapshot": { "status": 502, "body": { "message": "down" } } } })).await;
    let (id, _, _) = restarted.workspace("feat-c").await;
    let unknown = restarted
        .request("pr.status", json!({ "workspace": id }))
        .await;
    assert_eq!(unknown["known"], false, "a failed lookup is not 'no PR'");
    let action = restarted.next_action(&id).await;
    assert!(
        action["action"].is_null(),
        "no Create PR before the lookup answers: {action}"
    );
}

#[tokio::test]
async fn pr_previews_come_from_deployments_and_ride_along_in_pr_status() {
    let world = World::new(json!({}), json!({ "viewer": "me" })).await;
    let (id, _, head) = world.workspace("feat-previews").await;
    let deployment = |environment: &str, url: &str| {
        json!({ "environment": environment, "state": "ACTIVE", "createdAt": "2026-11-15T13:17:00Z",
            "commitOid": head, "latestStatus": { "state": "SUCCESS", "environmentUrl": url } })
    };
    world.set_fixtures(json!({
        "viewer": "me",
        "pulls": [pull(9, "feat-previews", &head, json!({}))],
        "previews": { "9": {
            "headRefOid": head, "headRefName": "feat-previews", "isCrossRepository": false,
            "comments": { "nodes": [] },
            "commits": { "nodes": [{ "commit": { "oid": head,
                "deployments": { "nodes": [
                    deployment("frontend-preview", "https://feat.example.pages.dev"),
                    deployment("insights-preview", "https://feat.insights.pages.dev"),
                    deployment("production", "https://example.com"),
                ] } } }] },
        } },
    }));

    let found = world
        .request("pr.previews", json!({ "workspace": id }))
        .await;
    assert_eq!(found["number"], 9);
    let apps: Vec<&str> = found["previews"]
        .as_array()
        .expect("previews")
        .iter()
        .filter_map(|p| p["app"].as_str())
        .collect();
    assert_eq!(apps, ["frontend", "insights"]);
    assert_eq!(found["primary"]["url"], "https://feat.example.pages.dev");
    assert_eq!(found["primary"]["source"], "deployment");
    assert_eq!(found["primary"]["is_head"], true);

    let status = world.request("pr.status", json!({ "workspace": id })).await;
    assert_eq!(status["previews"], found["previews"]);
    assert_eq!(status["primary_preview"], found["primary"]);
    assert_eq!(
        world.count("graphql PrPreviews"),
        1,
        "pr.status reuses the cached previews"
    );

    world.set_settings(json!({ "previews": { "primary_app": "insights" } }));
    let found = world
        .request("pr.previews", json!({ "workspace": id, "refresh": true }))
        .await;
    assert_eq!(found["primary"]["app"], "insights");
}

#[tokio::test]
async fn next_action_resolves_each_step() {
    let world = World::new(json!({}), json!({ "viewer": "me" })).await;
    let (no_pr, _, _) = world.workspace("no-pr").await;
    let (dirty, dirty_path, dirty_head) = world.workspace("dirty").await;
    let (draft, _, draft_head) = world.workspace("draft").await;
    let (conflict, _, conflict_head) = world.workspace("conflict").await;
    let (queue, _, queue_head) = world.workspace("queue").await;
    std::fs::write(dirty_path.join("wip.txt"), "wip\n").expect("dirty");
    world.set_fixtures(json!({
        "viewer": "me",
        "repo": { "squashMergeAllowed": true, "autoMergeAllowed": true, "mergeQueue": { "id": "MQ_1" } },
        "pulls": [
            pull(2, "dirty", &dirty_head, json!({})),
            pull(3, "draft", &draft_head, json!({ "isDraft": true })),
            pull(4, "conflict", &conflict_head, json!({ "mergeable": "CONFLICTING", "mergeStateStatus": "DIRTY" })),
            pull(6, "queue", &queue_head, json!({})),
        ],
    }));

    let expect = |value: &Value, kind: &str, label: &str, agent: bool| {
        assert_eq!(value["action"]["kind"], kind, "{value}");
        assert_eq!(value["action"]["label"], label, "{value}");
        assert_eq!(value["action"]["agent_step"], agent, "{value}");
    };
    let none = world.next_action(&no_pr).await;
    expect(&none, "create-pull-request", "Create PR", true);
    assert_eq!(none["local"]["branch"], "no-pr");
    assert_eq!(none["local"]["has_uncommitted_changes"], false);
    let dirty = world.next_action(&dirty).await;
    expect(&dirty, "commit-and-push", "Commit & push", true);
    assert_eq!(dirty["local"]["has_uncommitted_changes"], true);
    expect(
        &world.next_action(&draft).await,
        "ready-for-review",
        "Ready for review",
        true,
    );
    expect(
        &world.next_action(&conflict).await,
        "resolve-conflicts",
        "Resolve conflicts",
        true,
    );
    expect(
        &world.next_action(&queue).await,
        "add-to-merge-queue",
        "Add to merge queue",
        false,
    );
}

#[tokio::test]
async fn run_next_action_starts_an_agent_or_calls_github() {
    let world = World::new(fake_agent_settings(json!({})), json!({ "viewer": "me" })).await;
    let (draft, _, draft_head) = world.workspace("draft").await;
    let (ready, _, ready_head) = world.workspace("ready").await;
    world.set_fixtures(json!({
        "viewer": "me",
        "pulls": [
            pull(3, "draft", &draft_head, json!({ "isDraft": true })),
            pull(8, "ready", &ready_head, json!({})),
        ],
    }));

    let ran = world
        .request("workspace.run_next_action", json!({ "id": draft }))
        .await;
    assert_eq!(ran["action"]["kind"], "ready-for-review");
    assert!(ran["github"].is_null());
    let agent = ran["started_agent"].as_str().expect("agent id").to_owned();
    let text = wait_for_agent(&world, &agent).await;
    assert!(
        text.contains(
            "echo: Get pull request #3 (https://github.com/acme/widgets/pull/3) ready for review"
        ),
        "{text}"
    );
    let info = world.request("agent.info", json!({ "id": agent })).await;
    assert_eq!(info["title"], "Ready for review");
    assert_eq!(info["workspace"], draft.as_str());
    let again = world
        .request("workspace.run_next_action", json!({ "id": draft }))
        .await;
    assert_ne!(
        again["started_agent"], agent,
        "every run starts a new session"
    );

    assert_eq!(world.next_action(&ready).await["action"]["kind"], "merge");
    let merged = world
        .request("workspace.run_next_action", json!({ "id": ready }))
        .await;
    assert!(merged["started_agent"].is_null());
    assert_eq!(merged["github"]["merged"], true, "{merged}");
    let request = world
        .requests()
        .into_iter()
        .find(|r| r["route"] == "PUT repos/acme/widgets/pulls/8/merge")
        .expect("merge request");
    assert_eq!(
        request["body"]["sha"],
        ready_head.as_str(),
        "merge is pinned to the head"
    );
    assert_eq!(
        request["body"]["merge_method"], "merge",
        "the repo's first allowed method"
    );
}

#[tokio::test]
async fn a_push_updates_the_next_action_without_waiting_for_the_poll() {
    let world = World::new(json!({}), json!({ "viewer": "me" })).await;
    let (feat, feat_path, _) = world.workspace("feat").await;
    world.workspace("other").await;
    let commit = |name: &str| {
        std::fs::write(feat_path.join(name), "change\n").expect("write");
        git(&feat_path, &["add", "."]);
        git(&feat_path, &["commit", "-q", "-m", name]);
        git(&feat_path, &["rev-parse", "HEAD"])
    };
    let head = commit("one.txt");
    assert_ne!(world.next_action(&feat).await["action"]["kind"], "merge");
    world.set_fixtures(json!({ "viewer": "me", "pulls": [pull(9, "feat", &head, json!({}))] }));

    let pushed = Instant::now();
    world
        .request("git.push", json!({ "workspace": feat }))
        .await;
    let event = world
        .next_action_event(&feat, "merge", Duration::from_secs(6))
        .await;
    let took = pushed.elapsed();
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(5),
        "after GitHub's post-push delay, not the 60 s poll: {took:?}"
    );
    assert_eq!(event["local"]["unpushed_commits"], 0);
    let targeted = world
        .snapshot_queries()
        .into_iter()
        .filter(|vars| vars["b0"] == "feat" && vars.get("b1").is_none())
        .count();
    assert!(
        targeted >= 1,
        "one single-PR query: {:?}",
        world.snapshot_queries()
    );

    let status = world
        .request("pr.status", json!({ "workspace": feat }))
        .await;
    let button = world.next_action(&feat).await;
    assert_eq!(status["pull_request"]["number"], 9, "{status}");
    assert_eq!(
        button["action"]["kind"], "merge",
        "the panel and the button read one copy"
    );

    // A push the server didn't run (an agent's, a terminal's) shows up the next time it reads git.
    let head = commit("two.txt");
    world.set_fixtures(json!({
        "viewer": "me",
        "pulls": [pull(9, "feat", &head, json!({ "isDraft": true }))],
    }));
    assert_eq!(
        world.next_action(&feat).await["action"]["kind"],
        "commit-and-push"
    );
    git(&feat_path, &["push", "-q"]);
    let pushed = Instant::now();
    world.next_action(&feat).await;
    world
        .next_action_event(&feat, "ready-for-review", Duration::from_secs(6))
        .await;
    assert!(pushed.elapsed() < Duration::from_secs(5));
}

fn rules_settings(armed: Option<bool>) -> Value {
    let mut rules = json!({
        "projects": ["widgets"],
        "pr": {
            "enabled": true,
            "statusByCondition": { "draft": "in-progress", "reviewing": "in-review" },
            "reviewInbox": { "enabled": true, "agent": "claude" },
        },
    });
    if let Some(armed) = armed {
        rules["armed"] = json!(armed);
    }
    fake_agent_settings(json!({ "rules": rules }))
}

/// A branch someone else pushed for review, present only on the origin.
fn push_review_branch(world: &World) -> String {
    git(&world.repo, &["checkout", "-q", "-b", "feature-review"]);
    std::fs::write(world.repo.join("review.md"), "review me\n").expect("write");
    git(&world.repo, &["add", "."]);
    git(&world.repo, &["commit", "-q", "-m", "review"]);
    let head = git(&world.repo, &["rev-parse", "HEAD"]);
    git(&world.repo, &["push", "-q", "origin", "feature-review"]);
    git(&world.repo, &["checkout", "-q", "main"]);
    git(&world.repo, &["branch", "-q", "-D", "feature-review"]);
    head
}

fn review_fixtures(own_head: &str, review_head: &str) -> Value {
    json!({
        "viewer": "me",
        "pulls": [pull(5, "feat-a", own_head, json!({ "isDraft": true }))],
        "inbox": [pull(7, "feature-review", review_head, json!({
            "title": "Add review flow", "author": { "login": "alice" },
        }))],
    })
}

#[tokio::test]
async fn automation_tick_moves_a_status_and_creates_a_review_workspace() {
    let world = World::new(rules_settings(Some(true)), json!({ "viewer": "me" })).await;
    let (own, _, own_head) = world.workspace("feat-a").await;
    let review_head = push_review_branch(&world);
    world.set_fixtures(review_fixtures(&own_head, &review_head));

    let tick = world.request("rules.tick", json!({})).await;
    assert_eq!(tick["dry_run"], false, "{tick}");
    assert_eq!(tick["errors"], json!([]), "{tick}");
    let info = world.request("workspace.info", json!({ "id": own })).await;
    assert_eq!(info["status"], "in-progress", "{tick}");

    let workspaces = world.workspaces().await;
    let review = workspaces
        .iter()
        .find(|w| w["name"] == "review-7-add-review-flow")
        .unwrap_or_else(|| panic!("review workspace: {tick}"));
    assert_eq!(review["status"], "in-review");
    assert_eq!(review["branch"], "feature-review");
    assert_eq!(review["base_ref"], "origin/main");
    let path = PathBuf::from(review["path"].as_str().expect("path"));
    assert!(path.starts_with(world.home.path().join("workspaces")));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), review_head);
    let agent = review["agents"][0].clone();
    assert_eq!(agent["title"], "Review #7");
    let text = wait_for_agent(&world, agent["id"].as_str().expect("agent")).await;
    assert!(
        text.contains("echo: You are reviewing pull request #7"),
        "{text}"
    );

    let second = world.request("rules.tick", json!({})).await;
    assert_eq!(second["errors"], json!([]), "{second}");
    assert_eq!(
        world.workspaces().await.len(),
        2,
        "the ledger keeps the PR from being cloned twice: {second}"
    );
    let status = world.request("rules.status", json!({})).await;
    assert_eq!(status["ticks"], 2);
    assert_eq!(status["armed"], true);
    assert_eq!(status["last_tick"]["dry_run"], false);
}

#[tokio::test]
async fn automation_moves_worktrees_made_outside_icarus_too() {
    let world = World::new(rules_settings(Some(true)), json!({ "viewer": "me" })).await;
    let outside = world.repo.parent().expect("repos").join("elsewhere-feat-b");
    git(
        &world.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat-b",
            outside.to_str().expect("utf8"),
        ],
    );
    let adopted = world
        .request("workspace.adopt", json!({ "path": outside }))
        .await;
    let id = adopted["id"].as_str().expect("id").to_owned();
    let head = git(&outside, &["rev-parse", "HEAD"]);
    world.set_fixtures(json!({
        "viewer": "me",
        "pulls": [pull(9, "feat-b", &head, json!({ "isDraft": true }))],
    }));

    let tick = world.request("rules.tick", json!({})).await;
    assert_eq!(tick["errors"], json!([]), "{tick}");
    let info = world.request("workspace.info", json!({ "id": id })).await;
    assert_eq!(
        info["status"], "in-progress",
        "a draft outside the workspace root moves too: {tick}"
    );
}

#[tokio::test]
async fn deleting_a_merged_workspace_runs_the_archive_script_and_a_failure_keeps_it() {
    let markers = tempfile::tempdir().expect("markers");
    let marker = markers.path().join("archived.txt");
    let mut settings = rules_settings(Some(true));
    settings["rules"]["pr"]["onResolved"] = json!("delete");
    settings["project"] = json!({ "hooks": { "scripts": { "archive": format!(
        "[ \"$ORCA_WORKSPACE_NAME\" = feat-keep ] && exit 1; echo \"$ORCA_WORKSPACE_NAME\" >> '{}'",
        marker.display()
    ) } } });
    let world = World::new(settings, json!({ "viewer": "me" })).await;
    let (gone, gone_path, gone_head) = world.workspace("feat-gone").await;
    let (kept, kept_path, kept_head) = world.workspace("feat-keep").await;
    world.set_fixtures(json!({
        "viewer": "me",
        "pulls": [
            pull(11, "feat-gone", &gone_head, json!({ "state": "MERGED" })),
            pull(12, "feat-keep", &kept_head, json!({ "state": "MERGED" })),
        ],
    }));

    let tick = world.request("rules.tick", json!({})).await;
    let ids: Vec<String> = world
        .workspaces()
        .await
        .iter()
        .filter_map(|w| w["id"].as_str().map(str::to_owned))
        .collect();
    assert!(!ids.contains(&gone), "{tick}");
    assert!(!gone_path.exists());
    assert_eq!(
        std::fs::read_to_string(&marker).expect("archive ran"),
        "feat-gone\n"
    );
    assert!(
        ids.contains(&kept),
        "an unforced delete keeps a workspace whose archive failed: {tick}"
    );
    assert!(kept_path.is_dir());
    assert!(
        tick["errors"].to_string().contains("archive")
            || tick.to_string().contains("archive_failed"),
        "{tick}"
    );
}

#[tokio::test]
async fn unarmed_ticks_are_dry_runs_that_execute_nothing() {
    let world = World::new(rules_settings(None), json!({ "viewer": "me" })).await;
    let (own, _, own_head) = world.workspace("feat-a").await;
    let review_head = push_review_branch(&world);
    world.set_fixtures(review_fixtures(&own_head, &review_head));

    let tick = world.request("rules.tick", json!({})).await;
    assert_eq!(tick["dry_run"], true);
    let planned: Vec<&str> = tick["planned"]
        .as_array()
        .expect("planned")
        .iter()
        .filter_map(|a| a["action"].as_str())
        .collect();
    assert!(planned.contains(&"set_column"), "{tick}");
    assert!(planned.contains(&"create_workspace_from_pr"), "{tick}");
    assert_eq!(tick["executed"], json!([]));
    let info = world.request("workspace.info", json!({ "id": own })).await;
    assert!(info["status"].is_null());
    assert_eq!(world.workspaces().await.len(), 1);
    assert_eq!(world.request("agent.list", json!({})).await, json!([]));

    let explicit = world
        .request("rules.tick", json!({ "dry_run": true }))
        .await;
    assert_eq!(explicit["dry_run"], true);
    assert_eq!(world.workspaces().await.len(), 1);

    let mut armed = rules_settings(Some(true));
    armed["rules"]["pr"]["enabled"] = json!(false);
    world.set_settings(armed);
    let disabled = world.request("rules.tick", json!({})).await;
    assert_eq!(disabled["skipped"], "rules are disabled");
    assert!(
        world
            .requests()
            .iter()
            .all(|r| !r["route"].as_str().unwrap_or_default().starts_with("PUT")),
        "a dry run makes no GitHub mutations"
    );
}

#[tokio::test]
#[ignore = "reads from the real GitHub through the local gh login"]
async fn live_gh_viewer_login() {
    let github = icarus_github::GitHub::new(icarus_github::GhCli::new());
    let login = github.viewer_login().await.expect("gh login");
    assert!(!login.is_empty());
}
