use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use icarus_client::Client;
use serde_json::{Value, json};

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

fn labels(listed: &Value) -> Vec<&str> {
    listed["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|command| command["label"].as_str())
        .collect()
}

#[tokio::test]
async fn imported_orca_quick_commands_list_per_project_and_edit_in_user_settings() {
    let home = tempfile::tempdir().expect("home");
    let config = tempfile::tempdir().expect("config");
    let repo = tempfile::tempdir().expect("repo");
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "T"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    git(
        repo.path(),
        &["remote", "add", "origin", "https://github.com/acme/app.git"],
    );
    std::fs::write(repo.path().join("readme.md"), "hello\n").expect("write");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);

    let _server = ServerProcess(
        Command::new(env!("CARGO_BIN_EXE_icarus-server"))
            .args(["--home", home.path().to_str().expect("utf8"), "serve"])
            .env("ICARUS_WORKSPACE_ROOT", home.path().join("workspaces"))
            .env("ICARUS_CONFIG_DIR", config.path())
            .env("ICARUS_GH_FIXTURES", home.path().join("no-gh"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn server"),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(home.path().join("server.sock").exists() && home.path().join("server.token").exists()) {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
    let request = |method: &'static str, params: Value| {
        let client = client.clone();
        async move {
            client
                .request(method, params)
                .await
                .unwrap_or_else(|e| panic!("{method}: {e:#}"))
        }
    };

    let export = json!({
        "format": "orca-settings", "version": 1, "exportedAt": "",
        "settings": { "terminalQuickCommands": [
            { "id": "run", "label": "Run", "scope": { "type": "project", "remote": "github.com/acme/app" },
              "action": "terminal-command", "command": "mise run fu", "appendEnter": true },
            { "id": "other", "label": "Other repo", "scope": { "type": "project", "remote": "github.com/acme/other" },
              "action": "terminal-command", "command": "make", "appendEnter": true },
            { "id": "status", "label": "Status", "scope": { "type": "global" },
              "action": "terminal-command", "command": "git status", "appendEnter": true },
        ] }
    })
    .to_string();
    request(
        "settings.import_orca",
        json!({ "text": export, "zed_dir": config.path().join("zed") }),
    )
    .await;

    let project = request("project.add", json!({ "path": repo.path() })).await;
    let workspace = request(
        "workspace.create",
        json!({ "project": project["id"], "name": "qc" }),
    )
    .await;
    let workspace_id = workspace["id"].as_str().expect("id").to_owned();
    let listed = request("quick_command.list", json!({ "workspace": workspace_id })).await;
    assert_eq!(labels(&listed), ["Run", "Status"]);
    assert_eq!(listed["remote"], "github.com/acme/app");
    let all = request("quick_command.list", json!({})).await;
    assert_eq!(labels(&all), ["Run", "Other repo", "Status"]);

    let added = request(
        "quick_command.upsert",
        json!({ "command": { "label": "Preview", "command": "mise run preview",
                             "scope": { "type": "project", "remote": "github.com/acme/app" } } }),
    )
    .await;
    request(
        "quick_command.upsert",
        json!({ "command": { "id": "run", "label": "Run", "command": "mise run dev",
                             "scope": { "type": "project", "remote": "github.com/acme/app" } } }),
    )
    .await;
    request("quick_command.delete", json!({ "id": "status" })).await;
    let listed = request("quick_command.list", json!({ "workspace": workspace_id })).await;
    assert_eq!(labels(&listed), ["Run", "Preview"]);
    assert_eq!(listed["commands"][0]["command"], "mise run dev");
    assert_eq!(listed["commands"][1]["id"], added["id"]);

    let stored = request("config.get", json!({ "key": "quickCommands" })).await;
    assert_eq!(
        stored.as_array().map(Vec::len),
        Some(3),
        "edits land in the user setting the import wrote: {stored}"
    );
    assert!(
        client
            .request(
                "quick_command.upsert",
                json!({ "command": { "label": "", "command": "x" } })
            )
            .await
            .is_err(),
        "a command needs a name"
    );
}
