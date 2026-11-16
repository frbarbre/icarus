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

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

struct Env {
    home: tempfile::TempDir,
    _server: Guard,
    worktree: PathBuf,
    workspace: String,
}

impl Env {
    fn start() -> Self {
        let home = tempfile::tempdir().expect("home");
        let config = home.path().join("config");
        std::fs::create_dir_all(&config).expect("config");
        std::fs::write(
            config.join("icarus.json"),
            json!({ "agents": { "commands": { "claude": [sibling("icarus-fake-agent")] } } })
                .to_string(),
        )
        .expect("settings");
        let repo = home.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("repo");
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "me@example.com"]);
        git(&repo, &["config", "user.name", "Me"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("src/main.rs"), "fn main() {\n    run();\n}\n").expect("write");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
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
        let mut env = Self {
            home,
            _server: Guard(child),
            worktree: PathBuf::new(),
            workspace: String::new(),
        };
        let project = env.json(
            &[
                "project",
                "add",
                repo.to_str().expect("utf8"),
                "--base",
                "main",
            ],
            None,
        );
        let ws = env.json(
            &[
                "workspace",
                "create",
                "--project",
                project["id"].as_str().expect("id"),
                "notes",
                "--no-agent",
            ],
            None,
        );
        env.workspace = ws["id"].as_str().expect("id").to_owned();
        env.worktree = PathBuf::from(ws["path"].as_str().expect("path"));
        env
    }

    fn run(&self, args: &[&str], cwd: Option<&Path>, vars: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_icarus"));
        command
            .arg("--home")
            .arg(self.home.path())
            .args(args)
            .env_remove("ICARUS_AGENT_ID")
            .env_remove("ICARUS_WORKSPACE_ID")
            .current_dir(cwd.unwrap_or(self.home.path()));
        for (key, value) in vars {
            command.env(key, value);
        }
        command.output().expect("icarus")
    }

    fn ok_in(&self, args: &[&str], cwd: Option<&Path>, vars: &[(&str, &str)]) -> String {
        let output = self.run(args, cwd, vars);
        assert!(
            output.status.success(),
            "icarus {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn json(&self, args: &[&str], cwd: Option<&Path>) -> Value {
        self.json_with(args, cwd, &[])
    }

    fn json_with(&self, args: &[&str], cwd: Option<&Path>, vars: &[(&str, &str)]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&self.ok_in(&all, cwd, vars)).expect("json output")
    }
}

#[test]
fn notes_round_trip_through_the_cli_from_inside_the_worktree() {
    let env = Env::start();
    let src = env.worktree.join("src");

    let note = env.json(
        &["notes", "add", "main.rs:2", "Is", "this", "called\\ntwice?"],
        Some(&src),
    );
    let id = note["id"].as_str().expect("id").to_owned();
    assert_eq!(
        note["workspace_id"], env.workspace,
        "workspace from the cwd"
    );
    assert_eq!(note["path"], "src/main.rs", "path relative to the cwd");
    assert_eq!(note["body"], "Is this called\ntwice?");
    assert_eq!(note["author"], "user");

    let human = env.ok_in(&["notes", "list"], Some(&env.worktree), &[]);
    assert_eq!(
        human.trim(),
        format!("{id}  src/main.rs:2  user\n    Is this called")
    );

    let agent = env.json_with(
        &["notes", "add", "src/main.rs:1-3", "I", "wrapped", "this."],
        Some(env.home.path()),
        &[
            ("ICARUS_AGENT_ID", "agent-abc"),
            ("ICARUS_WORKSPACE_ID", env.workspace.as_str()),
        ],
    );
    assert_eq!(agent["author"], "agent");
    assert_eq!(agent["agent_id"], "agent-abc");
    assert_eq!(agent["excerpt"], "fn main() {\n    run();\n}");

    let answer = env.ok_in(
        &["notes", "reply", &id, "--wait", "@fake", "Only", "once."],
        Some(&env.worktree),
        &[],
    );
    assert_eq!(answer.trim(), "echo: Only once.");
    let shown = env.ok_in(&["notes", "show", &id], None, &[]);
    assert!(shown.contains("      2 |     run();"), "{shown}");
    assert!(shown.contains("reply agent (reply-"), "{shown}");
    assert!(shown.contains(", done:\n    echo: Only once."), "{shown}");

    let attached = env.json(&["notes", "attach", &id], None);
    assert_eq!(attached["kind"], "note");
    assert!(
        attached["text"]
            .as_str()
            .is_some_and(|t| t.contains("Reply agent:\necho: Only once.")),
        "{attached}"
    );

    env.ok_in(&["notes", "resolve", &id], Some(&env.worktree), &[]);
    let open = env.json(&["notes", "list", "--workspace", &env.workspace], None);
    assert_eq!(open.as_array().map(Vec::len), Some(1));
    let all = env.json(
        &["notes", "list", "--all", "--path", "src/main.rs"],
        Some(&env.worktree),
    );
    assert_eq!(all.as_array().map(Vec::len), Some(2));

    let removed = env.ok_in(&["notes", "rm", &id], Some(&env.worktree), &[]);
    assert_eq!(removed.trim(), format!("removed {id}"));
    let missing = env.run(&["notes", "show", &id], None, &[]);
    assert!(!missing.status.success());

    let outside = env.run(&["notes", "list"], Some(env.home.path()), &[]);
    assert!(!outside.status.success(), "no workspace outside a worktree");
    let no_line = env.run(
        &["notes", "add", "src/main.rs", "x"],
        Some(&env.worktree),
        &[],
    );
    assert!(
        String::from_utf8_lossy(&no_line.stderr).contains("give a line"),
        "{}",
        String::from_utf8_lossy(&no_line.stderr)
    );
}
