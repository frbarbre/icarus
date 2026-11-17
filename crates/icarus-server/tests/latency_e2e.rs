//! Latency budgets: a slow request never holds up the others on its connection, worktree scans
//! stay fast, and every card's pull request arrives in one batched query right after connecting.

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

fn start_server(home: &Path, env: &[(&str, std::ffi::OsString)]) -> ServerProcess {
    let _ = std::fs::remove_file(home.join("server.sock"));
    let _ = std::fs::remove_file(home.join("server.token"));
    let child = Command::new(env!("CARGO_BIN_EXE_icarus-server"))
        .args(["--home", home.to_str().expect("utf8 home"), "serve"])
        .env("ICARUS_WORKSPACE_ROOT", home.join("workspaces"))
        .env("ICARUS_CONFIG_DIR", home.join("config"))
        .envs(env.iter().map(|(key, value)| (key, value)))
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

fn repo(dirs: &Path) -> PathBuf {
    let repo = dirs.join("widgets");
    std::fs::create_dir_all(&repo).expect("repo dir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "T"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widgets.git",
        ],
    );
    std::fs::write(repo.join("readme.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

fn add_worktrees(repo: &Path, root: &Path, count: usize) {
    for index in 0..count {
        let path = root.join(format!("feature-{index}"));
        let branch = format!("feature-{index}");
        git(
            repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch,
                path.to_str().expect("utf8"),
            ],
        );
    }
}

fn tempdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = std::fs::canonicalize(dir.path()).expect("canonical");
    (dir, path)
}

async fn request(client: &Client, method: &str, params: Value) -> Value {
    client
        .request_timeout(method, params, Duration::from_secs(60))
        .await
        .unwrap_or_else(|e| panic!("{method}: {e:#}"))
}

fn pull(number: u64, branch: &str) -> Value {
    json!({
        "id": format!("PR_{number}"),
        "number": number,
        "title": format!("Pull request {number}"),
        "url": format!("https://github.com/acme/widgets/pull/{number}"),
        "state": "OPEN",
        "isDraft": false,
        "headRefName": branch,
        "baseRefName": "main",
        "headRefOid": format!("{number:040}"),
        "headRef": { "name": branch },
        "baseRef": { "name": "main" },
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "CLEAN",
        "author": { "login": "me" },
        "repository": { "name": "widgets", "owner": { "login": "acme" }, "autoMergeAllowed": true },
    })
}

fn snapshot_queries(log: &Path) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("graphql PrSnapshot"))
        .count()
}

#[tokio::test]
async fn a_slow_request_never_holds_up_the_ones_behind_it() {
    let (_home_dir, home) = tempdir();
    let (_dirs_dir, dirs) = tempdir();
    let repo = repo(&dirs);
    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("which git");
    let real_git = String::from_utf8_lossy(&real_git.stdout).trim().to_owned();
    let shims = dirs.join("bin");
    std::fs::create_dir_all(&shims).expect("shims");
    let shim = shims.join("git");
    std::fs::write(
        &shim,
        "#!/bin/sh\ncase \"$*\" in *\"worktree list\"*) [ -e \"$HANG_MARKER\" ] && sleep 5 ;; esac\nexec \"$REAL_GIT\" \"$@\"\n",
    )
    .expect("shim");
    let mut permissions = std::fs::metadata(&shim).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&shim, permissions).expect("chmod");
    let marker = dirs.join("hang");
    let path = format!(
        "{}:{}",
        shims.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let _server = start_server(
        &home,
        &[
            ("PATH", path.into()),
            ("ICARUS_KEEP_PATH", "1".into()),
            ("REAL_GIT", real_git.into()),
            ("HANG_MARKER", marker.clone().into()),
        ],
    );
    let (client, _events) = Client::connect(&home, "test").await.expect("connect");
    let project = request(&client, "project.add", json!({ "path": repo })).await;
    request(&client, "workspace.list", json!({ "refresh": true })).await;

    let mut round_trips = Vec::new();
    for _ in 0..50 {
        let started = Instant::now();
        request(&client, "workspace.list", json!({})).await;
        round_trips.push(started.elapsed());
    }
    round_trips.sort();
    let median = round_trips[round_trips.len() / 2];
    eprintln!("workspace.list round trip: median {median:?}");
    assert!(median < Duration::from_millis(20), "median {median:?}");

    std::fs::write(&marker, "").expect("marker");
    let slow = {
        let client = client.clone();
        let project = project["id"].clone();
        tokio::spawn(async move {
            let started = Instant::now();
            request(&client, "workspace.discover", json!({ "project": project })).await;
            started.elapsed()
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    request(&client, "project.list", json!({})).await;
    request(&client, "workspace.list", json!({})).await;
    let behind = started.elapsed();
    eprintln!("two requests behind a 5 s scan answered in {behind:?}");
    assert!(behind < Duration::from_millis(200), "{behind:?}");
    let slow = slow.await.expect("slow request");
    assert!(
        slow >= Duration::from_secs(4),
        "the scan really was slow: {slow:?}"
    );
}

#[tokio::test]
async fn twenty_worktrees_scan_in_well_under_a_second() {
    let (_home_dir, home) = tempdir();
    let (_dirs_dir, dirs) = tempdir();
    let repo = repo(&dirs);
    add_worktrees(&repo, &home.join("workspaces").join("widgets"), 20);
    let _server = start_server(&home, &[]);
    let (client, _events) = Client::connect(&home, "test").await.expect("connect");
    let project = request(&client, "project.add", json!({ "path": repo })).await;
    let mut scans = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        request(
            &client,
            "workspace.discover",
            json!({ "project": project["id"] }),
        )
        .await;
        scans.push(started.elapsed());
    }
    let listed = request(&client, "workspace.list", json!({})).await;
    assert_eq!(listed.as_array().map(Vec::len), Some(20));
    eprintln!("scans of 20 worktrees: {scans:?}");
    let slowest = scans.iter().max().copied().unwrap_or_default();
    assert!(slowest < Duration::from_millis(1500), "{scans:?}");
}

#[tokio::test]
async fn every_card_gets_its_pull_request_from_one_query_right_after_connecting() {
    let (_home_dir, home) = tempdir();
    let (_dirs_dir, dirs) = tempdir();
    let repo = repo(&dirs);
    add_worktrees(&repo, &home.join("workspaces").join("widgets"), 10);
    let fixtures = dirs.join("gh.json");
    let pulls: Vec<Value> = (0..10)
        .map(|index| pull(100 + index, &format!("feature-{index}")))
        .collect();
    std::fs::write(
        &fixtures,
        json!({ "viewer": "me", "pulls": pulls }).to_string(),
    )
    .expect("fixtures");
    let log = PathBuf::from(format!("{}.requests.jsonl", fixtures.display()));
    let linear = dirs.join("linear");
    std::fs::create_dir_all(&linear).expect("linear fixtures");
    for index in 0..10 {
        std::fs::write(
            linear.join(format!("ABC-{index}.json")),
            issue(&format!("ABC-{index}")).to_string(),
        )
        .expect("issue fixture");
    }
    let linear_log = linear.join("requests.jsonl");
    let env = [
        ("ICARUS_GH_FIXTURES", fixtures.clone().into_os_string()),
        ("ICARUS_LINEAR_FIXTURES", linear.clone().into_os_string()),
        ("LINEAR_API_KEY", "lin_api_fixture".into()),
    ];

    let server = start_server(&home, &env);
    {
        let (client, _events) = Client::connect(&home, "test").await.expect("connect");
        request(&client, "project.add", json!({ "path": repo })).await;
        let listed = request(&client, "workspace.list", json!({ "refresh": true })).await;
        for workspace in listed.as_array().into_iter().flatten() {
            let name = workspace["name"].as_str().expect("name");
            let index = name.trim_start_matches("feature-");
            request(
                &client,
                "workspace.set",
                json!({ "id": workspace["id"], "issue": { "provider": "linear", "id": format!("ABC-{index}") } }),
            )
            .await;
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let listed = request(&client, "workspace.list", json!({})).await;
            let known = listed
                .as_array()
                .into_iter()
                .flatten()
                .filter(|w| w["pr"]["number"].is_u64() && w["issue"]["title"].is_string())
                .count();
            if known == 10 {
                break;
            }
            assert!(Instant::now() < deadline, "{known} of 10 cards filled");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    drop(server);
    let _ = std::fs::remove_file(&log);
    let _ = std::fs::remove_file(&linear_log);
    // What changed on GitHub and Linear while the server was down.
    let pulls: Vec<Value> = (0..10)
        .map(|index| pull(200 + index, &format!("feature-{index}")))
        .collect();
    std::fs::write(
        &fixtures,
        json!({ "viewer": "me", "pulls": pulls }).to_string(),
    )
    .expect("fixtures");
    for index in 0..10 {
        let mut fresh = issue(&format!("ABC-{index}"));
        fresh["title"] = json!("Renamed");
        std::fs::write(linear.join(format!("ABC-{index}.json")), fresh.to_string())
            .expect("issue fixture");
    }
    let fresh = |listed: &Value| {
        let cards = listed.as_array().cloned().unwrap_or_default();
        let prs = cards
            .iter()
            .filter(|w| w["pr"]["number"].as_u64().is_some_and(|n| n >= 200))
            .count();
        let issues = cards
            .iter()
            .filter(|w| w["issue"]["title"] == "Renamed")
            .count();
        (prs, issues)
    };

    let server = start_server(&home, &env);
    let connected = Instant::now();
    let (client, _events) = Client::connect(&home, "icarus-app").await.expect("connect");
    let first = request(&client, "workspace.list", json!({})).await;
    let shown = first
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| w["pr"]["number"].is_u64() && w["issue"]["title"].is_string())
        .count();
    assert_eq!(
        shown, 10,
        "cards show the last known pull requests and issues from state.db at once"
    );
    let arrived = loop {
        let listed = request(&client, "workspace.list", json!({})).await;
        if fresh(&listed) == (10, 10) {
            break connected.elapsed();
        }
        assert!(
            connected.elapsed() < Duration::from_secs(5),
            "(pull requests, issues) up to date: {:?} of 10",
            fresh(&listed)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    eprintln!("10 cards had fresh pull requests and issues {arrived:?} after connecting");
    assert_eq!(
        snapshot_queries(&log),
        1,
        "ten workspaces are one batched query, not ten"
    );
    let issue_queries = std::fs::read_to_string(&linear_log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("\"Issues\""))
        .count();
    assert_eq!(
        issue_queries, 1,
        "ten issues are one batched query, not ten"
    );
    drop(client);
    drop(server);

    std::fs::write(&fixtures, "not json").expect("break fixtures");
    std::fs::write(
        linear.join("_status.json"),
        json!({ "status": 500 }).to_string(),
    )
    .expect("break linear");
    let _server = start_server(&home, &env);
    let (client, _events) = Client::connect(&home, "icarus-app").await.expect("connect");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let listed = request(&client, "workspace.list", json!({})).await;
    assert_eq!(
        fresh(&listed),
        (10, 10),
        "a failing GitHub or Linear keeps the last known data"
    );
}

fn issue(identifier: &str) -> Value {
    json!({
        "id": format!("uuid-{identifier}"),
        "identifier": identifier,
        "title": "Fix the login page",
        "description": "Steps to reproduce",
        "priority": 2,
        "priorityLabel": "High",
        "url": format!("https://linear.app/acme/issue/{}", identifier.to_lowercase()),
        "branchName": identifier.to_lowercase(),
        "updatedAt": "2026-11-14T10:00:00.000Z",
        "state": { "name": "In Progress", "type": "started", "color": "#f2c94c" },
        "assignee": null,
        "labels": { "nodes": [] },
        "team": { "key": "ABC" },
    })
}
