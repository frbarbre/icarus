use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use icarus_client::Client;

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn editing_a_settings_file_outside_the_server_announces_config_changed() {
    let home = tempfile::tempdir().expect("home");
    let config = tempfile::tempdir().expect("config");
    let _server = ServerProcess(
        Command::new(env!("CARGO_BIN_EXE_icarus-server"))
            .args(["--home", home.path().to_str().expect("utf8"), "serve"])
            .env("ICARUS_WORKSPACE_ROOT", home.path().join("workspaces"))
            .env("ICARUS_CONFIG_DIR", config.path())
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
    let (_app, mut events) = Client::connect(home.path(), "icarus-app")
        .await
        .expect("connect");
    // Let the watcher record the starting state before the edit.
    tokio::time::sleep(Duration::from_secs(3)).await;
    std::fs::write(
        config.path().join("icarus.json"),
        r#"{ "sidebar": { "sortBy": "name" } }"#,
    )
    .expect("write settings");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = tokio::time::timeout(remaining, events.recv())
            .await
            .expect("config.changed in time")
            .expect("open stream");
        if event.topic == "config.changed" {
            break;
        }
    }
}
