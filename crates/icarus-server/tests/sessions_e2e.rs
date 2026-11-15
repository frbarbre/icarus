use std::path::Path;
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

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

fn fake() -> &'static str {
    env!("CARGO_BIN_EXE_icarus-fake-agent")
}

fn start(home: &Path, config: &Path, env: &[(&str, &str)]) -> ServerProcess {
    let _ = std::fs::remove_file(home.join("server.token"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_icarus-server"));
    command
        .args(["--home", home.to_str().expect("utf8"), "serve"])
        .env("ICARUS_WORKSPACE_ROOT", home.join("workspaces"))
        .env("ICARUS_CONFIG_DIR", config)
        .env("ICARUS_GH_FIXTURES", home.join("no-gh"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("ANTHROPIC_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    let child = command.spawn().expect("spawn server");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(home.join("server.sock").exists() && home.join("server.token").exists()) {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    ServerProcess(child)
}

struct World {
    home: TempDir,
    config: TempDir,
    repo: TempDir,
    server: Option<ServerProcess>,
    client: Arc<Client>,
}

impl World {
    async fn new(env: &[(&str, &str)]) -> Self {
        let home = tempfile::tempdir().expect("home");
        let config = tempfile::tempdir().expect("config");
        std::fs::write(
            config.path().join("settings.json"),
            json!({ "agents": { "commands": { "claude": [fake()] } } }).to_string(),
        )
        .expect("settings");
        let repo = tempfile::tempdir().expect("repo");
        git(repo.path(), &["init", "-q", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "t@example.com"]);
        git(repo.path(), &["config", "user.name", "T"]);
        git(repo.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.path().join("readme.md"), "hello\n").expect("write");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-q", "-m", "init"]);
        let server = start(home.path(), config.path(), env);
        let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
        Self {
            home,
            config,
            repo,
            server: Some(server),
            client,
        }
    }

    async fn restart(&mut self) {
        drop(self.server.take());
        self.server = Some(start(self.home.path(), self.config.path(), &[]));
        let (client, _events) = Client::connect(self.home.path(), "test")
            .await
            .expect("reconnect");
        self.client = client;
    }

    async fn request(&self, method: &str, params: Value) -> Value {
        self.client
            .request(method, params)
            .await
            .unwrap_or_else(|e| panic!("{method}: {e:#}"))
    }

    async fn workspace(&self, name: &str, extra: Value) -> Value {
        let project = self
            .request("project.add", json!({ "path": self.repo.path() }))
            .await;
        let mut params = json!({ "project": project["id"], "name": name });
        for (key, value) in extra.as_object().into_iter().flatten() {
            params[key] = value.clone();
        }
        self.request("workspace.create", params).await
    }

    async fn wait(&self, id: &str) -> Value {
        self.request("agent.wait", json!({ "id": id, "timeout_ms": 10000 }))
            .await
    }

    async fn say(&self, id: &str, text: &str) -> String {
        self.request("agent.send", json!({ "id": id, "text": text }))
            .await;
        self.wait(id).await;
        self.request("agent.read", json!({ "id": id })).await["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    async fn history(&self, params: Value) -> Vec<Value> {
        self.request("agent.history", params)
            .await
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

fn ids(entries: &[Value]) -> Vec<&str> {
    entries.iter().filter_map(|e| e["id"].as_str()).collect()
}

#[tokio::test]
async fn history_searches_past_sessions_and_resume_brings_one_back() {
    let mut world = World::new(&[]).await;
    let ws = world
        .workspace("login", json!({ "prompt": "Fix the login redirect" }))
        .await;
    let ws_id = ws["id"].as_str().expect("id").to_owned();
    let login = ws["agents"][0]["id"].as_str().expect("agent").to_owned();
    world.wait(&login).await;
    let work = world.home.path().join("scratch");
    std::fs::create_dir_all(&work).expect("scratch");
    let other = world
        .request(
            "agent.create",
            json!({ "cwd": work, "title": "Deps", "prompt": "Bump serde" }),
        )
        .await["id"]
        .as_str()
        .expect("id")
        .to_owned();
    world.wait(&other).await;

    assert!(
        world.history(json!({})).await.is_empty(),
        "running sessions are not history yet"
    );
    world
        .request("agent.kill", json!({ "id": other, "remove": true }))
        .await;
    world.restart().await;

    let all = world.history(json!({})).await;
    assert_eq!(ids(&all).len(), 2, "{all:?}");
    let entry = all
        .iter()
        .find(|e| e["id"] == login.as_str())
        .expect("login session");
    assert_eq!(entry["state"], "sleeping");
    assert_eq!(entry["first_prompt"], "Fix the login redirect");
    assert_eq!(entry["workspace"], ws_id.as_str());
    assert_eq!(entry["turns"], 1);
    assert_eq!(entry["resumable"], true);
    let removed = all
        .iter()
        .find(|e| e["id"] == other.as_str())
        .expect("killed session");
    assert_eq!(removed["state"], "killed");

    let found = world
        .history(json!({ "query": "ECHO: fix the login" }))
        .await;
    assert_eq!(ids(&found), [login.as_str()]);
    assert!(
        found[0]["snippet"]
            .as_str()
            .unwrap_or_default()
            .contains("echo: fix the login")
    );
    let by_workspace = world.history(json!({ "workspace": "login" })).await;
    assert_eq!(ids(&by_workspace), [login.as_str()]);
    assert!(
        world
            .history(json!({ "query": "no such words" }))
            .await
            .is_empty()
    );

    let resumed = world.request("agent.resume", json!({ "id": other })).await;
    assert_eq!(resumed["id"], other.as_str());
    let text = world.say(&other, "and tokio").await;
    assert!(
        text.contains("echo: Bump serde") && text.contains("echo: and tokio"),
        "{text}"
    );
    let resumed = world.request("agent.info", json!({ "id": other })).await;
    assert_eq!(resumed["state"], "idle");
    assert!(
        world
            .history(json!({}))
            .await
            .iter()
            .all(|e| e["id"] != other.as_str()),
        "a resumed session is live again"
    );

    let woken = world.request("agent.resume", json!({ "id": login })).await;
    assert_ne!(woken["state"], "sleeping");
    assert!(
        world
            .client
            .request("agent.resume", json!({ "id": "agent-missing" }))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn workspace_sleep_parks_agents_and_terminals_and_wake_restarts_them() {
    let world = World::new(&[]).await;
    let ws = world
        .workspace("sleepy", json!({ "prompt": "hello" }))
        .await;
    let ws_id = ws["id"].as_str().expect("id").to_owned();
    let agent = ws["agents"][0]["id"].as_str().expect("agent").to_owned();
    world.wait(&agent).await;
    let terminal = world
        .request(
            "terminal.create",
            json!({ "command": ["/bin/cat"], "cwd": ws["path"], "workspace": ws_id }),
        )
        .await["id"]
        .as_str()
        .expect("terminal")
        .to_owned();
    world
        .request(
            "terminal.write",
            json!({ "id": terminal, "text": "before-sleep\n" }),
        )
        .await;
    world
        .request(
            "terminal.wait",
            json!({ "id": terminal, "contains": "before-sleep", "timeout_ms": 10000 }),
        )
        .await;

    let slept = world
        .request("workspace.sleep", json!({ "id": ws_id }))
        .await;
    assert_eq!(slept["agents"], json!([agent]));
    assert_eq!(slept["terminals"], json!([terminal]));
    let info = world
        .request("workspace.info", json!({ "id": ws_id }))
        .await;
    assert_eq!(info["sleeping"], true);
    assert_eq!(info["agents"][0]["state"], "sleeping");
    assert_eq!(info["terminals"][0]["running"], false);
    let read = world
        .request(
            "terminal.read",
            json!({ "id": terminal, "scrollback": true }),
        )
        .await;
    assert!(
        read["text"]
            .as_str()
            .unwrap_or_default()
            .contains("before-sleep")
    );

    let woke = world
        .request("workspace.wake", json!({ "id": ws_id }))
        .await;
    assert_eq!(woke["agents"], json!([agent]));
    assert_eq!(woke["terminals"], json!([terminal]));
    let info = world
        .request("workspace.info", json!({ "id": ws_id }))
        .await;
    assert_eq!(info["sleeping"], false);
    assert_eq!(info["terminals"][0]["running"], true);
    assert_ne!(info["agents"][0]["state"], "sleeping");
    world
        .request(
            "terminal.write",
            json!({ "id": terminal, "text": "after-wake\n" }),
        )
        .await;
    let woken = world
        .request(
            "terminal.wait",
            json!({ "id": terminal, "contains": "after-wake", "timeout_ms": 10000 }),
        )
        .await;
    assert_eq!(woken["running"], true);
    let read = world
        .request(
            "terminal.read",
            json!({ "id": terminal, "scrollback": true }),
        )
        .await;
    assert!(
        read["text"]
            .as_str()
            .unwrap_or_default()
            .contains("before-sleep")
    );
    let text = world.say(&agent, "still there?").await;
    assert!(
        text.contains("echo: hello") && text.contains("echo: still there?"),
        "{text}"
    );
    world
        .request("terminal.kill", json!({ "id": terminal }))
        .await;
}

#[tokio::test]
async fn accounts_choose_the_config_dir_a_launch_uses_and_carry_usage() {
    let world = World::new(&[("ANTHROPIC_API_KEY", "sk-should-not-leak")]).await;
    let listed = world.request("account.list", json!({})).await;
    let system: Vec<&Value> = listed
        .as_array()
        .expect("list")
        .iter()
        .filter(|a| a["id"] == "system")
        .collect();
    assert_eq!(system.len(), 2, "claude and codex system logins");
    assert!(system.iter().all(|a| a["default"] == true));

    let work = world
        .request(
            "account.add",
            json!({ "provider": "claude", "label": "Work" }),
        )
        .await;
    let work_id = work["id"].as_str().expect("id").to_owned();
    let home = work["home"].as_str().expect("home").to_owned();
    assert_eq!(
        Path::new(&home),
        world
            .home
            .path()
            .join("claude-profiles")
            .join(&work_id)
            .join("home")
    );
    assert!(Path::new(&home).is_dir());
    assert_eq!(
        work["login_command"],
        format!("CLAUDE_CONFIG_DIR={home} {} /login", fake())
    );
    assert_eq!(work["logged_in"], false);
    assert!(
        world
            .client
            .request(
                "account.add",
                json!({ "provider": "claude", "label": "Work" })
            )
            .await
            .is_err(),
        "labels are unique per provider"
    );
    let scratch = world.home.path().join("scratch");
    std::fs::create_dir_all(&scratch).expect("scratch");
    let env_probe = "env:CLAUDE_CONFIG_DIR,ANTHROPIC_API_KEY";

    let system_agent = world
        .request(
            "agent.create",
            json!({ "cwd": scratch, "prompt": env_probe }),
        )
        .await["id"]
        .as_str()
        .expect("id")
        .to_owned();
    world.wait(&system_agent).await;
    let text = world
        .request("agent.read", json!({ "id": system_agent }))
        .await["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        text.contains("CLAUDE_CONFIG_DIR unset ANTHROPIC_API_KEY=sk-should-not-leak"),
        "the system login runs untouched: {text}"
    );

    world
        .request("account.select", json!({ "id": "Work" }))
        .await;
    let ws = world
        .workspace("with-account", json!({ "prompt": env_probe }))
        .await;
    let ws_agent = ws["agents"][0]["id"].as_str().expect("agent").to_owned();
    world.wait(&ws_agent).await;
    let text = world.request("agent.read", json!({ "id": ws_agent })).await["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        text.contains(&format!("CLAUDE_CONFIG_DIR={home} ANTHROPIC_API_KEY unset")),
        "the default account's dir, with auth env stripped: {text}"
    );
    let info = world.request("agent.info", json!({ "id": ws_agent })).await;
    assert_eq!(info["account"], work_id.as_str());

    let forced = world
        .request(
            "agent.create",
            json!({ "cwd": scratch, "prompt": env_probe, "account": "system" }),
        )
        .await["id"]
        .as_str()
        .expect("id")
        .to_owned();
    world.wait(&forced).await;
    let text = world.request("agent.read", json!({ "id": forced })).await["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(text.contains("CLAUDE_CONFIG_DIR unset"), "{text}");

    let listed = world
        .request("account.list", json!({ "provider": "claude" }))
        .await;
    let accounts = listed.as_array().expect("list");
    assert_eq!(accounts.len(), 2);
    let work = accounts
        .iter()
        .find(|a| a["id"] == work_id.as_str())
        .expect("work");
    assert_eq!(work["default"], true);
    assert_eq!(work["usage"]["agent_id"], ws_agent.as_str());
    assert_eq!(
        work["usage"]["rate_limits"]["unifiedWindows"]["five_hour"]["utilization"],
        0.1
    );
    let system = accounts
        .iter()
        .find(|a| a["id"] == "system")
        .expect("system");
    assert_eq!(system["default"], false);
    assert!(!system["usage"].is_null());

    std::fs::write(
        Path::new(&home).join(".claude.json"),
        json!({ "oauthAccount": { "emailAddress": "me@work.example" } }).to_string(),
    )
    .expect("state file");
    let listed = world
        .request("account.list", json!({ "provider": "claude" }))
        .await;
    let work = listed
        .as_array()
        .expect("list")
        .iter()
        .find(|a| a["id"] == work_id.as_str())
        .cloned()
        .expect("work");
    assert_eq!(work["logged_in"], true);
    assert_eq!(work["email"], "me@work.example");

    assert!(
        world
            .client
            .request(
                "agent.create",
                json!({ "cwd": scratch, "provider": "claude", "account": "nobody" })
            )
            .await
            .is_err()
    );
    let removed = world
        .request("account.remove", json!({ "id": work_id }))
        .await;
    assert_eq!(removed["removed_files"], true);
    assert!(!Path::new(&home).exists());
    let listed = world
        .request("account.list", json!({ "provider": "claude" }))
        .await;
    assert_eq!(listed.as_array().map(Vec::len), Some(1));
    assert_eq!(listed[0]["default"], true, "back to the system login");
}
