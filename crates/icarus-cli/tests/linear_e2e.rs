use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn sibling(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_BIN_EXE_icarus")).with_file_name(name);
    assert!(
        path.exists(),
        "{name} not built; run tests for the whole workspace"
    );
    path
}

struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Env {
    home: tempfile::TempDir,
    fixtures: PathBuf,
    _server: Guard,
}

fn start(key: Option<&str>) -> Env {
    let home = tempfile::tempdir().expect("home");
    let fixtures = home.path().join("linear");
    let config = home.path().join("config");
    std::fs::create_dir_all(&fixtures).expect("fixtures");
    std::fs::create_dir_all(&config).expect("config");
    std::fs::write(
        config.join("icarus.json"),
        json!({ "linear": { "poll_interval_ms": 1000 } }).to_string(),
    )
    .expect("settings");
    write_issue(&fixtures, "ABC-12", "Fix login redirect", "In Progress");
    let mut command = Command::new(sibling("icarus-server"));
    command
        .args(["--home", home.path().to_str().expect("utf8"), "serve"])
        .env("ICARUS_WORKSPACE_ROOT", home.path().join("workspaces"))
        .env("ICARUS_CONFIG_DIR", &config)
        .env("ICARUS_GH_FIXTURES", home.path().join("no-gh"))
        .env("ICARUS_LINEAR_FIXTURES", &fixtures)
        .env_remove("LINEAR_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(key) = key {
        command.env("LINEAR_API_KEY", key);
    }
    let child = command.spawn().expect("server");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !home.path().join("server.token").exists() {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    Env {
        home,
        fixtures,
        _server: Guard(child),
    }
}

fn write_issue(dir: &Path, identifier: &str, title: &str, state: &str) {
    let lower = identifier.to_lowercase();
    let node = json!({
        "id": format!("uuid-{lower}"),
        "identifier": identifier,
        "title": title,
        "description": "Users land on /home after login instead of where they started.",
        "priority": 2,
        "priorityLabel": "High",
        "url": format!("https://linear.app/acme/issue/{lower}/fix-login-redirect"),
        "branchName": format!("frederik/{lower}-fix-login-redirect"),
        "updatedAt": "2026-11-14T10:00:00.000Z",
        "state": { "name": state, "type": "started", "color": "#f2c94c" },
        "assignee": { "name": "Frederik", "displayName": "frederik", "avatarUrl": "https://a/f.png" },
        "labels": { "nodes": [{ "name": "Bug", "color": "#eb5757" }] },
        "team": { "key": "ABC" },
    });
    std::fs::write(dir.join(format!("{identifier}.json")), node.to_string()).expect("issue");
}

impl Env {
    fn icarus(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_icarus"))
            .arg("--home")
            .arg(self.home.path())
            .args(args)
            .output()
            .expect("icarus")
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.icarus(args);
        assert!(
            output.status.success(),
            "icarus {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&self.ok(&all)).expect("json output")
    }

    fn project(&self) -> (tempfile::TempDir, String) {
        let repo = tempfile::tempdir().expect("repo");
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@example.com"],
            &["config", "user.name", "T"],
            &["config", "commit.gpgsign", "false"],
            &["commit", "-q", "--allow-empty", "-m", "init"],
        ] {
            let status = Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?}");
        }
        let project = self.json(&["project", "add", repo.path().to_str().expect("utf8")]);
        let id = project["id"].as_str().expect("project id").to_owned();
        (repo, id)
    }
}

#[tokio::test]
async fn a_workspace_from_a_linear_issue_is_named_from_it_and_its_card_stays_current() {
    let env = start(Some("lin_test_fixture_key"));
    let (_repo, project) = env.project();

    let created = env.json(&[
        "workspace",
        "create",
        "--project",
        &project,
        "--from-linear",
        "abc-12",
        "--no-agent",
    ]);
    assert_eq!(created["name"], "abc-12-fix-login-redirect");
    assert_eq!(created["branch"], "frederik/abc-12-fix-login-redirect");
    assert_eq!(created["issue"]["provider"], "linear");
    assert_eq!(created["issue"]["id"], "ABC-12");
    assert_eq!(created["issue"]["title"], "Fix login redirect");
    let workspace = created["id"].as_str().expect("id").to_owned();

    let listed = env.json(&["workspace", "list"]);
    let card = &listed[0];
    assert_eq!(
        card["issue"]["id"], "ABC-12",
        "the stored identifier is kept"
    );
    assert_eq!(card["issue"]["title"], "Fix login redirect");
    assert_eq!(card["issue"]["state"]["name"], "In Progress");
    assert_eq!(card["issue"]["labels"][0]["name"], "Bug");
    assert_eq!(card["issue"]["assignee"]["avatar_url"], "https://a/f.png");
    assert_eq!(
        card["issue"]["url"],
        "https://linear.app/acme/issue/abc-12/fix-login-redirect"
    );

    let shown = env.ok(&["issue", "show", "ABC-12"]);
    assert!(shown.contains("ABC-12  Fix login redirect"), "{shown}");
    assert!(
        shown.contains("In Progress  High  @frederik  [Bug]"),
        "{shown}"
    );
    assert!(
        shown.contains("branch: frederik/abc-12-fix-login-redirect"),
        "{shown}"
    );
    assert!(shown.contains("Users land on /home"), "{shown}");

    let opened = env.ok(&["issue", "open", "abc-12", "--no-browser"]);
    assert_eq!(
        opened.trim(),
        "https://linear.app/acme/issue/abc-12/fix-login-redirect"
    );

    let missing = env.icarus(&["issue", "show", "ABC-99"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not found"));

    let (_app, mut events) = icarus_client::Client::connect(env.home.path(), "icarus-app")
        .await
        .expect("app");
    write_issue(
        &env.fixtures,
        "ABC-12",
        "Fix login redirect loop",
        "In Review",
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let event = tokio::time::timeout(left, events.recv())
            .await
            .expect("workspace.changed in time")
            .expect("open stream");
        if event.topic == "workspace.changed" {
            assert_eq!(event.payload["workspaces"], json!([workspace]));
            assert_eq!(event.payload["source"], "linear");
            break;
        }
    }
    let listed = env.json(&["workspace", "list"]);
    assert_eq!(listed[0]["issue"]["title"], "Fix login redirect loop");
    assert_eq!(listed[0]["issue"]["state"]["name"], "In Review");

    let log = std::fs::read_to_string(env.fixtures.join("requests.jsonl")).expect("log");
    assert!(!log.contains("lin_test_fixture_key"));
    assert!(log.lines().all(|line| line.contains("\"authorized\":true")));
}

#[tokio::test]
async fn without_a_key_linear_degrades_quietly() {
    let env = start(None);
    let (_repo, project) = env.project();
    let created = env.json(&[
        "workspace",
        "create",
        "--project",
        &project,
        "--from-linear",
        "ABC-12",
        "--no-agent",
    ]);
    assert_eq!(created["name"], "abc-12");
    assert_eq!(created["branch"], "abc-12");
    assert_eq!(
        created["issue"],
        json!({ "provider": "linear", "id": "ABC-12" })
    );

    let listed = env.json(&["workspace", "list"]);
    assert_eq!(
        listed[0]["issue"],
        json!({ "provider": "linear", "id": "ABC-12" })
    );

    let shown = env.icarus(&["issue", "show", "ABC-12"]);
    assert!(!shown.status.success());
    assert!(
        String::from_utf8_lossy(&shown.stderr).contains("not configured"),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    assert!(
        !env.fixtures.join("requests.jsonl").exists(),
        "nothing is sent without a key"
    );
}
