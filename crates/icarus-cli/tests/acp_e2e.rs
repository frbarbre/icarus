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
    let child = Command::new(sibling("icarus-server"))
        .args(["--home", home.to_str().expect("utf8"), "serve"])
        .env("ICARUS_WORKSPACE_ROOT", home.join("workspaces"))
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
            .args(["--home", home.to_str().expect("utf8"), "acp"])
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

    /// Sends a request and collects every notification until its response arrives.
    fn call(&mut self, method: &str, params: Value) -> (Value, Vec<Value>) {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(self.stdin, "{message}").expect("write");
        let mut notifications = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(15);
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

#[test]
fn editor_threads_live_on_the_server_and_reload_after_the_editor_quits() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let _server = start_server(home.path());

    // The workspace's warm agent, as the server creates for every workspace; the fake binary stands in for claude.
    let fake = sibling("icarus-fake-agent");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let session = runtime.block_on(async {
        let (client, _) = icarus_client::Client::connect(home.path(), "test")
            .await
            .expect("connect");
        client
            .request(
                "agent.create",
                json!({ "cwd": work.path(), "command": [fake] }),
            )
            .await
            .expect("create fake agent")
    });
    let agent_id = session["id"].as_str().expect("id").to_string();

    let mut editor = Editor::start(home.path());
    let (init, _) = editor.call(
        "initialize",
        json!({ "protocolVersion": 1, "clientCapabilities": {} }),
    );
    assert_eq!(init["agentCapabilities"]["loadSession"], true);

    let (new, _) = editor.call(
        "session/new",
        json!({ "cwd": work.path(), "mcpServers": [] }),
    );
    assert_eq!(
        new["sessionId"],
        agent_id.as_str(),
        "an unused workspace agent is reused, so it is already warm"
    );

    let target = work.path().join("out.txt");
    let (result, updates) = editor.call(
        "session/prompt",
        json!({ "sessionId": agent_id, "prompt": [{ "type": "text", "text": format!("write:{} data", target.display()) }] }),
    );
    assert_eq!(result["stopReason"], "end_turn");
    let kinds: Vec<&str> = updates
        .iter()
        .filter_map(|u| u["params"]["update"]["sessionUpdate"].as_str())
        .collect();
    assert!(
        kinds.contains(&"tool_call")
            && kinds.contains(&"tool_call_update")
            && kinds.contains(&"agent_message_chunk"),
        "{kinds:?}"
    );
    assert_eq!(std::fs::read_to_string(&target).expect("written"), "data");

    drop(editor);
    let mut editor = Editor::start(home.path());
    editor.call(
        "initialize",
        json!({ "protocolVersion": 1, "clientCapabilities": {} }),
    );
    let (listed, _) = editor.call("session/list", json!({}));
    assert!(
        listed["sessions"]
            .as_array()
            .expect("sessions")
            .iter()
            .any(|s| s["sessionId"] == agent_id.as_str())
    );
    let (_, replay) = editor.call(
        "session/load",
        json!({ "sessionId": agent_id, "cwd": work.path(), "mcpServers": [] }),
    );
    let kinds: Vec<&str> = replay
        .iter()
        .filter_map(|u| u["params"]["update"]["sessionUpdate"].as_str())
        .collect();
    assert_eq!(kinds.first(), Some(&"user_message_chunk"), "{kinds:?}");
    assert!(
        kinds.contains(&"tool_call") && kinds.contains(&"agent_message_chunk"),
        "{kinds:?}"
    );

    let (result, _) = editor.call(
        "session/prompt",
        json!({ "sessionId": agent_id, "prompt": [{ "type": "text", "text": "again" }] }),
    );
    assert_eq!(result["stopReason"], "end_turn");
}

#[test]
fn each_new_editor_tab_gets_its_own_warm_agent() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let _server = start_server(home.path());
    let fake = sibling("icarus-fake-agent");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let created: Vec<String> = runtime.block_on(async {
        let (client, _) = icarus_client::Client::connect(home.path(), "test")
            .await
            .expect("connect");
        let mut ids = Vec::new();
        for _ in 0..2 {
            let agent = client
                .request(
                    "agent.create",
                    json!({ "cwd": work.path(), "command": [fake.clone()] }),
                )
                .await
                .expect("create fake agent");
            ids.push(agent["id"].as_str().expect("id").to_string());
        }
        ids
    });

    let mut editor = Editor::start(home.path());
    editor.call(
        "initialize",
        json!({ "protocolVersion": 1, "clientCapabilities": {} }),
    );
    let mut sessions = Vec::new();
    for _ in 0..2 {
        let (new, _) = editor.call(
            "session/new",
            json!({ "cwd": work.path(), "mcpServers": [] }),
        );
        sessions.push(new["sessionId"].as_str().expect("session").to_string());
    }
    assert_ne!(sessions[0], sessions[1], "two tabs never share one agent");
    for session in &sessions {
        assert!(
            created.contains(session),
            "both reuse warm agents: {session}"
        );
    }
}
