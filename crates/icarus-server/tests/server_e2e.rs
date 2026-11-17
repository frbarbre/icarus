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
    // A killed server leaves its socket and token behind; the token is rewritten only after a new bind.
    let _ = std::fs::remove_file(home.join("server.token"));
    let child = Command::new(env!("CARGO_BIN_EXE_icarus-server"))
        .args(["--home", home.to_str().expect("utf8 home"), "serve"])
        .env("ICARUS_WORKSPACE_ROOT", home.join("workspaces"))
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

#[tokio::test]
async fn a_terminal_reports_a_running_command_as_busy_and_can_be_renamed() {
    let home = tempfile::tempdir().expect("tempdir");
    let _server = start_server(home.path());
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
    let info = client
        .request(
            "terminal.create",
            json!({ "command": ["/bin/sh"], "cwd": "/tmp" }),
        )
        .await
        .expect("create");
    let id = info["id"].as_str().expect("id").to_string();
    async fn wait_for_busy(client: &Client, id: &str, want: bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let info = client
                .request("terminal.info", json!({ "id": id }))
                .await
                .expect("info");
            if info["busy"].as_bool() == Some(want) {
                return;
            }
            assert!(Instant::now() < deadline, "busy never became {want}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    wait_for_busy(&client, &id, false).await;
    client
        .request("terminal.write", json!({ "id": id, "text": "sleep 30\r" }))
        .await
        .expect("write");
    wait_for_busy(&client, &id, true).await;
    client
        .request("terminal.write", json!({ "id": id, "text": "\u{3}" }))
        .await
        .expect("interrupt");
    wait_for_busy(&client, &id, false).await;

    let renamed = client
        .request("terminal.set_label", json!({ "id": id, "label": "Run" }))
        .await
        .expect("rename");
    assert_eq!(renamed["label"], "Run");
}

async fn next_event(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<icarus_client::Event>,
    topic: &str,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline.into(), events.recv())
            .await
            .unwrap_or_else(|_| panic!("no {topic} event in time"))
            .expect("event stream open");
        if event.topic == topic {
            return event.payload;
        }
    }
}

#[tokio::test]
async fn unattached_clients_hear_keyboard_mode_and_label_changes() {
    let home = tempfile::tempdir().expect("tempdir");
    let _server = start_server(home.path());
    let (client, mut events) = Client::connect(home.path(), "test").await.expect("connect");
    let info = client
        .request(
            "terminal.create",
            json!({ "command": ["/bin/cat"], "cwd": "/tmp" }),
        )
        .await
        .expect("create");
    let id = info["id"].as_str().expect("id").to_string();
    assert_eq!(info["kitty_keyboard_flags"], 0);
    assert_eq!(info["task"], false);

    client
        .request(
            "terminal.write",
            json!({ "id": id, "text": "\u{1b}[>1u\n" }),
        )
        .await
        .expect("write");
    let keyboard = next_event(&mut events, "terminal.keyboard").await;
    assert_eq!(keyboard, json!({ "id": id, "flags": 1 }));
    let info = client
        .request("terminal.info", json!({ "id": id }))
        .await
        .expect("info");
    assert_eq!(info["kitty_keyboard_flags"], 1);

    client
        .request("terminal.set_label", json!({ "id": id, "label": "Logs" }))
        .await
        .expect("rename");
    let label = next_event(&mut events, "terminal.label").await;
    assert_eq!(label, json!({ "id": id, "label": "Logs" }));
    client
        .request("terminal.write", json!({ "id": id, "text": "\u{1b}[<u\n" }))
        .await
        .expect("write");
    let keyboard = next_event(&mut events, "terminal.keyboard").await;
    assert_eq!(keyboard["flags"], 0);
}

#[tokio::test]
async fn a_task_terminal_runs_its_command_and_is_listed_as_a_task() {
    let home = tempfile::tempdir().expect("tempdir");
    let _server = start_server(home.path());
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
    let info = client
        .request(
            "terminal.create",
            json!({
                "command": ["/bin/sh", "-c", "echo task-ran-$GREETING; exit 4"],
                "env": { "GREETING": "hi" },
                "cwd": "/tmp",
                "label": "greet",
                "task": true,
            }),
        )
        .await
        .expect("create");
    let id = info["id"].as_str().expect("id").to_string();
    let done = client
        .request(
            "terminal.wait",
            json!({ "id": id, "exit": true, "timeout_ms": 10_000 }),
        )
        .await
        .expect("wait");
    assert_eq!(done["exit_code"], 4);
    let listed = client
        .request("terminal.list", json!({}))
        .await
        .expect("list");
    let task = listed
        .as_array()
        .and_then(|items| items.iter().find(|item| item["id"] == json!(id)))
        .expect("listed");
    assert_eq!(task["task"], true);
    assert_eq!(task["label"], "greet");
    let read = client
        .request("terminal.read", json!({ "id": id }))
        .await
        .expect("read");
    assert!(read["text"].as_str().unwrap_or("").contains("task-ran-hi"));
}

#[tokio::test]
async fn terminal_outlives_its_client_and_resumes_by_sequence() {
    let home = tempfile::tempdir().expect("tempdir");
    let _server = start_server(home.path());

    let (client, mut events) = Client::connect(home.path(), "test").await.expect("connect");
    let info = client
        .request(
            "terminal.create",
            json!({ "command": ["/bin/cat"], "cwd": "/tmp" }),
        )
        .await
        .expect("create");
    let id = info["id"].as_str().expect("id").to_string();
    let attached = client
        .request("terminal.attach", json!({ "id": id }))
        .await
        .expect("attach");
    assert!(attached["snapshot"].is_string());

    client
        .request("terminal.write", json!({ "id": id, "text": "before\n" }))
        .await
        .expect("write");
    let mut last_seq: u64;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline.into(), events.recv())
            .await
            .expect("output event in time")
            .expect("event stream open");
        if event.topic == "terminal.output" {
            last_seq = event.payload["seq"].as_u64().expect("seq") + 1;
            let read = client
                .request("terminal.read", json!({ "id": id }))
                .await
                .expect("read");
            if read["text"].as_str().unwrap_or("").contains("before") {
                break;
            }
        }
    }
    drop(events);
    drop(client);

    // The client is gone; the program keeps running and producing output.
    let (client, _events) = Client::connect(home.path(), "test-2")
        .await
        .expect("reconnect");
    client
        .request("terminal.write", json!({ "id": id, "text": "after\n" }))
        .await
        .expect("write after reconnect");
    client
        .request(
            "terminal.wait",
            json!({ "id": id, "contains": "after", "timeout_ms": 10000 }),
        )
        .await
        .expect("wait");
    let resumed = client
        .request(
            "terminal.attach",
            json!({ "id": id, "since_seq": last_seq }),
        )
        .await
        .expect("resume");
    assert!(
        resumed["snapshot"].is_null(),
        "should resume without a snapshot"
    );
    let listed = client
        .request("terminal.list", json!({}))
        .await
        .expect("list");
    assert_eq!(listed[0]["running"], json!(true));

    client
        .request("terminal.kill", json!({ "id": id, "remove": true }))
        .await
        .expect("kill");
}

#[tokio::test]
async fn agent_keeps_working_without_a_client_and_replays_by_sequence() {
    let home = tempfile::tempdir().expect("tempdir");
    let work = tempfile::tempdir().expect("workdir");
    let _server = start_server(home.path());
    let fake = env!("CARGO_BIN_EXE_icarus-fake-agent");

    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
    let info = client
        .request(
            "agent.create",
            json!({ "cwd": work.path(), "command": [fake], "title": "Fix CI", "prompt": "hello" }),
        )
        .await
        .expect("create agent");
    let id = info["id"].as_str().expect("id").to_string();
    client
        .request("agent.wait", json!({ "id": id, "timeout_ms": 10000 }))
        .await
        .expect("first turn finishes");

    // Start a slow turn, then disconnect while it runs.
    let target = work.path().join("note.txt");
    client
        .request(
            "agent.send",
            json!({ "id": id, "text": format!("sleep:300 write:{} from-agent", target.display()) }),
        )
        .await
        .expect("send");
    let seen = client
        .request("agent.events", json!({ "id": id }))
        .await
        .expect("events")
        .as_array()
        .expect("array")
        .len() as u64;
    drop(client);

    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        std::fs::read_to_string(&target).expect("agent wrote file"),
        "from-agent"
    );

    let (client, mut events) = Client::connect(home.path(), "test-2")
        .await
        .expect("reconnect");
    let attached = client
        .request("agent.attach", json!({ "id": id, "since": seen }))
        .await
        .expect("attach");
    let kinds: Vec<&str> = attached["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|e| e["kind"].as_str().unwrap_or_default())
        .collect();
    assert!(kinds.contains(&"tool_call"), "{kinds:?}");
    assert!(kinds.contains(&"tool_result"), "{kinds:?}");
    assert!(kinds.contains(&"turn_end"), "{kinds:?}");
    assert_eq!(attached["info"]["state"], "idle");
    assert_eq!(
        attached["info"]["rate_limits"]["unifiedWindows"]["five_hour"]["utilization"],
        0.1
    );

    // Live events flow to the attached client.
    client
        .request("agent.send", json!({ "id": id, "text": "again" }))
        .await
        .expect("send again");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline.into(), events.recv())
            .await
            .expect("live event in time")
            .expect("open stream");
        if event.topic == "agent.event" && event.payload["event"]["kind"] == "turn_end" {
            break;
        }
    }
    let transcript = client
        .request("agent.read", json!({ "id": id }))
        .await
        .expect("read");
    let text = transcript["text"].as_str().expect("text");
    assert!(
        text.contains("> hello") && text.contains("echo: again"),
        "{text}"
    );
    assert!(
        home.path()
            .join("threads")
            .join(format!("{id}.jsonl"))
            .exists()
    );
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

#[tokio::test]
async fn project_inspect_reports_what_add_would_store_without_adding() {
    let home = tempfile::tempdir().expect("tempdir");
    let repo = tempfile::tempdir().expect("repo");
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "T"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::create_dir(repo.path().join("src")).expect("mkdir");
    std::fs::write(repo.path().join("src/lib.rs"), "\n").expect("write");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    let _server = start_server(home.path());
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");

    let inspected = client
        .request(
            "project.inspect",
            json!({ "path": repo.path().join("src") }),
        )
        .await
        .expect("inspect");
    let root = std::fs::canonicalize(repo.path()).expect("canonical");
    assert_eq!(
        std::fs::canonicalize(inspected["root"].as_str().expect("root")).expect("canonical"),
        root,
        "a subfolder resolves to the repository root"
    );
    assert_eq!(inspected["base_ref"], "main");
    assert_eq!(inspected["existing"], serde_json::Value::Null);
    assert_eq!(
        client
            .request("project.list", json!({}))
            .await
            .expect("list"),
        json!([]),
        "inspecting adds nothing"
    );

    let added = client
        .request(
            "project.add",
            json!({ "path": inspected["root"], "name": "", "base_ref": "" }),
        )
        .await
        .expect("add");
    assert_eq!(
        added["name"], inspected["name"],
        "a blank name means the default"
    );
    assert_eq!(
        added["base_ref"], "main",
        "a blank base means the detected one"
    );
    let again = client
        .request("project.inspect", json!({ "path": repo.path() }))
        .await
        .expect("inspect again");
    assert_eq!(again["existing"], added["id"]);

    let error = client
        .request("project.inspect", json!({ "path": home.path() }))
        .await
        .expect_err("not a repository");
    assert!(format!("{error:#}").contains("not_a_repo"), "{error:#}");
}

#[tokio::test]
async fn a_terminal_created_with_a_workspace_name_belongs_to_it_and_starts_in_its_folder() {
    let home = tempfile::tempdir().expect("tempdir");
    let repo = tempfile::tempdir().expect("repo");
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "T"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.path().join("readme.md"), "hello\n").expect("write");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    let _server = start_server(home.path());
    let fake = env!("CARGO_BIN_EXE_icarus-fake-agent");
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
    let project = client
        .request("project.add", json!({ "path": repo.path() }))
        .await
        .expect("add project");
    let workspace = client
        .request(
            "workspace.create",
            json!({ "project": project["id"], "name": "shells", "agent_command": [fake] }),
        )
        .await
        .expect("create workspace");

    let terminal = client
        .request(
            "terminal.create",
            json!({ "workspace": "shells", "command": ["sh", "-c", "pwd; sleep 30"] }),
        )
        .await
        .expect("create terminal");
    let info = client
        .request("workspace.info", json!({ "id": workspace["id"] }))
        .await
        .expect("workspace info");
    let listed: Vec<&Value> = info["terminals"]
        .as_array()
        .expect("terminals")
        .iter()
        .collect();
    assert!(
        listed.iter().any(|t| t["id"] == terminal["id"]),
        "the workspace lists the terminal: {info}"
    );
    let path = std::fs::canonicalize(workspace["path"].as_str().expect("path")).expect("path");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let read = client
            .request("terminal.read", json!({ "id": terminal["id"] }))
            .await
            .expect("read");
        if read.to_string().contains(path.to_str().expect("utf8")) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "pwd was {read}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn workspace_lifecycle_creates_a_worktree_with_an_agent_and_lists_changes() {
    let home = tempfile::tempdir().expect("tempdir");
    let repo = tempfile::tempdir().expect("repo");
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "T"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.path().join("readme.md"), "hello\n").expect("write");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    let _server = start_server(home.path());
    let fake = env!("CARGO_BIN_EXE_icarus-fake-agent");

    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
    let project = client
        .request("project.add", json!({ "path": repo.path() }))
        .await
        .expect("add project");
    assert_eq!(project["base_ref"], "main");

    let note = "notes/agent.txt";
    let workspace = client
        .request(
            "workspace.create",
            json!({
                "project": project["id"], "name": "ABC-12 Fix the login page", "status": "todo",
                "issue": { "provider": "linear", "id": "ABC-12" },
                "agent_command": [fake], "prompt": format!("write:{note} hi"),
            }),
        )
        .await
        .expect("create workspace");
    assert_eq!(workspace["name"], "abc-12-fix-the-login-page");
    let path = PathBuf::from(workspace["path"].as_str().expect("path"));
    assert!(path.join("readme.md").exists(), "worktree checked out");
    let agent_id = workspace["agents"][0]["id"]
        .as_str()
        .expect("an agent from the start")
        .to_string();
    client
        .request("agent.wait", json!({ "id": agent_id, "timeout_ms": 10000 }))
        .await
        .expect("agent turn");

    let changes = client
        .request("workspace.changes", json!({ "id": workspace["id"] }))
        .await
        .expect("changes");
    assert_eq!(changes["compare"]["status"], "ready");
    let rows = changes["rows"].as_array().expect("rows");
    assert!(
        rows.iter()
            .any(|r| r["path"] == note && r["status"] == "added"),
        "{rows:?}"
    );

    let mut activity = Value::Null;
    for _ in 0..50 {
        let info = client
            .request("workspace.info", json!({ "id": workspace["id"] }))
            .await
            .expect("info");
        activity = info["activity_at_ms"].clone();
        if !activity.is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        activity
            .as_u64()
            .is_some_and(|at| at >= workspace["created_at_ms"].as_u64().expect("created")),
        "the agent's turn stamps the activity time: {activity}"
    );
    let first_rank = workspace["manual_order"]
        .as_i64()
        .expect("a new workspace is ranked");
    let ordered = client
        .request("workspace.reorder", json!({ "ids": [workspace["name"]] }))
        .await
        .expect("reorder by name");
    assert_eq!(ordered["ordered"], 1);
    let info = client
        .request("workspace.info", json!({ "id": workspace["id"] }))
        .await
        .expect("info");
    assert!(info["manual_order"].as_i64().expect("rank") >= first_rank);
    let unknown = client
        .request("workspace.reorder", json!({ "ids": ["nope"] }))
        .await
        .expect_err("an unknown workspace");
    assert!(format!("{unknown:#}").contains("not_found"), "{unknown:#}");

    let updated = client
        .request(
            "workspace.set",
            json!({ "id": "abc-12-fix-the-login-page", "status": "in-review" }),
        )
        .await
        .expect("set status");
    assert_eq!(updated["status"], "in-review");
    assert_eq!(updated["pinned"], false);
    let pinned = client
        .request(
            "workspace.set",
            json!({ "id": workspace["id"], "pinned": true }),
        )
        .await
        .expect("pin");
    assert_eq!(pinned["pinned"], true);
    assert_eq!(pinned["status"], "in-review", "other fields stay");
    let listed = client
        .request("workspace.list", json!({}))
        .await
        .expect("list");
    assert_eq!(listed[0]["pinned"], true);

    let refused = client
        .request("workspace.remove", json!({ "id": workspace["id"] }))
        .await;
    assert!(refused.is_err(), "dirty worktree needs force");
    client
        .request(
            "workspace.remove",
            json!({ "id": workspace["id"], "force": true }),
        )
        .await
        .expect("forced remove");
    assert!(!path.exists());
    let listed = client
        .request("workspace.list", json!({}))
        .await
        .expect("list");
    assert_eq!(
        listed.as_array().map(Vec::len),
        Some(1),
        "only the main checkout is left: {listed}"
    );
    assert_eq!(listed[0]["primary"], true);
    let agents = client
        .request("agent.list", json!({}))
        .await
        .expect("agents");
    assert_eq!(agents, json!([]), "agents die with their workspace");
}

#[tokio::test]
async fn an_agent_never_sent_a_message_survives_a_restart_and_starts_fresh() {
    let home = tempfile::tempdir().expect("tempdir");
    let work = tempfile::tempdir().expect("workdir");
    let fake = env!("CARGO_BIN_EXE_icarus-fake-agent");
    let server = start_server(home.path());
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
    let mut ids = Vec::new();
    for _ in 0..2 {
        let info = client
            .request(
                "agent.create",
                json!({ "cwd": work.path(), "command": [fake], "env": { "ICARUS_FAKE_LAZY_INIT": "1" } }),
            )
            .await
            .expect("create");
        let id = info["id"].as_str().expect("id").to_string();
        client
            .request("agent.sleep", json!({ "id": id }))
            .await
            .expect("sleep before any message");
        ids.push(id);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drop(client);
    drop(server);

    let _server = start_server(home.path());
    let (client, _events) = Client::connect(home.path(), "test")
        .await
        .expect("reconnect");
    assert!(
        client
            .request("agent.info", json!({ "id": ids[0] }))
            .await
            .is_err(),
        "older unused agents hold nothing, so they aren't kept"
    );
    let id = ids[1].clone();
    let restored = client
        .request("agent.info", json!({ "id": id }))
        .await
        .expect("the warm agent comes back, so its tab can reload it");
    assert_eq!(restored["state"], "sleeping");
    client
        .request("agent.send", json!({ "id": id, "text": "hello" }))
        .await
        .expect("send wakes it");
    let after = client
        .request("agent.wait", json!({ "id": id, "timeout_ms": 10000 }))
        .await
        .expect("wait after wake");
    assert_eq!(after["state"], "idle");
    assert_eq!(after["turns"], 1);
}

#[tokio::test]
async fn agents_survive_a_server_restart_and_resume_their_session() {
    let home = tempfile::tempdir().expect("tempdir");
    let work = tempfile::tempdir().expect("workdir");
    let fake = env!("CARGO_BIN_EXE_icarus-fake-agent");
    let server = start_server(home.path());
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
    let info = client
        .request(
            "agent.create",
            json!({ "cwd": work.path(), "command": [fake], "prompt": "first" }),
        )
        .await
        .expect("create");
    let id = info["id"].as_str().expect("id").to_string();
    let done = client
        .request("agent.wait", json!({ "id": id, "timeout_ms": 10000 }))
        .await
        .expect("wait");
    let session = done["provider_session"]
        .as_str()
        .expect("session id")
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
    assert_eq!(restored["turns"], 1);
    client
        .request("agent.send", json!({ "id": id, "text": "second" }))
        .await
        .expect("send wakes the agent");
    let after = client
        .request("agent.wait", json!({ "id": id, "timeout_ms": 10000 }))
        .await
        .expect("wait after wake");
    assert_eq!(after["state"], "idle");
    assert_eq!(
        after["provider_session"],
        session.as_str(),
        "resumed the same provider session"
    );
    let transcript = client
        .request("agent.read", json!({ "id": id }))
        .await
        .expect("read");
    let text = transcript["text"].as_str().expect("text");
    assert!(
        text.contains("echo: first") && text.contains("echo: second"),
        "{text}"
    );
}

/// Sends `text` and returns the agent's answer to it.
async fn say(client: &Client, id: &str, text: &str) -> String {
    client
        .request("agent.send", json!({ "id": id, "text": text }))
        .await
        .expect("send");
    client
        .request("agent.wait", json!({ "id": id, "timeout_ms": 10000 }))
        .await
        .expect("wait");
    let transcript = client
        .request("agent.read", json!({ "id": id }))
        .await
        .expect("read");
    transcript["text"]
        .as_str()
        .expect("text")
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("echo: "))
        .expect("an answer")
        .to_string()
}

#[tokio::test]
async fn an_agents_mode_and_model_reach_claude_code_and_survive_a_restart() {
    let home = tempfile::tempdir().expect("tempdir");
    let work = tempfile::tempdir().expect("workdir");
    let fake = env!("CARGO_BIN_EXE_icarus-fake-agent");
    let server = start_server(home.path());
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
    let info = client
        .request(
            "agent.create",
            json!({ "cwd": work.path(), "command": [fake], "provider": "claude", "mode": "acceptEdits" }),
        )
        .await
        .expect("create");
    let id = info["id"].as_str().expect("id").to_string();
    assert_eq!(
        say(&client, &id, "settings").await,
        "mode=acceptEdits model=default"
    );

    let commands = client
        .request("agent.commands", json!({ "id": id }))
        .await
        .expect("commands");
    let names: Vec<&str> = commands["commands"]
        .as_array()
        .expect("commands")
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert_eq!(
        names,
        ["compact", "review"],
        "internal commands stay hidden"
    );
    assert_eq!(commands["commands"][0]["hint"], "<focus>");
    let models = client
        .request("agent.models", json!({ "id": id }))
        .await
        .expect("models");
    assert_eq!(models["current"], "default");
    assert!(
        models["models"]
            .as_array()
            .expect("models")
            .iter()
            .any(|m| m["id"] == "opus" && m["name"] == "Opus"),
        "the models Claude Code reported: {models}"
    );
    let modes = client
        .request("agent.modes", json!({ "id": id }))
        .await
        .expect("modes");
    assert_eq!(modes["current"], "acceptEdits");
    assert!(
        modes["modes"]
            .as_array()
            .expect("modes")
            .iter()
            .any(|m| m["id"] == "bypassPermissions")
    );

    let set = client
        .request("agent.set_mode", json!({ "id": id, "mode": "plan" }))
        .await
        .expect("set mode");
    assert_eq!(set["mode"], "plan");
    client
        .request("agent.set_model", json!({ "id": id, "model": "sonnet" }))
        .await
        .expect("set model");
    assert_eq!(
        say(&client, &id, "settings").await,
        "mode=plan model=sonnet"
    );
    assert!(
        client
            .request("agent.set_mode", json!({ "id": id, "mode": "yolo" }))
            .await
            .is_err(),
        "unknown modes are refused"
    );
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
    assert_eq!(restored["mode"], "plan");
    assert_eq!(restored["model"], "sonnet");
    let commands = client
        .request("agent.commands", json!({ "id": id }))
        .await
        .expect("commands while asleep");
    assert_eq!(commands["commands"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        say(&client, &id, "settings").await,
        "mode=plan model=sonnet",
        "the woken process starts in the picked mode and model"
    );

    client
        .request("agent.sleep", json!({ "id": id }))
        .await
        .expect("sleep");
    let deadline = Instant::now() + Duration::from_secs(10);
    while client
        .request("agent.info", json!({ "id": id }))
        .await
        .expect("info")["state"]
        != "sleeping"
    {
        assert!(Instant::now() < deadline, "never slept");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    client
        .request(
            "agent.set_mode",
            json!({ "id": id, "mode": "bypassPermissions" }),
        )
        .await
        .expect("set mode while asleep");
    assert_eq!(
        say(&client, &id, "settings").await,
        "mode=bypassPermissions model=sonnet"
    );
}

#[tokio::test]
async fn proxy_starts_the_server_and_bridges_a_tokenless_client() {
    let home = tempfile::tempdir().expect("tempdir");
    let server_bin = env!("CARGO_BIN_EXE_icarus-server");
    let home_arg = home.path().to_str().expect("utf8").to_string();
    // Same path an SSH session takes: `icarus-server proxy` on the remote machine.
    let (client, _events) =
        Client::connect_command(server_bin, &["--home", &home_arg, "proxy"], "remote")
            .await
            .expect("connect through proxy");
    let info = client
        .request("server.info", json!({}))
        .await
        .expect("info");
    assert_eq!(info["protocol"], 1);
    let created = client
        .request(
            "terminal.create",
            json!({ "command": ["/bin/echo", "via-proxy"], "cwd": "/tmp" }),
        )
        .await
        .expect("create over proxy");
    let id = created["id"].as_str().expect("id");
    client
        .request(
            "terminal.wait",
            json!({ "id": id, "contains": "via-proxy", "timeout_ms": 5000 }),
        )
        .await
        .expect("output over proxy");
    client
        .request("server.shutdown", json!({}))
        .await
        .expect("shutdown");
}

#[tokio::test]
async fn open_file_requests_reach_the_app_connection_only() {
    let home = tempfile::tempdir().expect("tempdir");
    let _server = start_server(home.path());
    let (cli, _) = Client::connect(home.path(), "icarus-cli")
        .await
        .expect("cli");
    let refused = cli
        .request("client.open_file", json!({ "path": "/w/a.tsx", "line": 3 }))
        .await;
    assert!(refused.is_err(), "no app connected yet");

    let (_app, mut app_events) = Client::connect(home.path(), "icarus-app")
        .await
        .expect("app");
    let (_panel, mut panel_events) = Client::connect(home.path(), "icarus-panel")
        .await
        .expect("panel");
    let delivered = cli
        .request(
            "client.open_file",
            json!({ "path": "/w/a.tsx", "line": 3, "column": 7 }),
        )
        .await
        .expect("open");
    assert_eq!(delivered["delivered"], 1);
    let event = tokio::time::timeout(Duration::from_secs(5), app_events.recv())
        .await
        .expect("event in time")
        .expect("open stream");
    assert_eq!(event.topic, "client.open_file");
    assert_eq!(event.payload["line"], 3);
    assert_eq!(event.payload["column"], 7);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), panel_events.recv())
            .await
            .is_err(),
        "only the app connection opens files, so a file never opens twice"
    );
}

#[tokio::test]
async fn agents_the_server_starts_for_a_workspace_are_offered_to_the_app_as_tabs_once() {
    let home = tempfile::tempdir().expect("tempdir");
    let repo = tempfile::tempdir().expect("repo");
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "T"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.path().join("readme.md"), "hello\n").expect("write");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    let _server = start_server(home.path());
    let fake = env!("CARGO_BIN_EXE_icarus-fake-agent");
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
    let (_app, mut app_events) = Client::connect(home.path(), "icarus-app")
        .await
        .expect("app");
    let project = client
        .request("project.add", json!({ "path": repo.path() }))
        .await
        .expect("add project");
    let workspace = client
        .request(
            "workspace.create",
            json!({ "project": project["id"], "name": "review", "agent": false }),
        )
        .await
        .expect("create workspace");
    let path = workspace["path"].as_str().expect("path").to_owned();
    let create = |offer_tab: Option<bool>| {
        let mut params = json!({
            "cwd": path, "workspace": workspace["id"], "command": [fake],
            "env": { "ICARUS_FAKE_LAZY_INIT": "1" },
        });
        if let Some(offer_tab) = offer_tab {
            params["offer_tab"] = json!(offer_tab);
        }
        let client = &client;
        async move {
            let info = client
                .request("agent.create", params)
                .await
                .expect("create agent");
            info["id"].as_str().expect("id").to_owned()
        }
    };
    let review = create(None).await;
    let offered = next_event(&mut app_events, "agent_tabs").await;
    assert_eq!(offered["workspace_id"], workspace["id"]);
    assert_eq!(offered["path"], json!(path));
    let screenshots = create(None).await;
    let attached = create(None).await;
    create(Some(false)).await;
    client
        .request("agent.attach", json!({ "id": attached }))
        .await
        .expect("attach");

    let idle = client
        .request("agent.wait", json!({ "id": review, "timeout_ms": 5000 }))
        .await
        .expect("an agent waiting for its first message reads as idle, not starting");
    assert_eq!(idle["state"], "idle");

    let taken = client
        .request("workspace.take_agent_tabs", json!({ "id": path }))
        .await
        .expect("take");
    assert_eq!(
        taken["agents"],
        json!([review, screenshots]),
        "an attached agent already has a tab, and a tab's own agent is never offered"
    );
    let again = client
        .request(
            "workspace.take_agent_tabs",
            json!({ "id": workspace["id"] }),
        )
        .await
        .expect("take again");
    assert_eq!(again["agents"], json!([]), "handed out once");
}

#[tokio::test]
async fn capture_requests_reach_only_the_app() {
    let home = tempfile::tempdir().expect("tempdir");
    let _server = start_server(home.path());
    let (cli, _) = Client::connect(home.path(), "icarus-cli")
        .await
        .expect("cli");
    assert!(
        cli.request("client.capture", json!({ "dir": "/tmp/shots" }))
            .await
            .is_err(),
        "no app connected yet"
    );
    let (_app, mut app_events) = Client::connect(home.path(), "icarus-app")
        .await
        .expect("app");
    assert!(
        cli.request("client.capture", json!({ "dir": "relative" }))
            .await
            .is_err(),
        "the app may run in another directory, so the path must be absolute"
    );
    let delivered = cli
        .request("client.capture", json!({ "dir": "/tmp/shots" }))
        .await
        .expect("capture");
    assert_eq!(delivered["delivered"], 1);
    let event = tokio::time::timeout(Duration::from_secs(5), app_events.recv())
        .await
        .expect("event in time")
        .expect("open stream");
    assert_eq!(event.topic, "client.capture");
    assert_eq!(event.payload["dir"], "/tmp/shots");
}

#[tokio::test]
async fn client_config_requests_reach_the_app_with_their_operation() {
    let home = tempfile::tempdir().expect("tempdir");
    let _server = start_server(home.path());
    let (cli, _) = Client::connect(home.path(), "icarus-cli")
        .await
        .expect("cli");
    let get = json!({ "op": "get", "result": "/tmp/r.json" });
    assert!(
        cli.request("client.config", get.clone()).await.is_err(),
        "no app connected yet"
    );
    let (_app, mut app_events) = Client::connect(home.path(), "icarus-app")
        .await
        .expect("app");
    for bad in [
        json!({ "op": "drop", "key": "a" }),
        json!({ "op": "set", "value": 1 }),
        json!({ "op": "set", "key": "workspace.cardFields" }),
        json!({ "op": "unset", "key": "a", "result": "relative.json" }),
    ] {
        let error = cli
            .request("client.config", bad.clone())
            .await
            .expect_err("invalid");
        assert!(format!("{error:#}").contains("invalid_params"), "{bad}");
    }
    let delivered = cli
        .request(
            "client.config",
            json!({ "op": "set", "key": "workspace.cardFields", "value": ["pr"], "result": "/tmp/r.json" }),
        )
        .await
        .expect("set");
    assert_eq!(delivered["delivered"], 1);
    let event = tokio::time::timeout(Duration::from_secs(5), app_events.recv())
        .await
        .expect("event in time")
        .expect("open stream");
    assert_eq!(event.topic, "client.config");
    assert_eq!(
        event.payload,
        json!({ "op": "set", "key": "workspace.cardFields", "value": ["pr"], "result": "/tmp/r.json" })
    );
}

async fn wait_for_unread(client: &Client, workspace: &Value, want: Value) -> Value {
    let mut info = Value::Null;
    for _ in 0..100 {
        info = client
            .request("workspace.info", json!({ "id": workspace["id"] }))
            .await
            .expect("info");
        if info["unread"]["kind"] == want || (want.is_null() && info["unread"].is_null()) {
            return info;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("unread never became {want}: {}", info["unread"]);
}

#[tokio::test]
async fn a_finished_or_waiting_agent_marks_its_workspace_unread_until_a_client_shows_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let repo = tempfile::tempdir().expect("repo");
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "T"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.path().join("readme.md"), "hello\n").expect("write");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    let _server = start_server(home.path());
    let fake = env!("CARGO_BIN_EXE_icarus-fake-agent");
    let (client, mut events) = Client::connect(home.path(), "test").await.expect("connect");
    let project = client
        .request("project.add", json!({ "path": repo.path() }))
        .await
        .expect("add project");
    let workspace = client
        .request(
            "workspace.create",
            json!({ "project": project["id"], "name": "unread", "agent_command": [fake], "prompt": "hello" }),
        )
        .await
        .expect("create workspace");
    let agent = workspace["agents"][0]["id"].clone();
    let path = PathBuf::from(workspace["path"].as_str().expect("path"));
    client
        .request("agent.wait", json!({ "id": agent, "timeout_ms": 10000 }))
        .await
        .expect("turn");
    let info = wait_for_unread(&client, &workspace, json!("done")).await;
    assert!(info["unread"]["at_ms"].as_u64().is_some());
    let listed = client
        .request("workspace.list", json!({}))
        .await
        .expect("list");
    assert_eq!(
        listed[0]["unread"]["kind"], "done",
        "the list carries it too"
    );

    let mut saw_event = false;
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(200), events.recv()).await
    {
        if event.topic == "workspace.changed" && event.payload["source"] == "unread" {
            assert_eq!(event.payload["workspaces"][0], workspace["id"]);
            saw_event = true;
        }
    }
    assert!(saw_event, "clients hear about it");

    let (app, _) = Client::connect(home.path(), "icarus-app")
        .await
        .expect("app");
    let focused = app
        .request("client.focus", json!({ "path": path.join("src") }))
        .await
        .expect("focus");
    assert_eq!(focused["workspace"], workspace["id"]);
    wait_for_unread(&client, &workspace, Value::Null).await;

    client
        .request("agent.send", json!({ "id": agent, "text": "again" }))
        .await
        .expect("send");
    client
        .request("agent.wait", json!({ "id": agent, "timeout_ms": 10000 }))
        .await
        .expect("turn");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let info = client
        .request("workspace.info", json!({ "id": workspace["id"] }))
        .await
        .expect("info");
    assert!(
        info["unread"].is_null(),
        "a turn in the workspace a client shows is seen"
    );

    app.request("client.focus", json!({}))
        .await
        .expect("unfocus");
    client
        .request(
            "agent.send",
            json!({ "id": agent, "text": "ask Which one?" }),
        )
        .await
        .expect("send");
    client
        .request("agent.wait", json!({ "id": agent, "timeout_ms": 10000 }))
        .await
        .expect("turn");
    wait_for_unread(&client, &workspace, json!("needs_input")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let info = client
        .request("workspace.info", json!({ "id": workspace["id"] }))
        .await
        .expect("info");
    assert_eq!(
        info["unread"]["kind"], "needs_input",
        "the end of the asking turn does not hide the question"
    );

    let read = client
        .request(
            "workspace.set_unread",
            json!({ "id": "unread", "unread": false }),
        )
        .await
        .expect("mark read");
    assert!(read["unread"].is_null());
    let marked = client
        .request(
            "workspace.set_unread",
            json!({ "id": workspace["id"], "unread": true }),
        )
        .await
        .expect("mark unread");
    assert_eq!(marked["unread"]["kind"], "done");

    app.request("client.focus", json!({ "workspace": "unread" }))
        .await
        .expect("focus by name");
    wait_for_unread(&client, &workspace, Value::Null).await;
    drop(app);
    tokio::time::sleep(Duration::from_millis(200)).await;
    client
        .request("agent.send", json!({ "id": agent, "text": "after" }))
        .await
        .expect("send");
    client
        .request("agent.wait", json!({ "id": agent, "timeout_ms": 10000 }))
        .await
        .expect("turn");
    wait_for_unread(&client, &workspace, json!("done")).await;
}

#[tokio::test]
async fn wrong_token_is_rejected() {
    let home = tempfile::tempdir().expect("tempdir");
    let _server = start_server(home.path());
    std::fs::write(home.path().join("server.token"), "nope").expect("overwrite token");
    let result = Client::connect(home.path(), "intruder").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn second_server_refuses_to_start_on_a_live_socket() {
    let home = tempfile::tempdir().expect("tempdir");
    let _server = start_server(home.path());
    let output = Command::new(env!("CARGO_BIN_EXE_icarus-server"))
        .args(["--home", home.path().to_str().expect("utf8"), "serve"])
        .output()
        .expect("run second server");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already running"));
}
