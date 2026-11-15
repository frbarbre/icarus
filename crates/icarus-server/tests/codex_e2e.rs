use std::path::Path;
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

/// A server whose `codex` is the fake app-server, set the way a user points Icarus at another binary.
fn start_server(home: &Path) -> ServerProcess {
    let _ = std::fs::remove_file(home.join("server.token"));
    let child = Command::new(env!("CARGO_BIN_EXE_icarus-server"))
        .args(["--home", home.to_str().expect("utf8 home"), "serve"])
        .env("ICARUS_WORKSPACE_ROOT", home.join("workspaces"))
        .env("ICARUS_CODEX_BIN", env!("CARGO_BIN_EXE_icarus-fake-codex"))
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

async fn create(client: &Client, cwd: &Path, extra: Value) -> String {
    let mut params = json!({ "cwd": cwd, "provider": "codex", "title": "Codex" });
    for (key, value) in extra.as_object().into_iter().flatten() {
        params[key] = value.clone();
    }
    let info = client
        .request("agent.create", params)
        .await
        .expect("create codex agent");
    assert_eq!(info["provider"], "codex");
    info["id"].as_str().expect("id").to_string()
}

async fn wait_idle(client: &Client, id: &str) -> Value {
    client
        .request("agent.wait", json!({ "id": id, "timeout_ms": 10000 }))
        .await
        .expect("turn finishes")
}

async fn events(client: &Client, id: &str) -> Vec<Value> {
    client
        .request("agent.events", json!({ "id": id }))
        .await
        .expect("events")
        .as_array()
        .expect("array")
        .clone()
}

fn kinds(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|e| e["kind"].as_str().unwrap_or_default())
        .collect()
}

async fn wait_for_state(client: &Client, id: &str, state: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let info = client
            .request("agent.info", json!({ "id": id }))
            .await
            .expect("info");
        if info["state"] == state {
            return;
        }
        assert!(Instant::now() < deadline, "never {state}: {info}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn codex_turns_stream_text_tools_plans_and_usage() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let _server = start_server(home.path());
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");

    let target = work.path().join("src/note.txt");
    let id = create(
        &client,
        work.path(),
        json!({ "prompt": format!("plan:read,write run:ls write:{} from-codex", target.display()) }),
    )
    .await;
    let done = wait_idle(&client, &id).await;
    assert!(
        done["provider_session"]
            .as_str()
            .is_some_and(|s| s.starts_with("fake-thread-")),
        "{done}"
    );
    assert_eq!(done["model"], "fake-codex-model");
    assert_eq!(done["turns"], 1);
    assert_eq!(
        std::fs::read_to_string(&target).expect("written under bypass"),
        "from-codex"
    );

    let events = events(&client, &id).await;
    let kinds = kinds(&events);
    for kind in [
        "session",
        "user_message",
        "plan",
        "tool_call",
        "tool_result",
        "text_delta",
        "message",
        "usage",
        "rate_limit",
        "turn_end",
    ] {
        assert!(kinds.contains(&kind), "{kind} missing from {kinds:?}");
    }
    let tools: Vec<&str> = events
        .iter()
        .filter(|e| e["kind"] == "tool_call")
        .map(|e| e["data"]["name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(tools, ["Bash", "apply_patch"]);
    let patch = events
        .iter()
        .find(|e| e["kind"] == "tool_result" && e["data"]["name"] == "apply_patch")
        .expect("file change result");
    assert_eq!(
        patch["data"]["input"]["changes"][0]["path"],
        target.to_string_lossy().as_ref()
    );
    let plan = events.iter().find(|e| e["kind"] == "plan").expect("plan");
    assert_eq!(plan["data"]["todos"][0]["status"], "in_progress");
    let end = events
        .iter()
        .rfind(|e| e["kind"] == "turn_end")
        .expect("end");
    assert_eq!(end["data"]["is_error"], false);

    let transcript = client
        .request("agent.read", json!({ "id": id }))
        .await
        .expect("read");
    let text = transcript["text"].as_str().expect("text");
    assert!(
        text.contains("[Bash]") && text.contains(&format!("echo: wrote {}", target.display())),
        "{text}"
    );

    client
        .request("agent.send", json!({ "id": id, "text": "fail now" }))
        .await
        .expect("send");
    wait_idle(&client, &id).await;
    let events = self::events(&client, &id).await;
    let end = events
        .iter()
        .rfind(|e| e["kind"] == "turn_end")
        .expect("end");
    assert_eq!(end["data"]["is_error"], true);
    assert_eq!(end["data"]["result"], "fake failure");
}

#[tokio::test]
async fn codex_interrupt_sleep_and_resume_keep_the_thread() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let _server = start_server(home.path());
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
    let id = create(&client, work.path(), json!({})).await;
    wait_for_state(&client, &id, "idle").await;

    client
        .request(
            "agent.send",
            json!({ "id": id, "text": "sleep:20000 slow" }),
        )
        .await
        .expect("send");
    let started = Instant::now();
    client
        .request("agent.interrupt", json!({ "id": id }))
        .await
        .expect("interrupt");
    let after = wait_idle(&client, &id).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(after["state"], "idle");
    let end = self::events(&client, &id)
        .await
        .into_iter()
        .rfind(|e| e["kind"] == "turn_end")
        .expect("end");
    assert_eq!(end["data"]["subtype"], "interrupted");
    assert_eq!(end["data"]["is_error"], false);

    let thread = after["provider_session"]
        .as_str()
        .expect("thread")
        .to_string();
    client
        .request("agent.sleep", json!({ "id": id }))
        .await
        .expect("sleep");
    wait_for_state(&client, &id, "sleeping").await;
    client
        .request("agent.send", json!({ "id": id, "text": "awake" }))
        .await
        .expect("wake");
    let woke = wait_idle(&client, &id).await;
    assert_eq!(
        woke["provider_session"],
        thread.as_str(),
        "resumed the thread"
    );
    let events = self::events(&client, &id).await;
    assert!(kinds(&events).contains(&"wake"));
    let transcript = client
        .request("agent.read", json!({ "id": id }))
        .await
        .expect("read");
    assert!(
        transcript["text"]
            .as_str()
            .is_some_and(|t| t.contains("echo: awake")),
        "{transcript}"
    );

    client
        .request("agent.kill", json!({ "id": id }))
        .await
        .expect("kill");
    wait_for_state(&client, &id, "exited").await;
}

#[tokio::test]
async fn codex_agents_survive_a_server_restart() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let server = start_server(home.path());
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
    let id = create(&client, work.path(), json!({ "prompt": "first" })).await;
    let thread = wait_idle(&client, &id).await["provider_session"]
        .as_str()
        .expect("thread")
        .to_string();
    drop(client);
    drop(server);

    let _server = start_server(home.path());
    let (client, _events) = Client::connect(home.path(), "test")
        .await
        .expect("reconnect");
    let restored = client
        .request("agent.info", json!({ "id": id }))
        .await
        .expect("restored");
    assert_eq!(restored["state"], "sleeping");
    assert_eq!(restored["provider"], "codex");
    client
        .request("agent.send", json!({ "id": id, "text": "second" }))
        .await
        .expect("send wakes the agent");
    let after = wait_idle(&client, &id).await;
    assert_eq!(after["provider_session"], thread.as_str());
    assert_eq!(after["turns"], 2);
    let text = client
        .request("agent.read", json!({ "id": id }))
        .await
        .expect("read")["text"]
        .as_str()
        .expect("text")
        .to_string();
    assert!(
        text.contains("echo: first") && text.contains("echo: second"),
        "{text}"
    );
}

#[tokio::test]
async fn codex_without_bypass_runs_read_only_and_declines_writes() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let _server = start_server(home.path());
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
    let target = work.path().join("blocked.txt");
    let id = create(
        &client,
        work.path(),
        json!({ "bypass_permissions": false, "prompt": format!("write:{} nope", target.display()) }),
    )
    .await;
    wait_idle(&client, &id).await;
    assert!(!target.exists(), "a read-only sandbox writes nothing");
    let result = self::events(&client, &id)
        .await
        .into_iter()
        .find(|e| e["kind"] == "tool_result")
        .expect("file change result");
    assert_eq!(result["data"]["is_error"], true);
}
