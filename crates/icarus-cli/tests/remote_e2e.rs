use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use icarus_client::Client;
use icarus_client::servers::SavedServers;
use serde_json::{Value, json};

fn sibling(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_BIN_EXE_icarus")).with_file_name(name);
    assert!(
        path.exists(),
        "{name} not built; run tests for the whole workspace"
    );
    path
}

struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Machine {
    _dir: tempfile::TempDir,
    home: PathBuf,
    config: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).expect("config dir");
        Self {
            _dir: dir,
            home,
            config,
        }
    }

    fn server_command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(sibling("icarus-server"));
        command
            .args(["--home", self.home.to_str().expect("utf8")])
            .args(args)
            .env("ICARUS_WORKSPACE_ROOT", self.home.join("workspaces"))
            .env("ICARUS_CONFIG_DIR", &self.config);
        command
    }

    fn serve(&self) -> (Guard, String) {
        let guard = Guard(
            self.server_command(&["serve", "--listen", "127.0.0.1:0"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("server"),
        );
        let addr_file = self.home.join("server.addr");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(&addr_file)
                && let Some(line) = text.lines().next()
                && self.home.join("server.token").exists()
            {
                return (guard, line.to_string());
            }
            assert!(Instant::now() < deadline, "server did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn icarus(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_icarus"))
            .args(["--home", self.home.to_str().expect("utf8")])
            .args(args)
            .env("ICARUS_CONFIG_DIR", &self.config)
            .output()
            .expect("run icarus")
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let output = self.icarus(&all);
        assert!(
            output.status.success(),
            "icarus {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("json output")
    }

    fn fails(&self, args: &[&str]) -> String {
        let output = self.icarus(args);
        assert!(!output.status.success(), "icarus {args:?} should fail");
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn pair_code(&self, ttl: Option<u64>) -> String {
        let ttl = ttl.map(|t| t.to_string());
        let mut args = vec!["server", "pair"];
        if let Some(ttl) = &ttl {
            args.extend(["--ttl", ttl]);
        }
        let value = self.json(&args);
        value["code"].as_str().expect("code").to_string()
    }
}

#[test]
fn pairing_with_a_code_saves_a_token_that_later_commands_reuse() {
    let server = Machine::new();
    let laptop = Machine::new();
    let (_guard, addr) = server.serve();

    let code = server.pair_code(None);
    assert_eq!(code.len(), 9, "XXXX-XXXX: {code}");
    let added = laptop.json(&[
        "remote", "add", "desk", &addr, "--code", &code, "--label", "laptop", "--ssh", "desk.lan",
    ]);
    assert_eq!(added["address"], json!(addr));
    let fingerprint = added["fingerprint"].as_str().expect("fingerprint");
    let info = server.json(&["server", "status"]);
    assert_eq!(info["tls_fingerprint"], json!(fingerprint));
    assert_eq!(info["listen"], json!([addr]));

    let saved = std::fs::read_to_string(laptop.config.join("servers.json")).expect("saved");
    assert!(saved.contains("\"token\""));
    let listed = laptop.json(&["remote", "list"]);
    assert_eq!(listed[0]["name"], json!("desk"));
    assert_eq!(
        listed[0]["ssh"],
        json!("desk.lan"),
        "the app opens this server's worktrees over SSH"
    );
    assert!(listed[0].get("token").is_none(), "list never prints tokens");

    // The laptop has no server of its own, so these can only succeed over TCP.
    for _ in 0..2 {
        assert_eq!(
            laptop.json(&["--server", "desk", "project", "list"]),
            json!([])
        );
    }
    let remote_info = laptop.json(&["--server", "desk", "server", "status"]);
    assert_eq!(remote_info["home"], json!(server.home));

    let clients = server.json(&["server", "clients"]);
    assert_eq!(clients.as_array().map(Vec::len), Some(1));
    assert_eq!(clients[0]["label"], json!("laptop"));

    let refused = laptop.fails(&["--server", "desk", "server", "pair"]);
    assert!(
        refused.contains("only works on the server's own machine"),
        "{refused}"
    );

    laptop.json(&["remote", "remove", "desk"]);
    assert!(
        laptop
            .fails(&["--server", "desk", "project", "list"])
            .contains("no saved server")
    );
}

#[test]
fn wrong_reused_and_expired_codes_are_refused() {
    let server = Machine::new();
    let laptop = Machine::new();
    let (_guard, addr) = server.serve();

    let wrong = laptop.fails(&["remote", "add", "desk", &addr, "--code", "AAAA-AAAA"]);
    assert!(wrong.contains("wrong, expired or already used"), "{wrong}");

    let code = server.pair_code(None);
    laptop.json(&["remote", "add", "desk", &addr, "--code", &code]);
    let reused = laptop.fails(&["remote", "add", "again", &addr, "--code", &code]);
    assert!(
        reused.contains("wrong, expired or already used"),
        "{reused}"
    );

    let short = server.pair_code(Some(1));
    std::thread::sleep(Duration::from_millis(1500));
    let expired = laptop.fails(&["remote", "add", "late", &addr, "--code", &short]);
    assert!(
        expired.contains("wrong, expired or already used"),
        "{expired}"
    );

    let saved = SavedServers::load_from(&laptop.config.join("servers.json")).expect("load");
    let names: Vec<&str> = saved.servers.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["desk"]);
}

#[tokio::test]
async fn revoking_a_client_closes_its_connections_and_voids_its_token() {
    let server = Machine::new();
    let laptop = Machine::new();
    let (_guard, addr) = server.serve();

    let code = server.pair_code(None);
    laptop.json(&["remote", "add", "desk", &addr, "--code", &code]);
    let saved = SavedServers::load_from(&laptop.config.join("servers.json")).expect("load");
    let desk = saved.get("desk").expect("desk").clone();

    let (live, _events) = Client::connect_saved(&desk, "test").await.expect("connect");
    live.request("project.list", json!({}))
        .await
        .expect("request over TCP");

    let mut tampered = desk.clone();
    tampered.fingerprint = Some("00".repeat(32));
    let pinned = Client::connect_saved(&tampered, "test")
        .await
        .err()
        .expect("a different certificate is refused");
    assert!(
        format!("{pinned:#}").contains("certificate changed"),
        "{pinned:#}"
    );

    let id = desk.client_id.clone().expect("client id");
    let revoked = server.json(&["server", "revoke", &id]);
    assert_eq!(revoked["closed_connections"], json!(1));

    let after = tokio::time::timeout(
        Duration::from_secs(5),
        live.request("project.list", json!({})),
    )
    .await
    .expect("the closed connection answers at once");
    assert!(after.is_err(), "revoked connection still served requests");

    let rejected = laptop.fails(&["--server", "desk", "project", "list"]);
    assert!(rejected.contains("unknown or revoked"), "{rejected}");
    assert_eq!(server.json(&["server", "clients"]), json!([]));

    // The local socket and its token are unaffected.
    assert_eq!(server.json(&["project", "list"]), json!([]));
}

#[test]
fn listening_on_a_public_address_needs_allow_public() {
    let server = Machine::new();
    let output = server
        .server_command(&["serve", "--listen", "8.8.8.8:7878"])
        .output()
        .expect("run server");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--allow-public"), "{stderr}");
    assert!(stderr.contains("Tailscale"), "{stderr}");
    assert!(!server.home.join("server.sock").exists());
}
