//! Per-project setup and archive scripts, end to end: real worktrees, real Setup terminals.

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
        .env("ICARUS_CONFIG_DIR", home.join("config"))
        .env("ICARUS_GH_FIXTURES", home.join("no-gh"))
        // A plain shell keeps the Setup terminal free of the developer's login profile.
        .env("SHELL", "/bin/sh")
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

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

fn repo_with_hooks(hooks: Value) -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("repo");
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "T"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.path().join("readme.md"), "hello\n").expect("write");
    std::fs::write(
        repo.path().join(".gitignore"),
        "icarus.local.json\nsetup-*.txt\nkeep\nagain\n",
    )
    .expect("write");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    let settings = json!({
        "agents": { "commands": { "claude": [env!("CARGO_BIN_EXE_icarus-fake-agent")] } },
        "project": { "hooks": hooks },
    });
    std::fs::write(
        repo.path().join("icarus.local.json"),
        serde_json::to_vec_pretty(&settings).expect("json"),
    )
    .expect("write settings");
    repo
}

struct World {
    _home: tempfile::TempDir,
    repo: tempfile::TempDir,
    _server: ServerProcess,
    client: std::sync::Arc<Client>,
    project: String,
}

impl World {
    async fn new(hooks: Value) -> Self {
        let repo = repo_with_hooks(hooks);
        let home = tempfile::tempdir().expect("home");
        let server = start_server(home.path());
        let (client, _) = Client::connect(home.path(), "icarus-cli")
            .await
            .expect("cli");
        let project = client
            .request("project.add", json!({ "path": repo.path() }))
            .await
            .expect("project");
        Self {
            _home: home,
            repo,
            _server: server,
            client,
            project: project["id"].as_str().expect("id").to_owned(),
        }
    }

    async fn create(&self, name: &str, extra: Value) -> Result<Value, String> {
        let mut params = json!({ "project": self.project, "name": name, "agent": false });
        for (key, value) in extra.as_object().into_iter().flatten() {
            params[key] = value.clone();
        }
        self.client
            .request_timeout("workspace.create", params, Duration::from_secs(60))
            .await
            .map_err(|e| format!("{e:#}"))
    }

    async fn info(&self, id: &str) -> Value {
        self.client
            .request("workspace.info", json!({ "id": id }))
            .await
            .expect("info")
    }

    /// Waits until the workspace's setup has finished, and returns its status.
    async fn finished_setup(&self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let setup = self.info(id).await["setup"].clone();
            if setup["state"] != "running" {
                return setup;
            }
            assert!(Instant::now() < deadline, "setup never finished: {setup}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn remove(&self, id: &str, extra: Value) -> Result<Value, String> {
        let mut params = json!({ "id": id });
        for (key, value) in extra.as_object().into_iter().flatten() {
            params[key] = value.clone();
        }
        self.client
            .request_timeout("workspace.remove", params, Duration::from_secs(60))
            .await
            .map_err(|e| format!("{e:#}"))
    }
}

fn path_of(workspace: &Value) -> PathBuf {
    PathBuf::from(workspace["path"].as_str().expect("path"))
}

#[tokio::test]
async fn setup_runs_in_a_setup_terminal_with_orcas_env_and_reports_success() {
    let world = World::new(json!({ "scripts": {
        "setup": "printf '%s\\n' \"$ORCA_ROOT_PATH\" \"$ORCA_WORKTREE_PATH\" \"$ORCA_WORKSPACE_NAME\" \"$PWD\" \"$ICARUS_WORKSPACE_ID\" > setup-env.txt\necho set up",
    } }))
    .await;
    let created = world
        .create("first", json!({ "agent": true, "layout": "default" }))
        .await
        .expect("create");
    let id = created["id"].as_str().expect("id");
    let terminal = created["setup"]["terminal"]
        .as_str()
        .expect("setup terminal")
        .to_owned();
    assert_eq!(created["setup"]["state"], "running");
    assert_eq!(created["setup"]["source"], "local");
    assert_eq!(
        created["layout"]["layout"]["tabs"][1],
        json!({ "type": "terminal", "terminal_id": terminal }),
        "the default layout shows the Setup terminal next to the agent"
    );
    let info = world
        .client
        .request("terminal.info", json!({ "id": terminal }))
        .await
        .expect("terminal");
    assert_eq!(info["label"], "Setup");
    assert_eq!(info["workspace"], id);

    let setup = world.finished_setup(id).await;
    assert_eq!(setup["state"], "succeeded", "{setup}");
    assert_eq!(setup["exit_code"], 0);
    let worktree = path_of(&created);
    let lines: Vec<String> = std::fs::read_to_string(worktree.join("setup-env.txt"))
        .expect("the script ran in the worktree")
        .lines()
        .map(str::to_owned)
        .collect();
    let canonical = |p: &Path| p.canonicalize().expect("canonical");
    assert_eq!(
        canonical(Path::new(&lines[0])),
        canonical(world.repo.path())
    );
    assert_eq!(canonical(Path::new(&lines[1])), canonical(&worktree));
    assert_eq!(lines[2], "first");
    assert_eq!(canonical(Path::new(&lines[3])), canonical(&worktree));
    assert_eq!(lines[4], id);
    let still = world
        .client
        .request("terminal.info", json!({ "id": terminal }))
        .await
        .expect("terminal");
    assert_eq!(still["running"], true, "the Setup tab stays a usable shell");
}

#[tokio::test]
async fn failing_and_slow_setups_are_reported_and_can_be_skipped_or_rerun() {
    let world = World::new(json!({
        "scripts": { "setup": "if [ -f again ]; then echo ok; else exit 3; fi" },
    }))
    .await;
    let failed = world.create("fails", json!({})).await.expect("create");
    let id = failed["id"].as_str().expect("id");
    let setup = world.finished_setup(id).await;
    assert_eq!(setup["state"], "failed", "{setup}");
    assert_eq!(setup["exit_code"], 3);

    std::fs::write(path_of(&failed).join("again"), "").expect("marker");
    let rerun = world
        .client
        .request("workspace.setup", json!({ "id": id }))
        .await
        .expect("rerun");
    assert_ne!(
        rerun["terminal"], setup["terminal"],
        "a re-run gets a new Setup terminal"
    );
    assert_eq!(world.finished_setup(id).await["state"], "succeeded");

    let skipped = world
        .create("skipped", json!({ "setup": "skip" }))
        .await
        .expect("create");
    assert_eq!(skipped["setup"]["state"], "skipped");
    assert!(skipped["setup"]["terminal"].is_null());
    assert!(
        skipped["terminals"].as_array().is_some_and(Vec::is_empty),
        "--no-setup starts no terminal"
    );

    let slow =
        World::new(json!({ "scripts": { "setup": "sleep 30" }, "setupTimeoutMs": 500 })).await;
    let created = slow.create("slow", json!({})).await.expect("create");
    let setup = slow
        .finished_setup(created["id"].as_str().expect("id"))
        .await;
    assert_eq!(setup["state"], "failed");
    assert!(
        setup["error"]
            .as_str()
            .is_some_and(|e| e.contains("timed out")),
        "{setup}"
    );
}

#[tokio::test]
async fn an_asking_project_needs_a_decision_and_a_waiting_agent_holds_its_prompt() {
    let world = World::new(json!({
        "scripts": { "setup": "sleep 2" },
        "setupRunPolicy": "ask",
        "setupAgentStartupPolicy": "wait-for-setup",
    }))
    .await;
    let error = world.create("undecided", json!({})).await.expect_err("ask");
    assert!(error.contains("setup_decision_required"), "{error}");
    assert!(
        world
            .client
            .request("workspace.list", json!({}))
            .await
            .expect("list")
            .as_array()
            .is_some_and(Vec::is_empty),
        "nothing is created before the decision"
    );

    let created = world
        .create(
            "decided",
            json!({ "setup": "run", "agent": true, "prompt": "hello" }),
        )
        .await
        .expect("create");
    let id = created["id"].as_str().expect("id");
    let agent = created["agents"][0]["id"]
        .as_str()
        .expect("agent")
        .to_owned();
    let early = world
        .client
        .request("agent.info", json!({ "id": agent }))
        .await
        .expect("agent");
    assert_eq!(early["turns"], 0, "the first prompt waits for setup");
    assert_eq!(world.finished_setup(id).await["state"], "succeeded");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let info = world
            .client
            .request("agent.info", json!({ "id": agent }))
            .await
            .expect("agent");
        if info["turns"] == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the held prompt never went: {info}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn the_archive_script_runs_before_removal_and_a_failure_keeps_the_workspace() {
    let marker_dir = tempfile::tempdir().expect("markers");
    let marker = marker_dir.path().join("archived.txt");
    let world = World::new(json!({ "scripts": {
        "archive": format!(
            "test -f keep && {{ echo still needed >&2; exit 7; }}; test -d \"$ORCA_WORKTREE_PATH\" && echo \"$ORCA_WORKSPACE_NAME\" >> '{}'",
            marker.display()
        ),
    } }))
    .await;

    let kept = world.create("kept", json!({})).await.expect("create");
    let kept_id = kept["id"].as_str().expect("id");
    std::fs::write(path_of(&kept).join("keep"), "").expect("keep");
    let refused = world.remove(kept_id, json!({})).await.expect_err("refused");
    assert!(
        refused.contains("archive_failed") && refused.contains("still needed"),
        "{refused}"
    );
    assert!(
        path_of(&kept).is_dir(),
        "a failed archive keeps the worktree"
    );
    world.info(kept_id).await;

    let forced = world
        .remove(kept_id, json!({ "force": true }))
        .await
        .expect("force");
    assert_eq!(forced["archive"]["success"], false);
    assert_eq!(forced["archive"]["exit_code"], 7);
    assert!(!path_of(&kept).exists());

    let clean = world.create("clean", json!({})).await.expect("create");
    let removed = world
        .remove(clean["id"].as_str().expect("id"), json!({}))
        .await
        .expect("remove");
    assert_eq!(removed["archive"]["success"], true);
    assert_eq!(
        std::fs::read_to_string(&marker).expect("marker"),
        "clean\n",
        "it ran while the worktree still existed"
    );

    let skipped = world
        .create("skip-archive", json!({}))
        .await
        .expect("create");
    std::fs::write(path_of(&skipped).join("keep"), "").expect("keep");
    let removed = world
        .remove(
            skipped["id"].as_str().expect("id"),
            json!({ "skip_archive": true }),
        )
        .await
        .expect("skip");
    assert!(removed["archive"].is_null());
}
