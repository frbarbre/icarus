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

fn start_server(home: &Path) -> ServerProcess {
    let _ = std::fs::remove_file(home.join("server.token"));
    let child = Command::new(env!("CARGO_BIN_EXE_icarus-server"))
        .args(["--home", home.to_str().expect("utf8 home"), "serve"])
        .env("ICARUS_WORKSPACE_ROOT", home.join("workspaces"))
        .env("ICARUS_CONFIG_DIR", home.join("config"))
        .env("ICARUS_GH_FIXTURES", home.join("no-gh"))
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

fn repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("repo");
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "T"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    git(
        repo.path(),
        &["remote", "add", "origin", "git@github.com:acme/app.git"],
    );
    std::fs::create_dir_all(repo.path().join("src")).expect("src");
    std::fs::write(repo.path().join("src/a.rs"), "one\ntwo\nthree\nfour\n").expect("write");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    repo
}

fn orca_documents() -> Value {
    json!({
        "worktree_meta": {
            "r1::/orca/workspaces/fix-login": {
                "workspaceStatus": "in-review",
                "displayName": "Fix the login page",
                "displayNameIsPinned": true,
                "linkedLinearIssue": "ABC-12",
                "diffComments": [
                    { "id": "c1", "filePath": "src/a.rs", "lineNumber": 3, "startLine": 2, "body": "Why two?", "side": "right", "createdAt": 1700000000000u64 },
                    { "id": "c2", "filePath": "src/a.rs", "lineNumber": 3, "body": "It was asked for.", "replyToNoteId": "c1" },
                ],
                "agentNotes": [
                    { "id": "n1", "filePath": "src/a.rs", "lineNumber": 1, "body": "Starts here", "agentAuthor": "claude" },
                ],
            },
            "r1::/orca/workspaces/elsewhere": { "workspaceStatus": "done" },
        },
        "repos": [ { "id": "r1", "gitRemoteIdentity": { "canonicalKey": "github.com/acme/app" } } ],
    })
}

#[tokio::test]
async fn orca_state_moves_statuses_names_issues_and_notes_once() {
    let repo = repo();
    let home = tempfile::tempdir().expect("home");
    let _server = start_server(home.path());
    let (client, _) = Client::connect(home.path(), "icarus-cli")
        .await
        .expect("cli");
    let project = client
        .request("project.add", json!({ "path": repo.path() }))
        .await
        .expect("project");
    let workspace = client
        .request(
            "workspace.create",
            json!({ "project": project["id"], "name": "fix-login", "agent": false }),
        )
        .await
        .expect("workspace");

    let mut request = orca_documents();
    request["dry_run"] = json!(true);
    let dry = client
        .request("settings.import_orca_state", request)
        .await
        .expect("dry run");
    assert_eq!(dry["orca_worktrees"], 2);
    assert_eq!(dry["unmatched"], 1);
    assert_eq!(dry["matched"][0]["matched_by"], "repo and name");
    assert_eq!(dry["matched"][0]["notes"], 2);
    assert_eq!(dry["matched"][0]["replies"], 1);
    let untouched = client
        .request("workspace.info", json!({ "id": workspace["id"] }))
        .await
        .expect("info");
    assert_eq!(
        untouched["status"],
        Value::Null,
        "a dry run changes nothing"
    );

    let applied = client
        .request("settings.import_orca_state", orca_documents())
        .await
        .expect("apply");
    assert_eq!(applied["dry_run"], false);
    let info = client
        .request("workspace.info", json!({ "id": workspace["id"] }))
        .await
        .expect("info");
    assert_eq!(info["status"], "in-review");
    assert_eq!(info["title"], "Fix the login page");
    assert_eq!(info["issue"]["id"], "ABC-12");
    let notes = client
        .request("note.list", json!({ "workspace": workspace["id"] }))
        .await
        .expect("notes");
    let notes = notes.as_array().expect("list");
    assert_eq!(notes.len(), 2);
    let question = notes
        .iter()
        .find(|n| n["body"] == "Why two?")
        .expect("diff note");
    assert_eq!(
        (question["line"].clone(), question["end_line"].clone()),
        (json!(2), json!(3))
    );
    assert_eq!(question["author"], "user");
    assert_eq!(question["replies"][0]["body"], "It was asked for.");
    let agent = notes
        .iter()
        .find(|n| n["body"] == "Starts here")
        .expect("agent note");
    assert_eq!(agent["author"], "agent");

    let again = client
        .request("settings.import_orca_state", orca_documents())
        .await
        .expect("again");
    assert_eq!(again["matched"][0]["notes"], 0, "notes come over once");
    assert_eq!(again["matched"][0]["notes_already_imported"], 3);
    assert_eq!(
        again["matched"][0]["status"],
        Value::Null,
        "the status already matches"
    );
    let notes = client
        .request("note.list", json!({ "workspace": workspace["id"] }))
        .await
        .expect("notes");
    assert_eq!(notes.as_array().map(Vec::len), Some(2));
}
