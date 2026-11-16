use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use icarus_client::{Client, Event};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedReceiver;

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn git(dir: &Path, args: &[&str]) {
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
}

fn settings(timeout_ms: Option<u64>) -> Value {
    let mut settings = json!({ "agents": { "commands": { "claude": [env!("CARGO_BIN_EXE_icarus-fake-agent")] } } });
    if let Some(ms) = timeout_ms {
        settings["notes"] = json!({ "reply_timeout_ms": ms });
    }
    settings
}

struct World {
    home: TempDir,
    config: TempDir,
    _repos: TempDir,
    server: Option<ServerProcess>,
    client: Arc<Client>,
    events: UnboundedReceiver<Event>,
    project: String,
}

impl World {
    async fn new() -> Self {
        let home = tempfile::tempdir().expect("home");
        let config = tempfile::tempdir().expect("config");
        let repos = tempfile::tempdir().expect("repos");
        let repo = repos.path().join("widgets");
        std::fs::create_dir_all(&repo).expect("repo");
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "me@example.com"]);
        git(&repo, &["config", "user.name", "Me"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::create_dir_all(repo.join("src")).expect("src");
        std::fs::write(
            repo.join("src/lib.rs"),
            "fn one() {}\nfn two() {}\nfn three() {}\nfn four() {}\n",
        )
        .expect("write");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        std::fs::write(
            config.path().join("icarus.json"),
            settings(None).to_string(),
        )
        .expect("settings");
        let server = Self::spawn(home.path(), config.path());
        let (client, events) = Client::connect(home.path(), "test").await.expect("connect");
        let project = client
            .request(
                "project.add",
                json!({ "path": repo, "name": "widgets", "base_ref": "main" }),
            )
            .await
            .expect("project");
        Self {
            project: project["id"].as_str().expect("id").to_owned(),
            home,
            config,
            _repos: repos,
            server: Some(server),
            client,
            events,
        }
    }

    fn spawn(home: &Path, config: &Path) -> ServerProcess {
        let _ = std::fs::remove_file(home.join("server.token"));
        let child = Command::new(env!("CARGO_BIN_EXE_icarus-server"))
            .args(["--home", home.to_str().expect("utf8"), "serve"])
            .env("ICARUS_WORKSPACE_ROOT", home.join("workspaces"))
            .env("ICARUS_CONFIG_DIR", config)
            .env("ICARUS_GH_FIXTURES", home.join("no-gh"))
            .env_remove("CLAUDE_CONFIG_DIR")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn server");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !home.join("server.token").exists() {
            assert!(Instant::now() < deadline, "server did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        ServerProcess(child)
    }

    async fn restart(&mut self) {
        self.server.take();
        self.server = Some(Self::spawn(self.home.path(), self.config.path()));
        let (client, events) = Client::connect(self.home.path(), "test")
            .await
            .expect("reconnect");
        self.client = client;
        self.events = events;
    }

    fn set_settings(&self, value: Value) {
        std::fs::write(self.config.path().join("icarus.json"), value.to_string())
            .expect("settings");
    }

    async fn request(&self, method: &str, params: Value) -> Value {
        self.client
            .request(method, params)
            .await
            .unwrap_or_else(|e| panic!("{method}: {e:#}"))
    }

    async fn error(&self, method: &str, params: Value) -> String {
        match self.client.request(method, params).await {
            Ok(value) => panic!("{method} succeeded: {value}"),
            Err(error) => format!("{error:#}"),
        }
    }

    async fn workspace(&self, name: &str, agent: bool) -> (String, PathBuf, Value) {
        let ws = self
            .request(
                "workspace.create",
                json!({ "project": self.project, "name": name, "agent": agent }),
            )
            .await;
        (
            ws["id"].as_str().expect("id").to_owned(),
            PathBuf::from(ws["path"].as_str().expect("path")),
            ws,
        )
    }

    async fn note(&self, workspace: &str, body: &str) -> String {
        let note = self
            .request(
                "note.add",
                json!({ "workspace": workspace, "path": "src/lib.rs", "line": 2, "end_line": 3, "body": body }),
            )
            .await;
        note["id"].as_str().expect("id").to_owned()
    }

    async fn reply(&self, note: &str, body: &str) -> String {
        let replied = self
            .request("note.reply", json!({ "id": note, "body": body }))
            .await;
        assert_eq!(replied["agent_reply"]["status"], "pending", "{replied}");
        assert_eq!(replied["reply"]["author"], "user");
        replied["agent_reply"]["id"]
            .as_str()
            .expect("agent reply")
            .to_owned()
    }

    async fn answered(&self, note: &str, reply: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let note = self.request("note.get", json!({ "id": note })).await;
            let entry = note["replies"]
                .as_array()
                .and_then(|r| r.iter().find(|r| r["id"] == reply))
                .cloned()
                .expect("reply in thread");
            if entry["status"] == "done" || entry["status"] == "failed" {
                return entry;
            }
            assert!(Instant::now() < deadline, "no answer: {note}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn note_event(&mut self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = tokio::time::timeout(remaining, self.events.recv())
                .await
                .unwrap_or_else(|_| panic!("no note.changed for {id}"))
                .expect("events open");
            if event.topic == "note.changed" && event.payload["id"] == id {
                return event.payload;
            }
        }
    }
}

#[tokio::test]
async fn notes_are_added_listed_edited_resolved_and_removed_with_events() {
    let mut world = World::new().await;
    let (ws, path, _) = world.workspace("crud", false).await;

    let note = world
        .request(
            "note.add",
            json!({ "workspace": ws, "path": path.join("src/lib.rs"), "line": 2, "end_line": 3, "body": "Why **two**?" }),
        )
        .await;
    let id = note["id"].as_str().expect("id").to_owned();
    assert_eq!(note["path"], "src/lib.rs", "absolute paths become relative");
    assert_eq!(note["author"], "user");
    assert_eq!(note["excerpt"], "fn two() {}\nfn three() {}");
    assert_eq!(note["resolved"], false);
    let event = world.note_event(&id).await;
    assert_eq!(
        event,
        json!({ "workspace": ws, "id": id, "path": "src/lib.rs", "removed": false })
    );

    let other = world
        .request(
            "note.add",
            json!({ "cwd": path.join("src"), "path": "README.md", "line": 1, "body": "Add docs", "side": "old" }),
        )
        .await;
    assert_eq!(
        other["workspace_id"], ws,
        "the workspace comes from the cwd"
    );
    let other_id = other["id"].as_str().expect("id").to_owned();

    let listed = world.request("note.list", json!({ "workspace": ws })).await;
    assert_eq!(listed.as_array().map(Vec::len), Some(2));
    let only = world
        .request(
            "note.list",
            json!({ "workspace": ws, "path": "src/lib.rs" }),
        )
        .await;
    assert_eq!(only[0]["id"], id);

    let edited = world
        .request("note.edit", json!({ "id": id, "body": "Why three?" }))
        .await;
    assert_eq!(edited["body"], "Why three?");
    world.note_event(&id).await;

    let resolved = world.request("note.resolve", json!({ "id": id })).await;
    assert_eq!(resolved["resolved"], true);
    world.note_event(&id).await;
    let open = world.request("note.list", json!({ "workspace": ws })).await;
    assert_eq!(open.as_array().map(Vec::len), Some(1), "resolved hidden");
    let all = world
        .request(
            "note.list",
            json!({ "workspace": ws, "include_resolved": true }),
        )
        .await;
    assert_eq!(all.as_array().map(Vec::len), Some(2));
    let reopened = world.request("note.unresolve", json!({ "id": id })).await;
    assert_eq!(reopened["resolved"], false);

    let removed = world
        .request("note.remove", json!({ "id": other_id }))
        .await;
    assert_eq!(removed["removed"], true);
    let gone = world.note_event(&other_id).await;
    assert_eq!(gone["removed"], true);
    assert!(
        world
            .error("note.get", json!({ "id": other_id }))
            .await
            .contains("not_found")
    );

    let past = world
        .error(
            "note.add",
            json!({ "workspace": ws, "path": "src/lib.rs", "line": 40, "body": "x" }),
        )
        .await;
    assert!(past.contains("past the end"), "{past}");
    let outside = world
        .error(
            "note.add",
            json!({ "workspace": ws, "path": "../escape.rs", "line": 1, "body": "x" }),
        )
        .await;
    assert!(outside.contains("inside the workspace"), "{outside}");
    let empty = world
        .error(
            "note.add",
            json!({ "workspace": ws, "path": "src/lib.rs", "line": 1, "body": "  " }),
        )
        .await;
    assert!(empty.contains("empty"), "{empty}");
}

#[tokio::test]
async fn agents_write_notes_and_their_replies_start_no_session() {
    let world = World::new().await;
    let (ws, _, created) = world.workspace("agent-notes", true).await;
    let main_agent = created["agents"][0]["id"]
        .as_str()
        .expect("main agent")
        .to_owned();
    let note = world
        .request(
            "note.add",
            json!({ "workspace": ws, "path": "src/lib.rs", "line": 4, "body": "I changed this.", "agent_id": main_agent }),
        )
        .await;
    assert_eq!(note["author"], "agent");
    assert_eq!(note["agent_id"], main_agent);
    let replied = world
        .request(
            "note.reply",
            json!({ "id": note["id"], "body": "Also see line 1.", "agent_id": main_agent }),
        )
        .await;
    assert_eq!(replied["reply"]["author"], "agent");
    assert!(
        replied["agent_reply"].is_null(),
        "an agent's reply asks no one"
    );
}

#[tokio::test]
async fn a_reply_is_answered_by_a_throwaway_session_not_the_workspace_agent() {
    let world = World::new().await;
    let (ws, _, created) = world.workspace("reply", true).await;
    let main_agent = created["agents"][0]["id"]
        .as_str()
        .expect("main agent")
        .to_owned();
    let note = world.note(&ws, "What do these do?").await;
    let pending = world.reply(&note, "Explain them in one line.").await;
    let answer = world.answered(&note, &pending).await;

    assert_eq!(answer["status"], "done", "{answer}");
    assert_eq!(answer["author"], "reply-agent");
    let body = answer["body"].as_str().expect("body");
    assert!(
        body.starts_with("echo: You are answering a note thread"),
        "{body}"
    );
    for context in [
        "src/lib.rs:2-3",
        "2  fn two() {}",
        "3  fn three() {}",
        "User:\nWhat do these do?",
        "User:\nExplain them in one line.",
    ] {
        assert!(body.contains(context), "missing {context:?} in {body}");
    }
    assert_eq!(answer["changed_files"], json!([]));

    let session = answer["agent_id"].as_str().expect("session id");
    assert_ne!(session, main_agent);
    let live = world.request("agent.list", json!({})).await;
    assert!(
        !live
            .as_array()
            .expect("list")
            .iter()
            .any(|a| a["id"] == session),
        "the reply session ended: {live}"
    );
    let main = world
        .request("agent.info", json!({ "id": main_agent }))
        .await;
    assert_eq!(main["turns"], 0, "the workspace's agent got no message");
    let history = world
        .request("agent.history", json!({ "workspace": ws }))
        .await;
    let entry = history
        .as_array()
        .and_then(|h| h.iter().find(|e| e["id"] == session))
        .unwrap_or_else(|| panic!("reply session in history: {history}"));
    assert_eq!(entry["state"], "killed");
    let notifications = world.request("notification.list", json!({})).await;
    assert!(
        !notifications.to_string().contains(session),
        "no turn notification for a reply session: {notifications}"
    );

    let follow_up = world.reply(&note, "@fake Thanks, and line 4?").await;
    let second = world.answered(&note, &follow_up).await;
    assert_eq!(second["body"], "echo: Thanks, and line 4?");
    assert_ne!(
        second["agent_id"], answer["agent_id"],
        "a new session per reply"
    );
}

#[tokio::test]
async fn concurrent_replies_each_get_their_own_answer() {
    let world = World::new().await;
    let (ws, _, _) = world.workspace("concurrent", false).await;
    let note = world.note(&ws, "Two questions").await;
    let started = Instant::now();
    let first = world.reply(&note, "@fake sleep:1500 first").await;
    let second = world.reply(&note, "@fake sleep:1500 second").await;
    let (a, b) = (
        world.answered(&note, &first).await,
        world.answered(&note, &second).await,
    );
    assert_eq!(a["body"], "echo: first");
    assert_eq!(b["body"], "echo: second");
    assert!(
        started.elapsed() < Duration::from_millis(2900),
        "the sessions ran at once ({:?})",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_reply_that_edits_the_worktree_lists_the_files() {
    let world = World::new().await;
    let (ws, path, _) = world.workspace("edits", false).await;
    let note = world.note(&ws, "Needs a changelog").await;
    let pending = world
        .reply(&note, "@fake write:CHANGELOG.md Added notes.")
        .await;
    let answer = world.answered(&note, &pending).await;
    assert_eq!(answer["status"], "done", "{answer}");
    assert_eq!(answer["changed_files"], json!(["CHANGELOG.md"]));
    assert_eq!(
        answer["body"],
        "echo: wrote CHANGELOG.md\n\nFiles changed:\n- `CHANGELOG.md`"
    );
    assert_eq!(
        std::fs::read_to_string(path.join("CHANGELOG.md")).expect("written"),
        "Added notes."
    );
}

#[tokio::test]
async fn failed_and_timed_out_replies_say_why() {
    let world = World::new().await;
    let (ws, _, _) = world.workspace("failures", false).await;
    let note = world.note(&ws, "Will fail").await;
    let failing = world.reply(&note, "@fake fail on purpose").await;
    let failed = world.answered(&note, &failing).await;
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["error"], "echo: fail on purpose");
    assert!(
        world
            .request("agent.list", json!({}))
            .await
            .as_array()
            .is_some_and(Vec::is_empty),
        "failed sessions end too"
    );

    world.set_settings(settings(Some(400)));
    let slow = world.reply(&note, "@fake sleep:5000 too slow").await;
    let timed_out = world.answered(&note, &slow).await;
    assert_eq!(timed_out["status"], "failed");
    assert_eq!(
        timed_out["error"],
        "no answer within 400 ms; the session was stopped"
    );
    assert!(
        world
            .request("agent.list", json!({}))
            .await
            .as_array()
            .is_some_and(Vec::is_empty),
        "timed-out sessions are killed"
    );
}

#[tokio::test]
async fn attach_note_puts_the_whole_conversation_in_the_composer() {
    let world = World::new().await;
    let (ws, path, _) = world.workspace("attach", false).await;
    let note = world.note(&ws, "Rename these").await;
    let pending = world.reply(&note, "@fake Done thinking").await;
    world.answered(&note, &pending).await;

    let (_app, mut app_events) = Client::connect(world.home.path(), "icarus-app")
        .await
        .expect("app");
    let attached = world.request("attach.note", json!({ "id": note })).await;
    assert_eq!(attached["kind"], "note");
    assert_eq!(attached["title"], "Note on src/lib.rs:2-3");
    assert_eq!(attached["delivered"], 1);
    assert_eq!(attached["source"]["type"], "note");
    let text = attached["text"].as_str().expect("text");
    for part in [
        format!("Note {note} on src/lib.rs:2-3").as_str(),
        "2  fn two() {}",
        "User:\nRename these",
        "User:\n@fake Done thinking",
        "Reply agent:\necho: Done thinking",
    ] {
        assert!(text.contains(part), "missing {part:?} in {text}");
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let event = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            app_events.recv(),
        )
        .await
        .expect("composer.attach")
        .expect("open");
        if event.topic == "composer.attach" {
            assert_eq!(event.payload["workspace_path"], json!(path));
            break;
        }
    }
    let block = world
        .request("attachment.get", json!({ "id": attached["id"] }))
        .await;
    assert!(
        block["text"].as_str().is_some_and(|t| t.starts_with(
            "<attachment kind=\"note\" title=\"Note on src/lib.rs:2-3\" source=\"src/lib.rs:2-3\">"
        )),
        "{block}"
    );
}

#[tokio::test]
async fn notes_survive_a_restart_and_unfinished_replies_fail() {
    let mut world = World::new().await;
    let (ws, _, _) = world.workspace("restart", false).await;
    let note = world.note(&ws, "Keep me").await;
    world.request("note.resolve", json!({ "id": note })).await;
    let pending = world.reply(&note, "@fake sleep:30000 never").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    world.restart().await;

    let kept = world.request("note.get", json!({ "id": note })).await;
    assert_eq!(kept["body"], "Keep me");
    assert_eq!(kept["resolved"], true);
    let entry = kept["replies"]
        .as_array()
        .and_then(|r| r.iter().find(|r| r["id"] == pending))
        .expect("reply kept");
    assert_eq!(entry["status"], "failed");
    assert!(
        entry["error"]
            .as_str()
            .is_some_and(|e| e.contains("server restarted")),
        "{entry}"
    );
    let live = world.request("agent.list", json!({})).await;
    assert!(
        live.as_array().is_some_and(Vec::is_empty),
        "the reply session was not revived: {live}"
    );
}
