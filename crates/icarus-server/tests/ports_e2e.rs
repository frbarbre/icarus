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

fn start_server(home: &Path, config: &Path) -> ServerProcess {
    let child = Command::new(env!("CARGO_BIN_EXE_icarus-server"))
        .args(["--home", home.to_str().expect("utf8 home"), "serve"])
        .env("ICARUS_WORKSPACE_ROOT", home.join("workspaces"))
        .env("ICARUS_CONFIG_DIR", config)
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("free port")
}

async fn service_on(
    client: &Client,
    workspace: &str,
    port: u16,
    until: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let listed = client
            .request("ports.list", json!({ "workspace": workspace }))
            .await
            .expect("ports.list");
        let found = listed
            .as_array()
            .expect("array")
            .iter()
            .find(|s| s["port"] == port)
            .cloned();
        if let Some(service) = found.filter(|s| until(s)) {
            return service;
        }
        assert!(
            Instant::now() < deadline,
            "no matching service on {port}: {listed}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test]
async fn a_dev_server_in_a_workspace_terminal_is_found_classified_and_primary() {
    let home = tempfile::tempdir().expect("home");
    let config = tempfile::tempdir().expect("config");
    let repo = tempfile::tempdir().expect("repo");
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "T"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.path().join("readme.md"), "hello\n").expect("write");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    let _server = start_server(home.path(), config.path());

    let (client, mut events) = Client::connect(home.path(), "test").await.expect("connect");
    let project = client
        .request("project.add", json!({ "path": repo.path() }))
        .await
        .expect("add project");
    let workspace = client
        .request(
            "workspace.create",
            json!({ "project": project["id"], "name": "ports", "agent": false }),
        )
        .await
        .expect("create workspace");
    let id = workspace["id"].as_str().expect("id").to_string();
    let path = workspace["path"].as_str().expect("path").to_string();
    let web = Path::new(&path).join("web");
    std::fs::create_dir_all(&web).expect("web dir");
    std::fs::write(
        web.join("index.html"),
        "<!doctype html><html><head><script type=\"module\" src=\"/@vite/client\"></script></head><body><div id=\"app\"></div></body></html>\n",
    )
    .expect("index");

    let port = free_port();
    // Runs from `/` so only terminal ancestry (not the cwd) ties it to the workspace.
    let script = format!(
        "cd / && exec python3 -m http.server {port} --bind 127.0.0.1 --directory '{}'",
        web.display()
    );
    let terminal = client
        .request(
            "terminal.create",
            json!({ "cwd": path, "workspace": id, "command": ["/bin/sh", "-c", script] }),
        )
        .await
        .expect("terminal");
    let terminal_id = terminal["id"].as_str().expect("terminal id").to_string();

    let service = service_on(&client, &id, port, |s| s["source"] == "advertised").await;
    assert_eq!(service["kind"], "frontend", "{service}");
    assert_eq!(service["framework"], "vite", "{service}");
    assert_eq!(service["confidence"], "high", "{service}");
    assert_eq!(service["attribution"], "terminal", "{service}");
    assert_eq!(service["terminal"], terminal_id.as_str());
    assert_eq!(service["primary"], true);
    let advertised = format!("http://127.0.0.1:{port}/");
    assert_eq!(service["url"], advertised.as_str());
    assert!(
        service["urls"]
            .as_array()
            .expect("urls")
            .contains(&json!(format!("http://localhost:{port}"))),
        "{service}"
    );

    let primary = client
        .request("ports.primary", json!({ "workspace": id }))
        .await
        .expect("primary");
    assert_eq!(primary["url"], advertised.as_str());
    let api = client
        .request("ports.primary", json!({ "workspace": id, "kind": "api" }))
        .await
        .expect("primary api");
    assert!(api["url"].is_null(), "{api}");

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let event = tokio::time::timeout_at(deadline.into(), events.recv())
            .await
            .expect("ports.changed in time")
            .expect("event stream open");
        if event.topic == "ports.changed" && event.payload["workspace"] == id.as_str() {
            break;
        }
    }

    std::fs::write(
        repo.path().join("icarus.json"),
        json!({
            "ports": {
                "labels_command": format!("echo 'Storefront: http://127.0.0.1:{port}/shop'"),
                "rules": [{ "match": { "label_regex": "^Store" }, "label": "Shop" }]
            }
        })
        .to_string(),
    )
    .expect("icarus.json");
    let labelled = service_on(&client, &id, port, |s| s["source"] == "labels_command").await;
    assert_eq!(labelled["label"], "Shop", "{labelled}");
    assert_eq!(labelled["url"], format!("http://127.0.0.1:{port}/shop"));
    assert_eq!(labelled["framework"], "vite");

    client
        .request(
            "terminal.kill",
            json!({ "id": terminal_id, "remove": true }),
        )
        .await
        .expect("kill");
}
