use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use icarus_client::Client;
use serde_json::{Value, json};

fn sibling(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_BIN_EXE_icarus")).with_file_name(name);
    assert!(
        path.exists(),
        "{name} not built; run tests for the whole workspace"
    );
    path
}

/// A home whose server runs detached, as `icarus server start` leaves it.
struct Home {
    dir: tempfile::TempDir,
    config: PathBuf,
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.icarus(&["server", "stop"]);
        std::thread::sleep(Duration::from_millis(200));
        if let Some(pid) = self.server_pid() {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
    }
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("home");
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).expect("config");
        std::fs::write(
            config.join("settings.json"),
            json!({ "agents": { "commands": { "claude": [sibling("icarus-fake-agent")] } } })
                .to_string(),
        )
        .expect("settings");
        let home = Self { dir, config };
        home.ok(&["server", "start"]);
        home
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn icarus(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_icarus"))
            .arg("--home")
            .arg(self.path())
            .args(args)
            .env("ICARUS_WORKSPACE_ROOT", self.path().join("workspaces"))
            .env("ICARUS_CONFIG_DIR", &self.config)
            .env("ICARUS_GH_FIXTURES", self.path().join("no-gh"))
            .env("ICARUS_HANDOFF_HOLD_SECS", "30")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("ANTHROPIC_API_KEY")
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

    fn server_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.path().join("server.pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    async fn client(
        &self,
    ) -> (
        Arc<Client>,
        tokio::sync::mpsc::UnboundedReceiver<icarus_client::Event>,
    ) {
        Client::connect(self.path(), "test").await.expect("connect")
    }

    /// `icarus server restart`, returning the new server's pid.
    fn restart(&self) -> u64 {
        let output = self.ok(&["--json", "server", "restart"]);
        let value: Value = serde_json::from_str(&output).expect("json");
        assert_eq!(value["restarted"], true, "{value}");
        assert_ne!(value["pid"], value["old_pid"], "a new server process");
        value["pid"].as_u64().expect("pid")
    }

    fn wait_until_stopped(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.path().join("server.sock").exists() {
            assert!(Instant::now() < deadline, "server did not stop");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

async fn request(client: &Client, method: &str, params: Value) -> Value {
    client
        .request(method, params)
        .await
        .unwrap_or_else(|e| panic!("{method}: {e:#}"))
}

async fn wait_for(client: &Client, id: &str, needle: &str) {
    request(
        client,
        "terminal.wait",
        json!({ "id": id, "contains": needle, "timeout_ms": 15000 }),
    )
    .await;
}

fn highest_tick(text: &str) -> u64 {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("tick ")?.parse().ok())
        .max()
        .unwrap_or(0)
}

fn line_count(path: &Path) -> usize {
    std::fs::read_to_string(path).map_or(0, |text| text.lines().count())
}

fn alive(pid: u64) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

#[tokio::test]
async fn a_terminal_keeps_its_process_output_and_sequence_across_server_restart() {
    let home = Home::new();
    let beats = home.path().join("beats.log");
    let (client, mut events) = home.client().await;
    // A counter on the terminal, a background job writing a file each second, and `cat` for input.
    let script = "(n=0; while :; do n=$((n+1)); echo \"tick $n\"; sleep 0.2; done) & \
                  (while :; do date +%s >> \"$1\"; sleep 1; done) & exec cat";
    let info = request(
        &client,
        "terminal.create",
        json!({ "command": ["/bin/bash", "-c", script, "bash", beats], "cwd": "/tmp", "rows": 10 }),
    )
    .await;
    let id = info["id"].as_str().expect("id").to_owned();
    let pid = info["pid"].as_u64().expect("pid");
    request(&client, "terminal.attach", json!({ "id": id })).await;
    wait_for(&client, &id, "tick 12").await;

    // What the attached client has seen when the server goes away.
    let mut seen = 0;
    while let Ok(event) = events.try_recv() {
        if event.topic == "terminal.output" {
            seen = event.payload["seq"].as_u64().expect("seq") + 1;
        }
    }
    assert!(seen > 0, "the client saw output");
    let ticks_before = highest_tick(
        request(
            &client,
            "terminal.read",
            json!({ "id": id, "scrollback": true }),
        )
        .await["text"]
            .as_str()
            .expect("text"),
    );
    let beats_before = line_count(&beats);
    drop(events);
    drop(client);

    home.restart();

    let (client, _events) = home.client().await;
    let listed = request(&client, "terminal.list", json!({})).await;
    let terminal = listed
        .as_array()
        .expect("list")
        .iter()
        .find(|t| t["id"] == id.as_str())
        .unwrap_or_else(|| panic!("terminal {id} listed after restart: {listed}"))
        .clone();
    assert_eq!(terminal["pid"].as_u64(), Some(pid), "same process");
    assert_eq!(terminal["running"], true);
    assert_eq!(terminal["lost"], false);
    assert!(alive(pid));

    // Output continues past the restart.
    wait_for(&client, &id, &format!("tick {}", ticks_before + 10)).await;

    // The client resumes by sequence: no snapshot, and nothing missing from where it stopped.
    let resumed = request(
        &client,
        "terminal.attach",
        json!({ "id": id, "since_seq": seen }),
    )
    .await;
    assert!(
        resumed["snapshot"].is_null(),
        "resumed by sequence: {resumed}"
    );
    let chunks = resumed["chunks"].as_array().expect("chunks");
    assert_eq!(
        chunks.first().map(|c| c["seq"].as_u64()),
        Some(Some(seen)),
        "{resumed}"
    );
    let seqs: Vec<u64> = chunks.iter().filter_map(|c| c["seq"].as_u64()).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
    let replay: Vec<u8> = chunks
        .iter()
        .flat_map(|c| {
            base64::engine::general_purpose::STANDARD
                .decode(c["data"].as_str().unwrap_or_default())
                .expect("base64")
        })
        .collect();
    assert!(String::from_utf8_lossy(&replay).contains("tick"));

    // The scrollback from before the restart is still there.
    let full = request(
        &client,
        "terminal.read",
        json!({ "id": id, "scrollback": true }),
    )
    .await;
    let full = full["text"].as_str().expect("text");
    assert!(full.contains("tick 1\n"), "{full}");

    // Input still reaches the program.
    request(
        &client,
        "terminal.write",
        json!({ "id": id, "text": "hello-after\n" }),
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = request(
            &client,
            "terminal.read",
            json!({ "id": id, "scrollback": true }),
        )
        .await;
        if text["text"]
            .as_str()
            .unwrap_or_default()
            .matches("hello-after")
            .count()
            >= 2
        {
            break;
        }
        assert!(Instant::now() < deadline, "input was not echoed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The background job kept writing throughout.
    let deadline = Instant::now() + Duration::from_secs(10);
    while line_count(&beats) < beats_before + 2 {
        assert!(Instant::now() < deadline, "the background job stopped");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    request(&client, "terminal.kill", json!({ "id": id })).await;
    request(
        &client,
        "terminal.wait",
        json!({ "id": id, "exit": true, "timeout_ms": 10000 }),
    )
    .await;
}

#[tokio::test]
async fn agents_sleep_and_resume_across_server_restart() {
    let home = Home::new();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).expect("work");
    let (client, _events) = home.client().await;
    let info = request(
        &client,
        "agent.create",
        json!({ "cwd": work, "title": "Fixer", "prompt": "hello" }),
    )
    .await;
    let id = info["id"].as_str().expect("id").to_owned();
    request(
        &client,
        "agent.wait",
        json!({ "id": id, "timeout_ms": 10000 }),
    )
    .await;
    drop(client);

    home.restart();

    let (client, _events) = home.client().await;
    let listed = request(&client, "agent.list", json!({})).await;
    let agent = listed
        .as_array()
        .expect("list")
        .iter()
        .find(|a| a["id"] == id.as_str())
        .unwrap_or_else(|| panic!("agent {id} listed after restart: {listed}"))
        .clone();
    assert_eq!(agent["state"], "sleeping", "{agent}");
    request(&client, "agent.send", json!({ "id": id, "text": "again" })).await;
    request(
        &client,
        "agent.wait",
        json!({ "id": id, "timeout_ms": 10000 }),
    )
    .await;
    let text = request(&client, "agent.read", json!({ "id": id })).await;
    let text = text["text"].as_str().expect("text");
    assert!(
        text.contains("echo: hello") && text.contains("echo: again"),
        "{text}"
    );
}

#[tokio::test]
async fn sigterm_hands_terminals_on_and_a_crash_reports_them_lost() {
    let home = Home::new();
    let (client, _events) = home.client().await;
    let info = request(
        &client,
        "terminal.create",
        json!({ "command": ["/bin/cat"], "cwd": "/tmp" }),
    )
    .await;
    let id = info["id"].as_str().expect("id").to_owned();
    let pid = info["pid"].as_u64().expect("pid");
    request(
        &client,
        "terminal.write",
        json!({ "id": id, "text": "before\n" }),
    )
    .await;
    wait_for(&client, &id, "before").await;
    drop(client);

    // A service manager restarting the server: SIGTERM, then a fresh start.
    let server = home.server_pid().expect("server pid");
    Command::new("kill")
        .args(["-TERM", &server.to_string()])
        .status()
        .expect("kill");
    home.wait_until_stopped();
    home.ok(&["server", "start"]);
    let (client, _events) = home.client().await;
    let adopted = request(&client, "terminal.info", json!({ "id": id })).await;
    assert_eq!(adopted["pid"].as_u64(), Some(pid));
    assert_eq!(adopted["running"], true);
    assert!(
        request(&client, "terminal.read", json!({ "id": id })).await["text"]
            .as_str()
            .unwrap_or_default()
            .contains("before")
    );
    drop(client);

    // A crash can't hand anything on: the terminal ends with the server and is listed as lost.
    let server = home.server_pid().expect("server pid");
    Command::new("kill")
        .args(["-9", &server.to_string()])
        .status()
        .expect("kill");
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(Instant::now() < deadline, "cat outlived its PTY");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    home.ok(&["server", "start"]);
    let (client, _events) = home.client().await;
    let lost = request(&client, "terminal.info", json!({ "id": id })).await;
    assert_eq!(lost["lost"], true, "{lost}");
    assert_eq!(lost["running"], false);

    // `server stop` is deliberate: the next server starts clean.
    request(&client, "server.shutdown", json!({})).await;
    drop(client);
    home.wait_until_stopped();
    home.ok(&["server", "start"]);
    let (client, _events) = home.client().await;
    assert_eq!(
        request(&client, "terminal.list", json!({})).await,
        json!([])
    );
}
