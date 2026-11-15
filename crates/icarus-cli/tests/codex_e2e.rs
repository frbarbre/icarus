use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
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

fn start_server(home: &Path) -> Guard {
    start_server_with(home, &sibling("icarus-fake-codex"))
}

fn start_server_with(home: &Path, codex: &Path) -> Guard {
    let child = Command::new(sibling("icarus-server"))
        .args(["--home", home.to_str().expect("utf8"), "serve"])
        .env("ICARUS_WORKSPACE_ROOT", home.join("workspaces"))
        .env("ICARUS_CODEX_BIN", codex)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("server");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !home.join("server.token").exists() {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    Guard(child)
}

struct Editor {
    _process: Guard,
    stdin: ChildStdin,
    lines: mpsc::Receiver<Value>,
    next_id: u64,
}

impl Editor {
    fn start(home: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_icarus"))
            .args([
                "--home",
                home.to_str().expect("utf8"),
                "acp",
                "--provider",
                "codex",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("acp bridge");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    let _ = tx.send(value);
                }
            }
        });
        Self {
            _process: Guard(child),
            stdin,
            lines,
            next_id: 1,
        }
    }

    fn call(&mut self, method: &str, params: Value) -> (Value, Vec<Value>) {
        self.call_within(method, params, Duration::from_secs(15))
    }

    fn call_within(&mut self, method: &str, params: Value, wait: Duration) -> (Value, Vec<Value>) {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(self.stdin, "{message}").expect("write");
        let mut notifications = Vec::new();
        let deadline = Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let message = self
                .lines
                .recv_timeout(remaining)
                .unwrap_or_else(|_| panic!("no reply to {method}"));
            if message["id"] == json!(id) {
                assert!(message["error"].is_null(), "{method} failed: {message}");
                return (message["result"].clone(), notifications);
            }
            notifications.push(message);
        }
    }
}

fn updates(notifications: &[Value]) -> Vec<&Value> {
    notifications
        .iter()
        .map(|n| &n["params"]["update"])
        .filter(|u| u.is_object())
        .collect()
}

#[test]
fn cli_creates_codex_agents_and_the_acp_bridge_renders_them() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let _server = start_server(home.path());

    let created = Command::new(env!("CARGO_BIN_EXE_icarus"))
        .args(["--home", home.path().to_str().expect("utf8"), "--json"])
        .args(["agent", "create", "--provider", "codex", "--cwd"])
        .arg(work.path())
        .args(["--prompt", "run:ls hello"])
        .output()
        .expect("icarus agent create");
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let info: Value = serde_json::from_slice(&created.stdout).expect("json info");
    assert_eq!(info["provider"], "codex");

    let mut editor = Editor::start(home.path());
    editor.call(
        "initialize",
        json!({ "protocolVersion": 1, "clientCapabilities": {} }),
    );
    let (new, _) = editor.call(
        "session/new",
        json!({ "cwd": work.path(), "mcpServers": [] }),
    );
    let session = new["sessionId"].as_str().expect("session").to_string();
    assert_ne!(
        session,
        info["id"].as_str().expect("id"),
        "the CLI agent already worked"
    );

    let target = work.path().join("out.txt");
    let (result, notifications) = editor.call(
        "session/prompt",
        json!({ "sessionId": session, "prompt": [{ "type": "text",
            "text": format!("plan:edit run:cat write:{} data", target.display()) }] }),
    );
    assert_eq!(result["stopReason"], "end_turn");
    assert_eq!(std::fs::read_to_string(&target).expect("written"), "data");
    let updates = updates(&notifications);
    let kinds: Vec<&str> = updates
        .iter()
        .filter_map(|u| u["sessionUpdate"].as_str())
        .collect();
    for kind in [
        "plan",
        "tool_call",
        "tool_call_update",
        "agent_message_chunk",
    ] {
        assert!(kinds.contains(&kind), "{kind} missing from {kinds:?}");
    }
    let execute = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "tool_call" && u["kind"] == "execute")
        .expect("command call");
    assert_eq!(execute["title"], "cat");
    let edit = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "tool_call" && u["kind"] == "edit")
        .expect("file change call");
    let edit_id = edit["toolCallId"].clone();
    let diff = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "tool_call_update" && u["toolCallId"] == edit_id)
        .expect("file change result");
    assert_eq!(diff["content"][0]["type"], "diff");
    assert_eq!(diff["content"][0]["newText"], "data");

    drop(editor);
    let mut editor = Editor::start(home.path());
    editor.call(
        "initialize",
        json!({ "protocolVersion": 1, "clientCapabilities": {} }),
    );
    let (_, replay) = editor.call(
        "session/load",
        json!({ "sessionId": session, "cwd": work.path(), "mcpServers": [] }),
    );
    let kinds: Vec<&str> = self::updates(&replay)
        .iter()
        .filter_map(|u| u["sessionUpdate"].as_str())
        .collect();
    assert_eq!(kinds.first(), Some(&"user_message_chunk"), "{kinds:?}");
    assert!(
        kinds.contains(&"tool_call") && kinds.contains(&"agent_message_chunk"),
        "{kinds:?}"
    );
}

fn reply_text(notifications: &[Value]) -> String {
    updates(notifications)
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|u| u["content"]["text"].as_str())
        .collect()
}

/// Needs a real `codex` signed in with `codex login`; costs two tiny turns.
/// `ICARUS_LIVE_CODEX=$(which codex) cargo nextest run -p icarus-cli --run-ignored only live_codex`
#[test]
#[ignore = "talks to the real Codex service"]
fn live_codex_streams_ends_turns_and_resumes_its_thread() {
    let codex =
        PathBuf::from(std::env::var("ICARUS_LIVE_CODEX").unwrap_or_else(|_| "codex".into()));
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(work.path())
        .status()
        .expect("git");
    assert!(git.success());
    let _server = start_server_with(home.path(), &codex);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (client, _) = runtime
        .block_on(icarus_client::Client::connect(home.path(), "test"))
        .expect("connect");
    let request = |method: &str, params: Value| {
        runtime
            .block_on(client.request(method, params))
            .unwrap_or_else(|e| panic!("{method}: {e:#}"))
    };

    let mut editor = Editor::start(home.path());
    editor.call(
        "initialize",
        json!({ "protocolVersion": 1, "clientCapabilities": {} }),
    );
    let (new, _) = editor.call_within(
        "session/new",
        json!({ "cwd": work.path(), "mcpServers": [] }),
        Duration::from_secs(60),
    );
    let session = new["sessionId"].as_str().expect("session").to_string();
    let prompt =
        |text: &str| json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] });

    let (result, notifications) = editor.call_within(
        "session/prompt",
        prompt("Reply with the single word: pong"),
        Duration::from_secs(180),
    );
    assert_eq!(result["stopReason"], "end_turn");
    let reply = reply_text(&notifications);
    assert!(reply.to_lowercase().contains("pong"), "{reply:?}");
    let info = request("agent.info", json!({ "id": session }));
    assert_eq!(info["provider"], "codex");
    let thread = info["provider_session"]
        .as_str()
        .expect("thread")
        .to_string();

    request("agent.sleep", json!({ "id": session }));
    let deadline = Instant::now() + Duration::from_secs(20);
    while request("agent.info", json!({ "id": session }))["state"] != "sleeping" {
        assert!(Instant::now() < deadline, "never slept");
        std::thread::sleep(Duration::from_millis(50));
    }

    let (result, notifications) = editor.call_within(
        "session/prompt",
        prompt("Reply with the single word: ping"),
        Duration::from_secs(180),
    );
    assert_eq!(result["stopReason"], "end_turn");
    let reply = reply_text(&notifications);
    assert!(reply.to_lowercase().contains("ping"), "{reply:?}");
    let info = request("agent.info", json!({ "id": session }));
    assert_eq!(
        info["provider_session"],
        thread.as_str(),
        "resumed the thread"
    );
    assert_eq!(info["turns"], 2);
    let events = request("agent.events", json!({ "id": session }));
    let kinds: Vec<&str> = events
        .as_array()
        .expect("events")
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect();
    for kind in ["session", "wake", "text_delta", "message", "turn_end"] {
        assert!(kinds.contains(&kind), "{kind} missing from {kinds:?}");
    }
    eprintln!("live codex events: {kinds:?}");

    drop(editor);
    let mut editor = Editor::start(home.path());
    editor.call(
        "initialize",
        json!({ "protocolVersion": 1, "clientCapabilities": {} }),
    );
    let (_, replay) = editor.call(
        "session/load",
        json!({ "sessionId": session, "cwd": work.path(), "mcpServers": [] }),
    );
    let replayed = reply_text(&replay).to_lowercase();
    assert!(
        replayed.contains("pong") && replayed.contains("ping"),
        "{replayed:?}"
    );
}
