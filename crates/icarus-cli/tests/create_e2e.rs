//! `icarus workspace create --from-pr` and `--trust`, and `icarus project scripts / trust`.

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

struct Env {
    home: tempfile::TempDir,
    repos: tempfile::TempDir,
    _server: Guard,
}

impl Env {
    fn start(fixtures: Value) -> Self {
        let home = tempfile::tempdir().expect("home");
        std::fs::write(home.path().join("gh.json"), fixtures.to_string()).expect("fixtures");
        let child = Command::new(sibling("icarus-server"))
            .args(["--home", home.path().to_str().expect("utf8"), "serve"])
            .env("ICARUS_WORKSPACE_ROOT", home.path().join("workspaces"))
            .env("ICARUS_CONFIG_DIR", home.path().join("config"))
            .env("ICARUS_GH_FIXTURES", home.path().join("gh.json"))
            .env("SHELL", "/bin/sh")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("server");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !home.path().join("server.token").exists() {
            assert!(Instant::now() < deadline, "server did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        Self {
            home,
            repos: tempfile::tempdir().expect("repos"),
            _server: Guard(child),
        }
    }

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

    /// A repository known as github.com/acme/widgets whose fetches go to a local bare one.
    fn project(&self, files: &[(&str, &str)]) -> (PathBuf, String) {
        let origin = self.repos.path().join("origin.git");
        let repo = self.repos.path().join("widgets");
        std::fs::create_dir_all(&repo).expect("repo");
        git(
            self.repos.path(),
            &["init", "-q", "--bare", "-b", "main", "origin.git"],
        );
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "T"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        for (name, text) in files {
            std::fs::write(repo.join(name), text).expect("write");
        }
        std::fs::write(repo.join("readme.md"), "hi\n").expect("write");
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
        let project = self.json(&["project", "add", repo.to_str().expect("utf8")]);
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
        (repo, project["id"].as_str().expect("id").to_owned())
    }
}

#[test]
fn from_pr_checks_out_the_pull_requests_head() {
    let env = Env::start(json!({}));
    let (repo, project) = env.project(&[]);
    git(&repo, &["checkout", "-q", "-b", "fix/typo"]);
    std::fs::write(repo.join("typo.txt"), "fixed\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "typo"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["push", "-q", "origin", "HEAD:refs/pull/3/head"]);
    git(&repo, &["checkout", "-q", "main"]);
    std::fs::write(
        env.home.path().join("gh.json"),
        json!({ "pulls": [{
            "id": "PR_3", "number": 3, "title": "Fix the typo",
            "url": "https://github.com/acme/widgets/pull/3", "state": "OPEN", "isDraft": false,
            "headRefName": "fix/typo", "baseRefName": "main", "headRefOid": head,
            "isCrossRepository": true,
            "repository": { "name": "widgets", "owner": { "login": "acme" } },
        }] })
        .to_string(),
    )
    .expect("fixtures");

    let created = env.json(&[
        "workspace",
        "create",
        "--project",
        &project,
        "--from-pr",
        "https://github.com/acme/widgets/pull/3",
        "--no-agent",
    ]);
    assert_eq!(created["branch"], "fix/typo");
    assert_eq!(created["base_ref"], "origin/main");
    assert_eq!(created["title"], "Fix the typo");
    let worktree = PathBuf::from(created["path"].as_str().expect("path"));
    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), head);

    let missing = env.icarus(&[
        "workspace",
        "create",
        "--project",
        &project,
        "--from-pr",
        "nope",
        "--no-agent",
    ]);
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("pull request number or URL"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
}

#[test]
fn an_untrusted_setup_names_the_trust_command_and_trust_runs_it() {
    let env = Env::start(json!({}));
    let (_repo, project) = env.project(&[("orca.yaml", "scripts:\n  setup: touch from-setup\n")]);
    let refused = env.icarus(&[
        "workspace",
        "create",
        "--project",
        &project,
        "first",
        "--no-agent",
    ]);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains(&format!("icarus project trust {project}")),
        "{stderr}"
    );

    let scripts = env.ok(&["project", "scripts", &project]);
    assert!(
        scripts.contains("touch from-setup") && scripts.contains("NOT TRUSTED"),
        "{scripts}"
    );

    let created = env.json(&[
        "workspace",
        "create",
        "--project",
        &project,
        "first",
        "--no-agent",
        "--trust",
    ]);
    let worktree = PathBuf::from(created["path"].as_str().expect("path"));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !worktree.join("from-setup").exists() {
        assert!(Instant::now() < deadline, "setup never ran");
        std::thread::sleep(Duration::from_millis(100));
    }
    let trusted = env.json(&["project", "scripts", &project]);
    assert_eq!(trusted["trusted"], true);
    env.ok(&["project", "untrust", &project]);
    env.ok(&["project", "trust", &project]);
    env.ok(&[
        "workspace",
        "create",
        "--project",
        &project,
        "second",
        "--no-agent",
    ]);

    let rank = |name: &str| {
        let listed = env.json(&["workspace", "list"]);
        listed
            .as_array()
            .expect("list")
            .iter()
            .find(|workspace| workspace["name"] == name)
            .and_then(|workspace| workspace["manual_order"].as_i64())
            .expect("ranked")
    };
    assert!(rank("second") > rank("first"), "new workspaces rank on top");
    assert_eq!(
        env.ok(&["workspace", "reorder", "first", "second"]).trim(),
        "ordered 2 workspace(s)"
    );
    assert!(rank("first") > rank("second"), "reordered");
}

#[test]
fn workspaces_are_marked_read_and_unread() {
    let env = Env::start(json!({}));
    let (repo, _) = env.project(&[]);
    let project = env.json(&["project", "add", repo.to_str().expect("utf8")])["id"]
        .as_str()
        .expect("id")
        .to_owned();
    env.ok(&[
        "workspace",
        "create",
        "--project",
        &project,
        "quiet",
        "--no-agent",
    ]);
    assert_eq!(
        env.ok(&["workspace", "mark-unread", "quiet"]).trim(),
        "unread"
    );
    assert!(
        env.ok(&["workspace", "list"]).contains("  unread"),
        "the list says which workspaces are unread"
    );
    assert_eq!(
        env.json(&["workspace", "show", "quiet"])["unread"]["kind"],
        "done"
    );
    assert_eq!(env.ok(&["workspace", "mark-read", "quiet"]).trim(), "read");
    assert!(!env.ok(&["workspace", "list"]).contains("unread"));
}

/// Stands in for the app: answers `client.config` by writing what it would hold.
#[tokio::test]
async fn app_config_reaches_the_app_and_prints_its_answer() {
    let env = Env::start(json!({}));
    assert!(
        !env.icarus(&["app", "config", "get"]).status.success(),
        "no app connected"
    );
    let (_app, mut events) = icarus_client::Client::connect(env.home.path(), "icarus-app")
        .await
        .expect("app");
    let answering = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(event) = events.recv().await {
            if event.topic != "client.config" {
                continue;
            }
            let payload = event.payload;
            let value = match payload["op"].as_str() {
                Some("set") => payload["value"].clone(),
                _ => Value::Null,
            };
            let result = payload["result"].as_str().expect("result").to_owned();
            std::fs::write(result, json!({ "value": value }).to_string()).expect("answer");
            seen.push(payload);
            if seen.len() == 2 {
                return seen;
            }
        }
        seen
    });
    let home = env.home.path().to_owned();
    let run = move |args: &'static [&'static str]| {
        let home = home.clone();
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_icarus"))
                .arg("--home")
                .arg(home)
                .args(args)
                .output()
                .expect("icarus")
        })
    };
    let set = run(&[
        "app",
        "config",
        "set",
        "workspace.cardFields",
        r#"["pr","unread"]"#,
    ])
    .await
    .expect("join");
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    assert!(String::from_utf8_lossy(&set.stdout).contains("\"unread\""));
    let unset = run(&["app", "config", "unset", "workspace.cardFields"])
        .await
        .expect("join");
    assert!(
        String::from_utf8_lossy(&unset.stdout).contains("server's value applies"),
        "{}",
        String::from_utf8_lossy(&unset.stdout)
    );
    let seen = answering.await.expect("answered");
    assert_eq!(seen[0]["op"], "set");
    assert_eq!(
        seen[0]["value"],
        json!(["pr", "unread"]),
        "the value is JSON"
    );
    assert_eq!(seen[1]["op"], "unset");
}
