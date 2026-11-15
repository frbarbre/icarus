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
    _server: Guard,
}

fn start() -> Env {
    let home = tempfile::tempdir().expect("home");
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).expect("config");
    let fake = sibling("icarus-fake-agent");
    std::fs::write(
        config.join("settings.json"),
        json!({ "agents": { "commands": { "claude": [fake] } } }).to_string(),
    )
    .expect("settings");
    let child = Command::new(sibling("icarus-server"))
        .args(["--home", home.path().to_str().expect("utf8"), "serve"])
        .env("ICARUS_WORKSPACE_ROOT", home.path().join("workspaces"))
        .env("ICARUS_CONFIG_DIR", &config)
        .env("ICARUS_GH_FIXTURES", home.path().join("no-gh"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("server");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !home.path().join("server.token").exists() {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    Env {
        home,
        _server: Guard(child),
    }
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

    fn scratch(&self) -> String {
        let dir = self.home.path().join("scratch");
        std::fs::create_dir_all(&dir).expect("scratch");
        dir.to_string_lossy().into_owned()
    }

    fn agent(&self, args: &[&str]) -> String {
        let scratch = self.scratch();
        let mut all = vec!["agent", "create", "--cwd", &scratch];
        all.extend_from_slice(args);
        let id = self.json(&all)["id"].as_str().expect("id").to_owned();
        self.ok(&["agent", "wait", &id]);
        id
    }

    fn wait_for_notifications(&self, count: usize) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let listed = self.json(&["notifications"]);
            if listed.as_array().map_or(0, Vec::len) >= count {
                return listed;
            }
            assert!(Instant::now() < deadline, "notifications: {listed}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[tokio::test]
async fn notifications_can_be_listed_and_marked_read_from_the_cli() {
    let env = start();
    env.agent(&["--title", "Fixer", "--prompt", "hello"]);
    let listed = env.wait_for_notifications(1);
    let id = listed[0]["id"].as_str().expect("id").to_owned();
    assert_eq!(listed[0]["kind"], "agent_turn_done");

    let text = env.ok(&["notifications", "--unread"]);
    assert!(
        text.contains(&id) && text.contains("Fixer: finished"),
        "{text}"
    );
    assert!(text.contains("echo: hello"), "{text}");
    env.ok(&["notifications", "read", &id]);
    assert_eq!(env.json(&["notifications", "--unread"]), json!([]));
    env.agent(&["--prompt", "fail now"]);
    env.wait_for_notifications(2);
    let marked = env.json(&["notifications", "read", "--all"]);
    assert_eq!(marked["updated"], 1);
    assert!(!env.icarus(&["notifications", "read"]).status.success());
    env.ok(&["notifications", "clear"]);
    assert_eq!(env.ok(&["notifications"]).trim(), "no notifications");
}

#[tokio::test]
async fn accounts_history_resume_and_workspace_sleep_from_the_cli() {
    let env = start();
    let added = env.ok(&["account", "add", "claude", "Work"]);
    assert!(added.contains("sign in with:"), "{added}");
    assert!(added.contains("CLAUDE_CONFIG_DIR="), "{added}");
    assert!(added.contains("/login"), "{added}");
    env.ok(&["account", "use", "Work"]);
    assert!(
        !env.icarus(&["account", "use", "system"]).status.success(),
        "system needs a provider"
    );
    let listed = env.ok(&["account", "list", "--provider", "claude"]);
    let default_line = listed
        .lines()
        .find(|l| l.starts_with('*'))
        .expect("a default");
    assert!(default_line.contains("Work"), "{listed}");
    assert!(listed.contains("not signed in"), "{listed}");

    let id = env.agent(&["--prompt", "env:CLAUDE_CONFIG_DIR"]);
    let read = env.ok(&["agent", "read", &id]);
    assert!(read.contains("claude-profiles"), "{read}");
    let listed = env.ok(&["account", "list", "--provider", "claude"]);
    assert!(listed.contains("session 10%, week 20%"), "{listed}");
    env.ok(&["account", "use", "system", "--provider", "claude"]);
    let accounts = env.json(&["account", "list", "--provider", "claude"]);
    let work = accounts
        .as_array()
        .expect("list")
        .iter()
        .find(|a| a["label"] == "Work")
        .expect("work")
        .clone();
    assert_eq!(work["default"], false);
    env.ok(&["account", "remove", work["id"].as_str().expect("id")]);

    env.ok(&["agent", "kill", &id]);
    let history = env.ok(&["agent", "history", "--search", "claude_config_dir"]);
    assert!(
        history.contains(&id) && history.contains("killed"),
        "{history}"
    );
    let resumed = env.json(&["agent", "resume", &id]);
    assert_eq!(resumed["id"], id.as_str());
    env.ok(&["agent", "send", &id, "back again"]);
    env.ok(&["agent", "wait", &id]);
    assert!(env.ok(&["agent", "read", &id]).contains("echo: back again"));

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
    let project = env.json(&["project", "add", repo.path().to_str().expect("utf8")]);
    let ws = env.json(&[
        "workspace",
        "create",
        "--project",
        project["id"].as_str().expect("id"),
        "nap",
        "--account",
        "system",
    ]);
    let ws_id = ws["id"].as_str().expect("id");
    assert_eq!(
        env.ok(&["workspace", "sleep", ws_id]).trim(),
        "parked 1 agent(s) and 0 terminal(s)"
    );
    assert_eq!(env.json(&["workspace", "show", ws_id])["sleeping"], true);
    assert_eq!(
        env.ok(&["workspace", "wake", ws_id]).trim(),
        "woke 1 agent(s) and 0 terminal(s)"
    );
}
