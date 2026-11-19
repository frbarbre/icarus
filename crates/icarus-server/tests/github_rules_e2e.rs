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
    _events: tokio::sync::mpsc::UnboundedReceiver<icarus_client::Event>,
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
            _events: events,
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

/// The template script's next step, after a manual run decides the buttons.
async fn scripted_step(world: &World, id: &str) -> Value {
    let run = run(world).await;
    assert!(run["error"].is_null(), "{run}");
    world.next_action(id).await
}

#[tokio::test]
async fn the_template_script_offers_each_next_step() {
    if !bun_installed() {
        eprintln!("skipped: no bun");
        return;
    }
    let scripts = tempfile::tempdir().expect("scripts");
    let script = flow_script(scripts.path());
    let world = World::new(flow_settings(&script, true), json!({ "viewer": "me" })).await;
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

    let expect = |value: &Value, id: &str, label: &str| {
        assert_eq!(value["action"]["kind"], format!("script:{id}"), "{value}");
        assert_eq!(value["action"]["label"], label, "{value}");
    };
    let none = scripted_step(&world, &no_pr).await;
    expect(&none, "create-pull-request", "Create PR");
    assert_eq!(none["local"]["branch"], "no-pr");
    let dirty = world.next_action(&dirty).await;
    expect(&dirty, "commit-and-push", "Commit & push");
    assert_eq!(dirty["local"]["has_uncommitted_changes"], true);
    expect(
        &world.next_action(&draft).await,
        "ready-for-review",
        "Ready for review",
    );
    expect(
        &world.next_action(&conflict).await,
        "resolve-conflicts",
        "Resolve conflicts",
    );
    expect(
        &world.next_action(&queue).await,
        "merge",
        "Add to merge queue",
    );
}

#[tokio::test]
async fn clicking_the_next_step_starts_an_agent_or_merges() {
    if !bun_installed() {
        eprintln!("skipped: no bun");
        return;
    }
    let scripts = tempfile::tempdir().expect("scripts");
    let script = flow_script(scripts.path());
    let world = World::new(flow_settings(&script, true), json!({ "viewer": "me" })).await;
    let (draft, _, draft_head) = world.workspace("draft").await;
    let (ready, _, ready_head) = world.workspace("ready").await;
    world.set_fixtures(json!({
        "viewer": "me",
        "pulls": [
            pull(3, "draft", &draft_head, json!({ "isDraft": true })),
            pull(8, "ready", &ready_head, json!({})),
        ],
    }));

    assert_eq!(
        scripted_step(&world, &draft).await["action"]["kind"],
        "script:ready-for-review"
    );
    let ran = world
        .request("workspace.run_next_action", json!({ "id": draft }))
        .await;
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
        "a used session is not reused"
    );

    assert_eq!(
        world.next_action(&ready).await["action"]["kind"],
        "script:merge"
    );
    let merged = world
        .request("workspace.run_next_action", json!({ "id": ready }))
        .await;
    assert!(merged["started_agent"].is_null(), "{merged}");
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
    let targeted = || {
        world
            .snapshot_queries()
            .into_iter()
            .filter(|vars| vars["b0"] == "feat" && vars.get("b1").is_none())
            .count()
    };
    let wait_for_query = |after: usize| async move {
        let deadline = Instant::now() + Duration::from_secs(6);
        while targeted() <= after {
            assert!(
                Instant::now() < deadline,
                "no single-PR query after the push"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let head = commit("one.txt");
    world.next_action(&feat).await;
    world.set_fixtures(json!({ "viewer": "me", "pulls": [pull(9, "feat", &head, json!({}))] }));

    let before = targeted();
    let pushed = Instant::now();
    world
        .request("git.push", json!({ "workspace": feat }))
        .await;
    wait_for_query(before).await;
    let took = pushed.elapsed();
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(5),
        "after GitHub's post-push delay, not the 60 s poll: {took:?}"
    );
    let status = world
        .request("pr.status", json!({ "workspace": feat }))
        .await;
    assert_eq!(status["pull_request"]["number"], 9, "{status}");

    // A push the server didn't run (an agent's, a terminal's) shows up the next time it reads git.
    commit("two.txt");
    world.next_action(&feat).await;
    let before = targeted();
    git(&feat_path, &["push", "-q"]);
    let pushed = Instant::now();
    world.next_action(&feat).await;
    wait_for_query(before).await;
    assert!(pushed.elapsed() < Duration::from_secs(5));
}

/// Mirrors the server's lookup closely enough to skip where no bun is installed.
fn bun_installed() -> bool {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let shim =
        |p: &Path| std::fs::read_link(p).is_ok_and(|t| t.file_name() == Some("mise".as_ref()));
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("bun"))
        .any(|p| p.is_file() && !shim(&p))
        || home.join(".bun/bin/bun").is_file()
        || std::fs::read_dir(home.join(".local/share/mise/installs/bun"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|v| v.path().join("bin/bun").is_file())
}

fn flow_script(dir: &Path) -> PathBuf {
    let mut options = icarus_settings::automation_template::orca_flow_defaults();
    options["statusByCondition"] = json!({ "draft": "in-progress", "reviewing": "in-review" });
    options["newWorkspaceStatus"] = Value::Null;
    options["agent"] = json!("claude");
    options["reviewInbox"]["enabled"] = json!(true);
    options["reviewInbox"]["prompt"] = json!("Review #{{prNumber}} by {{author}}");
    let path = dir.join("orca-flow.ts");
    std::fs::write(
        &path,
        icarus_settings::automation_template::orca_flow(&options),
    )
    .expect("script");
    path
}

async fn run(world: &World) -> Value {
    let out = world
        .request("automation.run", json!({ "project": world.project }))
        .await;
    out["runs"][0].clone()
}

fn flow_settings(script: &Path, armed: bool) -> Value {
    fake_agent_settings(json!({
        "automations": { "armed": armed, "scripts": { "widgets": script } },
    }))
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
async fn the_orca_flow_script_moves_a_status_and_creates_a_review_workspace() {
    if !bun_installed() {
        eprintln!("skipped: no bun");
        return;
    }
    let scripts = tempfile::tempdir().expect("scripts");
    let script = flow_script(scripts.path());
    let world = World::new(flow_settings(&script, false), json!({ "viewer": "me" })).await;
    let (own, _, own_head) = world.workspace("feat-a").await;
    let review_head = push_review_branch(&world);
    world.set_fixtures(review_fixtures(&own_head, &review_head));

    let dry = run(&world).await;
    assert_eq!(dry["dry_run"], true, "{dry}");
    let info = world.request("workspace.info", json!({ "id": own })).await;
    assert!(info["status"].is_null(), "{dry}");
    assert_eq!(world.workspaces().await.len(), 1, "{dry}");

    world.set_settings(flow_settings(&script, true));
    let armed = run(&world).await;
    assert_eq!(armed["dry_run"], false, "{armed}");
    let info = world.request("workspace.info", json!({ "id": own })).await;
    assert_eq!(info["status"], "in-progress", "{armed}");

    let workspaces = world.workspaces().await;
    let review = workspaces
        .iter()
        .find(|w| w["branch"] == "feature-review")
        .unwrap_or_else(|| panic!("review workspace: {armed}"));
    assert_eq!(review["status"], "in-review");
    assert_eq!(review["branch"], "feature-review");
    assert_eq!(review["base_ref"], "origin/main");
    let path = PathBuf::from(review["path"].as_str().expect("path"));
    assert!(path.starts_with(world.home.path().join("workspaces")));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), review_head);
    let agent = review["agents"][0].clone();
    assert_eq!(agent["title"], "Review #7");
    let text = wait_for_agent(&world, agent["id"].as_str().expect("agent")).await;
    assert!(text.contains("echo: Review #7 by alice"), "{text}");

    let second = run(&world).await;
    assert_eq!(
        world.workspaces().await.len(),
        2,
        "memory keeps the PR from being cloned twice: {second}"
    );
}

fn write_flow(path: &Path, status_by_condition: Value) {
    let mut options = icarus_settings::automation_template::orca_flow_defaults();
    options["statusByCondition"] = status_by_condition;
    options["newWorkspaceStatus"] = Value::Null;
    std::fs::write(
        path,
        icarus_settings::automation_template::orca_flow(&options),
    )
    .expect("script");
}

fn changes_requested_by(login: &str) -> Value {
    json!({ "latestReviews": { "nodes": [
        { "state": "CHANGES_REQUESTED", "author": { "login": login } },
    ] } })
}

#[tokio::test]
async fn remapped_conditions_move_cards_between_columns() {
    if !bun_installed() {
        eprintln!("skipped: no bun");
        return;
    }
    let scripts = tempfile::tempdir().expect("scripts");
    let script = scripts.path().join("orca-flow.ts");
    write_flow(
        &script,
        json!({ "draft": "in-progress", "changes-requested": "in-progress", "review": "in-review", "merged": "done" }),
    );
    let world = World::new(flow_settings(&script, true), json!({ "viewer": "me" })).await;
    let (draft, _, draft_head) = world.workspace("feat-draft").await;
    let (review, _, review_head) = world.workspace("feat-review").await;
    let (changes, _, changes_head) = world.workspace("feat-changes").await;
    let status = async |id: &str| {
        world.request("workspace.info", json!({ "id": id })).await["status"].clone()
    };
    let github_changes = async |draft_extra: Value, review_extra: Value| {
        world.set_fixtures(json!({ "viewer": "me", "pulls": [
            pull(1, "feat-draft", &draft_head, draft_extra),
            pull(2, "feat-review", &review_head, review_extra),
            pull(3, "feat-changes", &changes_head, changes_requested_by("bob")),
        ] }));
        // Stands in for the 60 s poll, which refetches past the PR cache.
        world
            .request("pr.status", json!({ "workspace": draft, "refresh": true }))
            .await;
    };

    github_changes(json!({ "isDraft": true }), json!({})).await;
    let out = run(&world).await;
    assert_eq!(status(&draft).await, "in-progress", "{out}");
    assert_eq!(status(&review).await, "in-review", "{out}");
    assert_eq!(status(&changes).await, "in-progress", "{out}");

    write_flow(
        &script,
        json!({ "draft": "draft", "changes-requested": "changes-requested", "review": "in-review", "merged": "done" }),
    );
    let out = run(&world).await;
    assert_eq!(status(&draft).await, "draft", "{out}");
    assert_eq!(status(&review).await, "in-review", "{out}");
    assert_eq!(status(&changes).await, "changes-requested", "{out}");

    github_changes(json!({}), changes_requested_by("bob")).await;
    let out = run(&world).await;
    assert_eq!(status(&draft).await, "in-review", "ready for review: {out}");
    assert_eq!(status(&review).await, "changes-requested", "{out}");
    assert_eq!(status(&changes).await, "changes-requested", "{out}");

    write_flow(
        &script,
        json!({ "draft": "draft", "changes-requested": "draft", "review": "in-review", "merged": "done" }),
    );
    let out = run(&world).await;
    assert_eq!(status(&draft).await, "in-review", "{out}");
    assert_eq!(status(&review).await, "draft", "{out}");
    assert_eq!(status(&changes).await, "draft", "{out}");

    github_changes(json!({ "state": "MERGED" }), json!({ "isDraft": true })).await;
    let out = run(&world).await;
    assert_eq!(status(&draft).await, "done", "merged: {out}");
    assert_eq!(
        status(&review).await,
        "draft",
        "draft wins over changes requested: {out}"
    );

    write_flow(&script, json!({ "merged": "done" }));
    world
        .request("workspace.set", json!({ "id": changes, "status": "todo" }))
        .await;
    let out = run(&world).await;
    assert_eq!(
        status(&changes).await,
        "todo",
        "an unmapped condition leaves the card: {out}"
    );
}

#[tokio::test]
#[ignore = "reads from the real GitHub through the local gh login"]
async fn live_gh_viewer_login() {
    let github = icarus_github::GitHub::new(icarus_github::GhCli::new());
    let login = github.viewer_login().await.expect("gh login");
    assert!(!login.is_empty());
}
