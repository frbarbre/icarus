//! A folder macOS privacy settings hold (open and git block until the user answers) only slows the
//! request that needs it: everything else keeps answering, and startup never waits on a login shell.

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

fn start_server(home: &Path, env: &[(&str, std::ffi::OsString)]) -> ServerProcess {
    let child = Command::new(env!("CARGO_BIN_EXE_icarus-server"))
        .args(["--home", home.to_str().expect("utf8 home"), "serve"])
        .env("ICARUS_WORKSPACE_ROOT", home.join("workspaces"))
        .env("ICARUS_CONFIG_DIR", home.join("config"))
        .env_remove("ICARUS_KEEP_PATH")
        .env_remove("ICARUS_LOGIN_PATH_RESTARTED")
        .envs(env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    let server = ServerProcess(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(home.join("server.sock").exists() && home.join("server.token").exists()) {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    server
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

fn repo(path: &Path) {
    std::fs::create_dir_all(path).expect("mkdir");
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.email", "t@example.com"]);
    git(path, &["config", "user.name", "T"]);
    git(path, &["config", "commit.gpgsign", "false"]);
    std::fs::write(path.join("readme.md"), "hello\n").expect("write");
    git(path, &["add", "."]);
    git(path, &["commit", "-q", "-m", "init"]);
}

fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write script");
    let mut permissions = std::fs::metadata(path).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(path, permissions).expect("chmod");
}

async fn timed(client: &Client, method: &str, params: Value) -> (anyhow::Result<Value>, Duration) {
    let started = Instant::now();
    let result = client.request(method, params).await;
    (result, started.elapsed())
}

async fn primary_workspace(client: &Client, project: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let list = client
            .request(
                "workspace.list",
                json!({ "project": project, "refresh": true }),
            )
            .await
            .expect("list");
        if let Some(id) = list
            .as_array()
            .and_then(|list| list.first())
            .and_then(|w| w["id"].as_str())
        {
            return id.to_owned();
        }
        assert!(Instant::now() < deadline, "no workspace for {project}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_project_only_slows_the_request_that_reads_it() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join("home");
    let blocked = dir.path().join("held-by-privacy");
    let open = dir.path().join("open-repo");
    repo(&blocked);
    repo(&open);

    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("which git");
    let real_git = String::from_utf8_lossy(&real_git.stdout).trim().to_owned();
    let shims = dir.path().join("bin");
    std::fs::create_dir_all(&shims).expect("shims");
    executable(
        &shims.join("git"),
        "#!/bin/sh\ncase \"$*\" in *held-by-privacy*) [ -e \"$HANG_MARKER\" ] && sleep 60 ;; esac\nexec \"$REAL_GIT\" \"$@\"\n",
    );
    let marker = dir.path().join("hang");
    let path = format!(
        "{}:{}",
        shims.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let _server = start_server(
        &home,
        &[
            ("PATH", path.into()),
            ("ICARUS_KEEP_PATH", "1".into()),
            ("ICARUS_GIT_TIMEOUT_MS", "1500".into()),
            ("REAL_GIT", real_git.into()),
            ("HANG_MARKER", marker.clone().into()),
        ],
    );
    let (client, _events) = Client::connect(&home, "test").await.expect("connect");
    let mut workspaces = Vec::new();
    for root in [&blocked, &open] {
        let project = client
            .request("project.add", json!({ "path": root }))
            .await
            .expect("add project");
        workspaces.push(primary_workspace(&client, project["id"].as_str().expect("id")).await);
    }
    let (held, other) = (workspaces[0].clone(), workspaces[1].clone());

    // Now macOS holds the folder: its settings file never opens and git in it never returns.
    std::fs::write(&marker, "").expect("marker");
    let status = Command::new("mkfifo")
        .arg(blocked.join("icarus.json"))
        .status()
        .expect("mkfifo");
    assert!(status.success());

    let slow = {
        let client = client.clone();
        tokio::spawn(
            async move { timed(&client, "workspace.changes", json!({ "id": held })).await },
        )
    };
    tokio::time::sleep(Duration::from_millis(200)).await;

    for _ in 0..3 {
        let (info, took) = timed(&client, "server.info", json!({})).await;
        info.expect("server.info");
        assert!(took < Duration::from_secs(1), "server.info took {took:?}");
        let (list, took) = timed(&client, "workspace.list", json!({})).await;
        list.expect("workspace.list");
        assert!(
            took < Duration::from_secs(1),
            "workspace.list took {took:?}"
        );
        let (changes, took) = timed(&client, "workspace.changes", json!({ "id": other })).await;
        changes.expect("changes of the other project");
        assert!(took < Duration::from_secs(1), "other changes took {took:?}");
        let started = Instant::now();
        let (fresh, _) = Client::connect(&home, "test-2").await.expect("hello");
        fresh.request("server.info", json!({})).await.expect("info");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "hello took {:?}",
            started.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    let (result, took) = slow.await.expect("join");
    let error = result.expect_err("the held folder cannot answer");
    assert!(
        took < Duration::from_secs(8),
        "blocked request took {took:?}"
    );
    assert!(
        format!("{error:#}").contains("did not finish within"),
        "unclear error: {error:#}"
    );
}

#[test]
fn a_slow_login_shell_does_not_delay_startup() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join("home");
    let shell = dir.path().join("slow-shell");
    executable(&shell, "#!/bin/sh\nexec sleep 30\n");
    let started = Instant::now();
    let _server = start_server(&home, &[("SHELL", shell.into_os_string())]);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "startup took {:?}",
        started.elapsed()
    );
}
