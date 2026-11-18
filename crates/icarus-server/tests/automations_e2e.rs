use std::os::unix::fs::PermissionsExt;
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
    std::fs::write(repo.path().join("readme.md"), "hello\n").expect("write");
    std::fs::write(repo.path().join(".gitignore"), "icarus.local.json\n").expect("write");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    repo
}

fn write_settings(repo: &Path, settings: &Value) {
    std::fs::write(
        repo.join("icarus.local.json"),
        serde_json::to_vec_pretty(settings).expect("json"),
    )
    .expect("write settings");
}

/// A shell script: a button click moves the card to `completed` and starts nothing; any other run
/// moves it to `in-review`, keeps a counter in memory and offers one button.
fn write_script(path: &Path, workspace: &str, scopes: &Path) {
    let script = format!(
        r#"#!/bin/sh
input=$(cat)
printf '%s' "$input" > "{scopes}/$ICARUS_RUN_ID.json"
case "$input" in
  *'"name":"button"'*)
    echo '[{{"method":"workspace.set","params":{{"id":"{workspace}","status":"completed"}}}},{{"method":"agent.create","params":{{"workspace":"{workspace}","title":"Fixer","prompt_id":"fix"}}}}]' ;;
  *)
    echo "deciding" >&2
    echo '{{"actions":[{{"method":"workspace.set","params":{{"id":"{workspace}","status":"in-review"}}}}],"memory":{{"runs":1}},"buttons":{{"{workspace}":[{{"id":"go","label":"Ship it","reason":"from the test"}}]}}}}' ;;
esac
"#,
        scopes = scopes.display(),
    );
    std::fs::write(path, script).expect("script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

async fn status(client: &Client, id: &str) -> Value {
    client
        .request("workspace.info", json!({ "id": id }))
        .await
        .expect("info")["status"]
        .clone()
}

#[tokio::test]
async fn a_script_decides_statuses_and_buttons_and_its_runs_replay() {
    let fake = env!("CARGO_BIN_EXE_icarus-fake-agent");
    let repo = repo();
    let home = tempfile::tempdir().expect("home");
    let scopes = home.path().join("scopes");
    std::fs::create_dir_all(&scopes).expect("scopes");
    let script = home.path().join("decide.sh");
    let _server = start_server(home.path());
    let (client, _) = Client::connect(home.path(), "icarus-cli")
        .await
        .expect("cli");
    let project = client
        .request("project.add", json!({ "path": repo.path() }))
        .await
        .expect("project");
    let project_name = project["name"].as_str().expect("name").to_owned();
    let created = client
        .request_timeout(
            "workspace.create",
            json!({ "project": project["id"], "name": "w1" }),
            Duration::from_secs(60),
        )
        .await
        .expect("create");
    let workspace = created["workspace"]["id"]
        .as_str()
        .or(created["id"].as_str())
        .expect("workspace id")
        .to_owned();
    write_script(&script, &workspace, &scopes);
    let mut settings = json!({
        "agents": { "commands": { "claude": [fake] } },
        "automations": { "armed": true, "scripts": { project_name.clone(): script } },
        "prompts": { "fix": "Fix {{workspace.name}}" },
    });
    write_settings(repo.path(), &settings);

    let dry = client
        .request(
            "automation.run",
            json!({ "project": project["id"], "dry_run": true }),
        )
        .await
        .expect("dry run");
    let dry = &dry["runs"][0];
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["results"][0]["dry_run"], true);
    assert_ne!(
        status(&client, &workspace).await,
        "in-review",
        "a dry run applies nothing"
    );
    let next = client
        .request("workspace.next_action", json!({ "id": workspace }))
        .await
        .expect("next");
    assert_eq!(
        next["decided_by"], "built-in",
        "a dry run's buttons aren't shown"
    );

    let armed = client
        .request("automation.run", json!({ "project": project["id"] }))
        .await
        .expect("run");
    let armed = &armed["runs"][0];
    assert_eq!(armed["dry_run"], false, "{armed}");
    assert_eq!(armed["acted"], true, "{armed}");
    assert_eq!(armed["stderr"], "deciding\n");
    assert_eq!(status(&client, &workspace).await, "in-review");
    let scope: Value = serde_json::from_slice(
        &std::fs::read(scopes.join(format!("{}.json", armed["id"].as_str().expect("id"))))
            .expect("scope file"),
    )
    .expect("scope json");
    assert_eq!(scope["trigger"]["kind"], "manual");
    assert_eq!(scope["memory"], json!({}));
    assert_eq!(scope["workspaces"][0]["id"], workspace);
    assert!(scope["workspaces"][0]["git"]["head"].is_string(), "{scope}");
    assert!(
        scope["settings"]["statuses"]
            .as_array()
            .is_some_and(|s| !s.is_empty())
    );

    let next = client
        .request("workspace.next_action", json!({ "id": workspace }))
        .await
        .expect("next");
    assert_eq!(next["action"]["label"], "Ship it");
    assert_eq!(next["decided_by"], armed["id"]);
    let explain = client
        .request("workspace.button", json!({ "id": workspace }))
        .await
        .expect("explain");
    assert_eq!(explain["decided_by"], "script");
    assert_eq!(explain["buttons"][0]["reason"], "from the test");
    assert_eq!(
        explain["scope_then"]["status"],
        scope["workspaces"][0]["status"]
    );

    let clicked = client
        .request("workspace.run_next_action", json!({ "id": workspace }))
        .await
        .expect("click");
    assert_eq!(clicked["run"]["trigger"]["name"], "button", "{clicked}");
    assert_eq!(clicked["run"]["trigger"]["payload"]["id"], "go");
    assert_eq!(status(&client, &workspace).await, "completed");
    let started = &clicked["run"]["results"][1];
    assert_eq!(started["ok"], true, "{clicked}");
    let agents = client
        .request("agent.list", json!({}))
        .await
        .expect("agents");
    assert!(
        agents
            .as_array()
            .or(agents["agents"].as_array())
            .into_iter()
            .flatten()
            .any(|a| a["title"] == "Fixer"),
        "{agents}"
    );
    let click_scope: Value = serde_json::from_slice(
        &std::fs::read(scopes.join(format!(
            "{}.json",
            clicked["run"]["id"].as_str().expect("id")
        )))
        .expect("scope file"),
    )
    .expect("json");
    assert_eq!(
        click_scope["memory"],
        json!({ "runs": 1 }),
        "memory carries over"
    );
    assert_eq!(click_scope["previous"]["workspaces"][0]["id"], workspace);

    let replay = client
        .request("automation.replay", json!({ "run": armed["id"] }))
        .await
        .expect("replay");
    assert_eq!(replay["same_as_recorded"], true, "{replay}");

    let emitted = client
        .request(
            "automation.emit",
            json!({ "name": "linear.open_issue", "path": repo.path(), "payload": { "issue": "ABC-1" } }),
        )
        .await
        .expect("emit");
    assert_eq!(emitted["trigger"]["name"], "linear.open_issue");
    assert_eq!(emitted["trigger"]["payload"]["issue"], "ABC-1");

    settings["automations"]["hold_status"] = json!("todo");
    write_settings(repo.path(), &settings);
    client
        .request(
            "workspace.set",
            json!({ "id": workspace, "status": "todo" }),
        )
        .await
        .expect("hold");
    let held = client
        .request("automation.run", json!({ "project": project["id"] }))
        .await
        .expect("held run");
    let result = &held["runs"][0]["results"][0];
    assert_eq!(result["ok"], false, "{held}");
    assert!(
        result["error"].as_str().is_some_and(|e| e.contains("hold")),
        "{result}"
    );
    assert_eq!(status(&client, &workspace).await, "todo");

    let runs = client
        .request("automation.runs", json!({ "project": project["id"] }))
        .await
        .expect("runs");
    assert_eq!(runs["runs"].as_array().map(Vec::len), Some(5));
    let failed = client
        .request(
            "automation.runs",
            json!({ "project": project["id"], "failed": true }),
        )
        .await
        .expect("failed runs");
    assert_eq!(failed["runs"].as_array().map(Vec::len), Some(1));
    let shown = client
        .request(
            "automation.show",
            json!({ "run": armed["id"], "scope": true }),
        )
        .await
        .expect("show");
    assert_eq!(shown["scope"]["workspaces"][0]["id"], workspace);
    assert_eq!(shown["record"]["memory_after"], json!({ "runs": 1 }));

    let status_reply = client
        .request("automation.status", json!({}))
        .await
        .expect("status");
    let entry = status_reply["projects"]
        .as_array()
        .and_then(|p| p.iter().find(|p| p["id"] == project["id"]))
        .expect("project status");
    assert_eq!(entry["script_exists"], true);
    assert_eq!(status_reply["running"], false, "no app window is connected");
}

#[tokio::test]
async fn ticks_only_run_while_an_app_window_is_connected() {
    let repo = repo();
    let home = tempfile::tempdir().expect("home");
    let marker = home.path().join("ran");
    let script = home.path().join("tick.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncat > /dev/null\ntouch {}\necho '[]'\n",
            marker.display()
        ),
    )
    .expect("script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::create_dir_all(home.path().join("config")).expect("config");
    std::fs::write(
        home.path().join("config/icarus.json"),
        serde_json::to_vec(&json!({ "automations": { "interval_ms": 5000 } })).expect("json"),
    )
    .expect("user settings");
    let _server = start_server(home.path());
    let (client, _) = Client::connect(home.path(), "icarus-cli")
        .await
        .expect("cli");
    let project = client
        .request("project.add", json!({ "path": repo.path() }))
        .await
        .expect("project");
    write_settings(
        repo.path(),
        &json!({ "automations": { "scripts": { project["name"].as_str().expect("name"): script } } }),
    );
    tokio::time::sleep(Duration::from_millis(5600)).await;
    assert!(!marker.exists(), "a CLI connection doesn't start ticks");

    let (_app, _events) = Client::connect(home.path(), "icarus-app")
        .await
        .expect("app");
    let deadline = Instant::now() + Duration::from_secs(12);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "no tick ran with an app connected"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn generated_types_follow_the_status_and_prompt_settings() {
    let home = tempfile::tempdir().expect("home");
    let config = home.path().join("config");
    std::fs::create_dir_all(config.join("automations")).expect("automations dir");
    let types = config.join("automations/.generated/icarus.d.ts");
    let _server = start_server(home.path());
    let (_client, _) = Client::connect(home.path(), "icarus-cli")
        .await
        .expect("cli");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !types.exists() {
        assert!(Instant::now() < deadline, "no types written on start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        std::fs::read_to_string(&types)
            .expect("types")
            .contains("\"in-review\"")
    );
    // The settings watcher notices changes against its first look, taken once a client is connected.
    tokio::time::sleep(Duration::from_millis(2500)).await;

    std::fs::write(
        config.join("icarus.json"),
        serde_json::to_vec(&json!({
            "workspace": { "statuses": [{ "id": "triage", "label": "Triage" }] },
            "prompts": { "fix-checks": "Fix the checks of #{{pr.number}}" },
        }))
        .expect("json"),
    )
    .expect("settings");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let text = std::fs::read_to_string(&types).expect("types");
        if text.contains("export type StatusId = \"triage\";")
            && text.contains("export type PromptId = \"fix-checks\";")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "types did not follow the settings:\n{text}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn recorded_triggers(dir: &Path) -> Vec<Value> {
    std::fs::read_dir(dir)
        .expect("runs dir")
        .filter_map(|entry| std::fs::read(entry.ok()?.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .map(|scope| scope["trigger"].clone())
        .collect()
}

#[tokio::test]
async fn a_status_change_runs_the_script_once_with_its_event() {
    let repo = repo();
    let home = tempfile::tempdir().expect("home");
    let runs = home.path().join("runs");
    std::fs::create_dir_all(&runs).expect("runs");
    let script = home.path().join("events.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncat > \"{}/$ICARUS_RUN_ID.json\"\necho '[]'\n",
            runs.display()
        ),
    )
    .expect("script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::create_dir_all(home.path().join("config")).expect("config");
    std::fs::write(
        home.path().join("config/icarus.json"),
        serde_json::to_vec(&json!({ "automations": { "interval_ms": 600_000 } })).expect("json"),
    )
    .expect("user settings");
    let _server = start_server(home.path());
    let (client, _) = Client::connect(home.path(), "icarus-cli")
        .await
        .expect("cli");
    let (_app, _events) = Client::connect(home.path(), "icarus-app")
        .await
        .expect("app");
    let project = client
        .request("project.add", json!({ "path": repo.path() }))
        .await
        .expect("project");
    write_settings(
        repo.path(),
        &json!({ "automations": { "scripts": { project["name"].as_str().expect("name"): script } } }),
    );
    let created = client
        .request_timeout(
            "workspace.create",
            json!({ "project": project["id"], "name": "w1" }),
            Duration::from_secs(60),
        )
        .await
        .expect("create");
    let workspace = created["workspace"]["id"]
        .as_str()
        .or(created["id"].as_str())
        .expect("workspace id")
        .to_owned();
    tokio::time::sleep(Duration::from_millis(3000)).await;
    for entry in std::fs::read_dir(&runs).expect("runs dir").flatten() {
        std::fs::remove_file(entry.path()).expect("clear");
    }

    client
        .request(
            "workspace.set",
            json!({ "id": workspace, "status": "in-review" }),
        )
        .await
        .expect("set");
    let deadline = Instant::now() + Duration::from_secs(10);
    while recorded_triggers(&runs).is_empty() {
        if Instant::now() > deadline {
            let recorded = client
                .request("automation.runs", json!({ "project": project["id"] }))
                .await
                .expect("runs");
            panic!("no event run; recorded: {recorded}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let triggers = recorded_triggers(&runs);
    assert_eq!(triggers.len(), 1, "one burst is one run: {triggers:?}");
    let trigger = &triggers[0];
    assert_eq!(trigger["kind"], "event", "{trigger}");
    assert_eq!(trigger["name"], "workspace.status_changed", "{trigger}");
    assert_eq!(trigger["workspace"], workspace);
    assert_eq!(trigger["payload"]["to"], "in-review");
}
