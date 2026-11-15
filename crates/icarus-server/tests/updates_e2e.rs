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

fn start(home: &Path, config: &Path, fixture: &Path) -> ServerProcess {
    let child = Command::new(env!("CARGO_BIN_EXE_icarus-server"))
        .args(["--home", home.to_str().expect("utf8"), "serve"])
        .env("ICARUS_WORKSPACE_ROOT", home.join("workspaces"))
        .env("ICARUS_CONFIG_DIR", config)
        .env("ICARUS_UPDATE_FIXTURE", fixture)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(home.join("server.sock").exists() && home.join("server.token").exists()) {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    ServerProcess(child)
}

fn release(tag: &str) -> Value {
    json!({
        "tag_name": tag,
        "html_url": format!("https://github.com/frbarbre/icarus/releases/tag/{tag}"),
        "body": "Fixes and features.",
        "draft": false,
        "prerelease": false,
        "assets": [{ "name": "SHA256SUMS", "browser_download_url": "https://example.invalid/SHA256SUMS", "size": 1 }]
    })
}

async fn update_notifications(client: &Client) -> Vec<Value> {
    let listed = client
        .request("notification.list", json!({}))
        .await
        .expect("list");
    listed
        .as_array()
        .expect("array")
        .iter()
        .filter(|n| n["kind"] == "update_available")
        .cloned()
        .collect()
}

#[tokio::test]
async fn update_check_reports_a_newer_release_and_notifies_once_per_version() {
    let home = tempfile::tempdir().expect("home");
    let config = tempfile::tempdir().expect("config");
    std::fs::write(config.path().join("icarus.json"), "{}").expect("settings");
    let fixture = home.path().join("release.json");
    std::fs::write(&fixture, "null").expect("fixture");
    let _server = start(home.path(), config.path(), &fixture);
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");

    let none = client
        .request("update.check", json!({}))
        .await
        .expect("check");
    assert_eq!(none["latest"], Value::Null);
    assert_eq!(none["update_available"], false);
    assert_eq!(none["source"], "fixture");
    let current = none["current"].as_str().expect("current").to_owned();
    let info = client
        .request("server.info", json!({}))
        .await
        .expect("info");
    assert_eq!(info["version"], current.as_str());

    std::fs::write(&fixture, release("v99.0.0").to_string()).expect("fixture");
    for _ in 0..2 {
        let newer = client
            .request("update.check", json!({}))
            .await
            .expect("check");
        assert_eq!(newer["update_available"], true);
        assert_eq!(newer["latest"], "99.0.0");
        assert_eq!(newer["current"], current.as_str());
        assert_eq!(
            newer["url"],
            "https://github.com/frbarbre/icarus/releases/tag/v99.0.0"
        );
        assert_eq!(newer["notes"], "Fixes and features.");
        assert_eq!(newer["assets"][0]["name"], "SHA256SUMS");
    }
    let found = update_notifications(&client).await;
    assert_eq!(found.len(), 1, "one notification per version: {found:?}");
    assert_eq!(found[0]["title"], "Update available — restart to update");
    assert_eq!(found[0]["data"]["latest"], "99.0.0");

    std::fs::write(&fixture, release(&format!("v{current}")).to_string()).expect("fixture");
    let same = client
        .request("update.check", json!({}))
        .await
        .expect("check");
    assert_eq!(same["update_available"], false);

    let kinds = client
        .request("notification.kinds", json!({}))
        .await
        .expect("kinds");
    assert_eq!(kinds["kinds"]["update_available"], true);
}

#[tokio::test]
async fn update_notifications_follow_their_toggle() {
    let home = tempfile::tempdir().expect("home");
    let config = tempfile::tempdir().expect("config");
    std::fs::write(
        config.path().join("icarus.json"),
        json!({ "notifications": { "update_available": false } }).to_string(),
    )
    .expect("settings");
    let fixture = home.path().join("release.json");
    std::fs::write(&fixture, release("v99.0.0").to_string()).expect("fixture");
    let _server = start(home.path(), config.path(), &fixture);
    let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
    let check = client
        .request("update.check", json!({}))
        .await
        .expect("check");
    assert_eq!(check["update_available"], true);
    assert!(update_notifications(&client).await.is_empty());
}
