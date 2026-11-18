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

fn repo_with_settings(settings: Value) -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("repo");
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "T"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.path().join("readme.md"), "hello\n").expect("write");
    std::fs::write(repo.path().join(".gitignore"), "icarus.local.json\n").expect("write");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    std::fs::write(
        repo.path().join("icarus.local.json"),
        serde_json::to_vec_pretty(&settings).expect("json"),
    )
    .expect("write settings");
    repo
}

fn tab_types(layout: &Value) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(node: &Value, out: &mut Vec<String>) {
        for child in node["children"].as_array().into_iter().flatten() {
            walk(child, out);
        }
        for tab in node["tabs"].as_array().into_iter().flatten() {
            out.push(tab["type"].as_str().unwrap_or("?").to_owned());
        }
    }
    walk(layout, &mut out);
    out
}

#[tokio::test]
async fn a_workspace_created_with_the_review_layout_puts_its_agent_left_and_the_pr_right() {
    let fake = env!("CARGO_BIN_EXE_icarus-fake-agent");
    let repo = repo_with_settings(json!({ "agents": { "commands": { "claude": [fake] } } }));
    let home = tempfile::tempdir().expect("home");
    let _server = start_server(home.path());
    let (client, _) = Client::connect(home.path(), "icarus-cli")
        .await
        .expect("cli");
    let (_app, mut app_events) = Client::connect(home.path(), "icarus-app")
        .await
        .expect("app");
    let project = client
        .request("project.add", json!({ "path": repo.path() }))
        .await
        .expect("project");

    let pr_url = "https://github.com/acme/widgets/pull/7";
    let created = client
        .request_timeout(
            "workspace.create",
            json!({ "project": project["id"], "name": "review 7", "layout": "review", "pr_url": pr_url }),
            Duration::from_secs(60),
        )
        .await
        .expect("create");
    let agent_id = created["agents"][0]["id"].as_str().expect("agent");
    let layout = &created["layout"]["layout"];
    assert_eq!(tab_types(layout), ["agent", "diff", "browser"]);
    assert_eq!(layout["split"], "row");
    assert_eq!(
        layout["children"][0]["tabs"][0]["agent_id"], agent_id,
        "the workspace's own agent sits in the left tab"
    );
    assert!(
        layout["children"][1]["tabs"][0].get("path").is_none(),
        "nothing changed yet, so the diff entry shows the Source Control panel"
    );
    assert_eq!(layout["children"][1]["tabs"][1]["url"], pr_url);
    assert_eq!(layout["children"][1]["active"], 1, "the PR stays in front");
    assert_eq!(created["layout"]["delivered"], 1);

    let event = loop {
        let event = tokio::time::timeout(Duration::from_secs(5), app_events.recv())
            .await
            .expect("event in time")
            .expect("open stream");
        if event.topic != "agent.state" {
            break event;
        }
    };
    assert_eq!(event.topic, "open_layout");
    assert_eq!(event.payload["layout"], *layout);
    assert_eq!(event.payload["path"], created["path"]);

    let taken = client
        .request("workspace.take_layout", json!({ "id": created["id"] }))
        .await
        .expect("take");
    assert_eq!(taken["layout"], *layout);
    let again = client
        .request("workspace.take_layout", json!({ "id": created["id"] }))
        .await
        .expect("take again");
    assert_eq!(again, Value::Null, "a pending layout applies once");

    let worktree = std::path::PathBuf::from(created["path"].as_str().expect("path"));
    std::fs::write(worktree.join("billing.rs"), "fn total() {}\n").expect("change a file");
    let reapplied = client
        .request_timeout(
            "workspace.layout",
            json!({ "id": created["id"], "layout": "review", "pr_url": pr_url }),
            Duration::from_secs(60),
        )
        .await
        .expect("apply review again");
    let changes = &reapplied["layout"]["children"][1]["tabs"][0];
    assert_eq!(
        changes["path"], "billing.rs",
        "the first changed file opens next to the PR"
    );
    assert_eq!(
        changes["scope"], "base:main",
        "against the workspace's base"
    );
}

#[tokio::test]
async fn custom_layouts_start_prompted_agents_and_wait_for_an_app_that_opens_later() {
    let fake = env!("CARGO_BIN_EXE_icarus-fake-agent");
    let repo = repo_with_settings(json!({
        "agents": { "commands": { "claude": [fake] } },
        "layouts": { "triage": { "split": "row", "sizes": [0.5, 0.5], "children": [
            { "tabs": [{ "type": "agent" }, { "type": "diff" }] },
            { "split": "column", "children": [
                { "tabs": [{ "type": "agent", "prompt": "hello from the layout" }] },
                { "tabs": [{ "type": "explorer" }, { "type": "browser", "url": "{workspace_path}/index.html" }] },
            ]},
        ]}},
    }));
    let home = tempfile::tempdir().expect("home");
    let _server = start_server(home.path());
    let (client, _) = Client::connect(home.path(), "icarus-cli")
        .await
        .expect("cli");
    let project = client
        .request("project.add", json!({ "path": repo.path() }))
        .await
        .expect("project");

    let listed = client
        .request("layout.list", json!({ "project": project["id"] }))
        .await
        .expect("list");
    assert!(listed.get("review").is_some() && listed.get("triage").is_some());
    assert!(
        client
            .request("layout.get", json!({ "name": "nope" }))
            .await
            .is_err()
    );

    let workspace = client
        .request_timeout(
            "workspace.create",
            json!({ "project": project["id"], "name": "triage" }),
            Duration::from_secs(60),
        )
        .await
        .expect("create");
    let applied = client
        .request_timeout(
            "workspace.layout",
            json!({ "id": workspace["id"], "layout": "triage" }),
            Duration::from_secs(60),
        )
        .await
        .expect("apply");
    assert_eq!(applied["delivered"], 0, "no app is connected");
    let layout = &applied["layout"];
    assert_eq!(
        tab_types(layout),
        ["agent", "diff", "agent", "explorer", "browser"]
    );
    let first = layout["children"][0]["tabs"][0]["agent_id"]
        .as_str()
        .expect("bound");
    let second = layout["children"][1]["children"][0]["tabs"][0]["agent_id"]
        .as_str()
        .expect("bound");
    assert_eq!(first, workspace["agents"][0]["id"].as_str().expect("agent"));
    assert_ne!(first, second, "a prompted agent tab starts its own agent");
    client
        .request("agent.wait", json!({ "id": second, "timeout_ms": 10000 }))
        .await
        .expect("prompted agent ran");
    let url = layout["children"][1]["children"][1]["tabs"][1]["url"]
        .as_str()
        .expect("url");
    assert_eq!(
        url,
        format!("{}/index.html", workspace["path"].as_str().expect("path"))
    );

    let pending = client
        .request("workspace.pending_layouts", json!({}))
        .await
        .expect("pending");
    assert_eq!(pending.as_array().map(Vec::len), Some(1));
    assert_eq!(pending[0]["workspace_id"], workspace["id"]);
}

#[tokio::test]
async fn a_new_workspace_opens_with_one_agent_tab_unless_settings_change_the_default() {
    let fake = env!("CARGO_BIN_EXE_icarus-fake-agent");
    let repo = repo_with_settings(json!({ "agents": { "commands": { "claude": [fake] } } }));
    let home = tempfile::tempdir().expect("home");
    let _server = start_server(home.path());
    let (client, _) = Client::connect(home.path(), "icarus-cli")
        .await
        .expect("cli");
    let project = client
        .request("project.add", json!({ "path": repo.path() }))
        .await
        .expect("project");
    let created = client
        .request_timeout(
            "workspace.create",
            json!({ "project": project["id"], "name": "plain" }),
            Duration::from_secs(60),
        )
        .await
        .expect("create");
    assert_eq!(created["layout"]["name"], "default");
    assert_eq!(
        created["layout"]["layout"],
        json!({ "tabs": [{ "type": "agent", "agent_id": created["agents"][0]["id"] }] })
    );

    let bare = client
        .request_timeout(
            "workspace.create",
            json!({ "project": project["id"], "name": "bare", "agent": false }),
            Duration::from_secs(60),
        )
        .await
        .expect("create without agent");
    assert!(
        bare.get("layout").is_none(),
        "no agent asked for, so no agent tab"
    );

    std::fs::write(
        repo.path().join("icarus.local.json"),
        serde_json::to_vec(&json!({
            "agents": { "commands": { "claude": [fake] } },
            "layouts": { "default": { "split": "row", "children": [
                { "tabs": [{ "type": "agent" }] }, { "tabs": [{ "type": "terminal" }] }
            ]}},
        }))
        .expect("json"),
    )
    .expect("rewrite settings");
    let custom = client
        .request_timeout(
            "workspace.create",
            json!({ "project": project["id"], "name": "custom" }),
            Duration::from_secs(60),
        )
        .await
        .expect("create with custom default");
    assert_eq!(
        tab_types(&custom["layout"]["layout"]),
        ["agent", "terminal"]
    );
}

#[tokio::test]
async fn an_invalid_layout_in_settings_is_reported_by_name() {
    let repo = repo_with_settings(json!({ "layouts": { "broken": { "tabs": [] } } }));
    let home = tempfile::tempdir().expect("home");
    let _server = start_server(home.path());
    let (client, _) = Client::connect(home.path(), "icarus-cli")
        .await
        .expect("cli");
    let project = client
        .request("project.add", json!({ "path": repo.path() }))
        .await
        .expect("project");
    let error = client
        .request("layout.list", json!({ "project": project["id"] }))
        .await
        .expect_err("invalid layout");
    assert!(format!("{error:#}").contains("layouts.broken"), "{error:#}");
}

#[tokio::test]
async fn tab_and_pane_changes_queue_in_order_and_listing_asks_the_app() {
    let fake = env!("CARGO_BIN_EXE_icarus-fake-agent");
    let repo = repo_with_settings(json!({ "agents": { "commands": { "claude": [fake] } } }));
    let home = tempfile::tempdir().expect("home");
    let _server = start_server(home.path());
    let (client, _) = Client::connect(home.path(), "icarus-cli")
        .await
        .expect("cli");
    let project = client
        .request("project.add", json!({ "path": repo.path() }))
        .await
        .expect("project");
    let created = client
        .request_timeout(
            "workspace.create",
            json!({ "project": project["id"], "name": "tabs" }),
            Duration::from_secs(60),
        )
        .await
        .expect("create");
    let ws = created["id"].clone();
    let stale: Vec<Value> = serde_json::from_value(
        client
            .request("tab.pending", json!({}))
            .await
            .expect("pending"),
    )
    .expect("list");

    let split = client
        .request(
            "pane.split",
            json!({ "workspace": ws, "direction": "right" }),
        )
        .await
        .expect("split");
    assert_eq!(split["delivered"], 0, "no app, so it waits");
    client
        .request(
            "tab.open",
            json!({ "workspace": ws, "type": "browser", "url": "https://example.com", "pane": "right", "focus": true }),
        )
        .await
        .expect("open");
    client
        .request(
            "tab.move",
            json!({ "workspace": ws, "tab": "42", "pane": "0.1", "index": 0 }),
        )
        .await
        .expect("move");
    client
        .request("pane.close", json!({ "workspace": ws, "pane": "p7" }))
        .await
        .expect("close pane");
    for bad in [
        json!({ "workspace": ws, "direction": "sideways" }),
        json!({ "workspace": ws, "direction": "down", "pane": "middle" }),
    ] {
        assert!(client.request("pane.split", bad).await.is_err());
    }
    assert!(
        client
            .request("tab.focus", json!({ "workspace": ws, "tab": "the agent" }))
            .await
            .is_err()
    );
    assert!(
        client
            .request(
                "tab.open",
                json!({ "workspace": ws, "type": "browser", "url": "x", "focus": true, "background": true }),
            )
            .await
            .is_err()
    );

    let pending: Vec<Value> = serde_json::from_value(
        client
            .request("tab.pending", json!({}))
            .await
            .expect("pending"),
    )
    .expect("list");
    let ops: Vec<&str> = pending[stale.len()..]
        .iter()
        .map(|p| p["tab"]["op"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(
        ops,
        ["split", "open", "move", "close_pane"],
        "in the order made"
    );
    let open = &pending[stale.len() + 1]["tab"];
    assert_eq!(open["pane"], "right");
    assert_eq!(open["focus"], true);
    assert_eq!(open["tab"]["url"], "https://example.com");
    assert_eq!(open["path"], created["path"]);

    let error = client
        .request("tab.list", json!({ "workspace": ws }))
        .await
        .expect_err("no app to ask");
    assert!(format!("{error:#}").contains("not_open"), "{error:#}");

    let (app, mut app_events) = Client::connect(home.path(), "icarus-app")
        .await
        .expect("app");
    let workspace_path = created["path"].clone();
    tokio::spawn(async move {
        while let Some(event) = app_events.recv().await {
            if event.topic != "layout_query" {
                continue;
            }
            let layout = if event.payload["path"] == workspace_path {
                json!({
                    "tree": { "split": "row", "children": [{ "pane": "p1" }, { "pane": "p2" }] },
                    "panes": [
                        { "id": "p1", "path": "0", "active": false, "tabs": [
                            { "id": "11", "type": "agent", "title": "Agent", "active": true },
                        ]},
                        { "id": "p2", "path": "1", "active": true, "tabs": [
                            { "id": "21", "type": "diff", "title": "a.rs", "active": false },
                            { "id": "22", "type": "browser", "title": "PR", "active": true },
                        ]},
                    ],
                })
            } else {
                Value::Null
            };
            app.request(
                "pane.answer",
                json!({ "request": event.payload["request"], "layout": layout }),
            )
            .await
            .expect("answer");
        }
    });

    let tabs = client
        .request("tab.list", json!({ "workspace": ws }))
        .await
        .expect("tab list");
    assert_eq!(tabs["panes"][1]["tabs"][1]["id"], "22");
    let panes = client
        .request("pane.list", json!({ "workspace": ws }))
        .await
        .expect("pane list");
    assert_eq!(panes["tree"]["split"], "row");
    assert_eq!(panes["panes"][1]["tabs"], 2);
    assert_eq!(panes["panes"][1]["active_tab"]["title"], "PR");

    let other = client
        .request_timeout(
            "workspace.create",
            json!({ "project": project["id"], "name": "closed" }),
            Duration::from_secs(60),
        )
        .await
        .expect("create");
    let error = client
        .request("pane.list", json!({ "workspace": other["id"] }))
        .await
        .expect_err("no window shows it");
    assert!(format!("{error:#}").contains("not_open"), "{error:#}");
}
