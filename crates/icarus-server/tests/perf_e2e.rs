//! Latency budgets for what the app does all the time, on a repository with thousands of files.
//! Each measurement prints a `PERF` line that `scripts/dev/perf.sh` collects into a table; the
//! budgets leave room for slow CI machines and debug builds.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use icarus_client::Client;
use serde_json::{Value, json};

const FILES: usize = 4000;
const MODIFIED: usize = 300;
const UNTRACKED: usize = 50;

struct ServerProcess {
    child: Child,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_server(home: &Path, env: &[(&str, std::ffi::OsString)]) -> ServerProcess {
    let _ = std::fs::remove_file(home.join("server.sock"));
    let _ = std::fs::remove_file(home.join("server.token"));
    let child = Command::new(env!("CARGO_BIN_EXE_icarus-server"))
        .args(["--home", home.to_str().expect("utf8 home"), "serve"])
        .env("ICARUS_WORKSPACE_ROOT", home.join("workspaces"))
        .env("ICARUS_CONFIG_DIR", home.join("config"))
        .envs(env.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(home.join("server.sock").exists() && home.join("server.token").exists()) {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(5));
    }
    ServerProcess { child }
}

fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn tempdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = std::fs::canonicalize(dir.path()).expect("canonical");
    (dir, path)
}

/// A repository of `FILES` committed files with one worktree where `MODIFIED` are changed and
/// `UNTRACKED` are new; returns the worktree.
fn large_repo(dirs: &Path, home: &Path) -> PathBuf {
    let repo = dirs.join("monorepo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "T"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    for index in 0..FILES {
        let dir = repo.join(format!("pkg{}/src", index % 40));
        std::fs::create_dir_all(&dir).expect("dir");
        let body: String = (0..20)
            .map(|line| format!("fn f{index}_{line}() -> u32 {{ {line} }}\n"))
            .collect();
        std::fs::write(dir.join(format!("m{index}.rs")), body).expect("write");
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let worktree = home.join("workspaces").join("monorepo").join("feature");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            worktree.to_str().expect("utf8"),
        ],
    );
    for index in 0..MODIFIED {
        let path = worktree.join(format!("pkg{}/src/m{index}.rs", index % 40));
        let mut body = std::fs::read_to_string(&path).expect("read");
        body.push_str("fn added() {}\n");
        std::fs::write(path, body).expect("modify");
    }
    for index in 0..UNTRACKED {
        std::fs::write(
            worktree.join(format!("pkg0/src/new{index}.rs")),
            "fn new() {}\n",
        )
        .expect("untracked");
    }
    repo
}

async fn request(client: &Client, method: &str, params: Value) -> Value {
    client
        .request_timeout(method, params, Duration::from_secs(60))
        .await
        .unwrap_or_else(|e| panic!("{method}: {e:#}"))
}

async fn timed(client: &Client, method: &str, params: Value) -> Duration {
    let started = Instant::now();
    request(client, method, params).await;
    started.elapsed()
}

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

/// Prints the measurement for the perf table and fails when it is over budget.
fn check(name: &str, measured: Duration, budget: Duration) {
    println!(
        "PERF\tserver\t{name}\t{:.1}\t{}",
        measured.as_secs_f64() * 1000.,
        budget.as_millis()
    );
    assert!(
        measured <= budget,
        "{name}: {measured:?} is over its {budget:?} budget"
    );
}

fn feature_id(listed: &Value) -> Option<String> {
    listed
        .as_array()?
        .iter()
        .find(|workspace| workspace["name"] == "feature")
        .and_then(|workspace| workspace["id"].as_str())
        .map(str::to_owned)
}

async fn workspace_id(client: &Client) -> String {
    let listed = request(client, "workspace.list", json!({ "refresh": true })).await;
    feature_id(&listed).unwrap_or_else(|| panic!("the worktree is listed: {listed}"))
}

#[tokio::test]
async fn reads_on_a_large_repository_stay_within_budget() {
    let (_home_dir, home) = tempdir();
    let (_dirs_dir, dirs) = tempdir();
    let repo = large_repo(&dirs, &home);
    let _server = start_server(&home, &[]);
    let (client, _events) = Client::connect(&home, "test").await.expect("connect");
    request(&client, "project.add", json!({ "path": repo })).await;
    let id = workspace_id(&client).await;

    let first = timed(&client, "workspace.changes", json!({ "id": id })).await;
    check(
        "workspace.changes (first)",
        first,
        Duration::from_millis(2000),
    );
    let listing = request(&client, "workspace.changes", json!({ "id": id })).await;
    let rows = listing["rows"].as_array().map(Vec::len).unwrap_or_default();
    assert_eq!(rows, MODIFIED + UNTRACKED, "every change is listed");

    let mut changes = Vec::new();
    let mut status = Vec::new();
    let mut list = Vec::new();
    for _ in 0..7 {
        changes.push(timed(&client, "workspace.changes", json!({ "id": id })).await);
        status.push(timed(&client, "git.status", json!({ "workspace": id })).await);
    }
    for _ in 0..30 {
        list.push(timed(&client, "workspace.list", json!({})).await);
    }
    check(
        "workspace.changes (median)",
        median(&mut changes),
        Duration::from_millis(1000),
    );
    check(
        "git.status (median)",
        median(&mut status),
        Duration::from_millis(500),
    );
    check(
        "workspace.list (median)",
        median(&mut list),
        Duration::from_millis(50),
    );
}

#[tokio::test]
async fn concurrent_identical_reads_share_one_git_run() {
    let (_home_dir, home) = tempdir();
    let (_dirs_dir, dirs) = tempdir();
    let repo = large_repo(&dirs, &home);
    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("which git");
    let real_git = String::from_utf8_lossy(&real_git.stdout).trim().to_owned();
    let shims = dirs.join("bin");
    std::fs::create_dir_all(&shims).expect("shims");
    let shim = shims.join("git");
    let log = dirs.join("git.log");
    // Each git run is slowed down so the concurrent requests surely overlap.
    std::fs::write(
        &shim,
        "#!/bin/sh\necho \"$*\" >> \"$GIT_LOG\"\nsleep 0.05\nexec \"$REAL_GIT\" \"$@\"\n",
    )
    .expect("shim");
    let mut permissions = std::fs::metadata(&shim).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&shim, permissions).expect("chmod");
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
            ("REAL_GIT", real_git.into()),
            ("GIT_LOG", log.clone().into()),
        ],
    );
    let (first, _events) = Client::connect(&home, "test").await.expect("connect");
    let (second, _events) = Client::connect(&home, "test").await.expect("connect");
    request(&first, "project.add", json!({ "path": repo })).await;
    let id = workspace_id(&first).await;
    let runs = || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count()
    };

    for (method, params) in [
        ("workspace.changes", json!({ "id": id })),
        ("git.status", json!({ "workspace": id })),
    ] {
        request(&first, method, params.clone()).await;
        std::fs::remove_file(&log).ok();
        let started = Instant::now();
        request(&first, method, params.clone()).await;
        let alone = started.elapsed();
        let one = runs();
        assert!(one > 0, "{method} runs git");

        std::fs::remove_file(&log).ok();
        let started = Instant::now();
        let calls: Vec<_> = (0..8)
            .map(|index| {
                let client: Arc<Client> = if index % 2 == 0 {
                    first.clone()
                } else {
                    second.clone()
                };
                let params = params.clone();
                tokio::spawn(async move { request(&client, method, params).await })
            })
            .collect();
        let mut answers = Vec::new();
        for call in calls {
            answers.push(call.await.expect("request"));
        }
        let together = started.elapsed();
        assert!(
            answers.windows(2).all(|pair| pair[0] == pair[1]),
            "{method}: every caller got the same answer"
        );
        assert_eq!(
            runs(),
            one,
            "{method}: eight identical requests from two windows ran git as often as one"
        );
        check(
            &format!(
                "{method} x8 concurrent (one alone: {:.0} ms)",
                alone.as_secs_f64() * 1000.
            ),
            together,
            alone * 2 + Duration::from_millis(200),
        );
    }
}

#[tokio::test]
async fn notes_add_and_remove_and_tell_other_windows_quickly() {
    let (_home_dir, home) = tempdir();
    let (_dirs_dir, dirs) = tempdir();
    let repo = large_repo(&dirs, &home);
    let _server = start_server(&home, &[]);
    let (client, _events) = Client::connect(&home, "test").await.expect("connect");
    let (_watcher, mut events) = Client::connect(&home, "icarus-app").await.expect("connect");
    request(&client, "project.add", json!({ "path": repo })).await;
    let id = workspace_id(&client).await;

    let mut adds = Vec::new();
    let mut seen = Vec::new();
    let mut removes = Vec::new();
    for index in 0..20 {
        let started = Instant::now();
        let note = request(
            &client,
            "note.add",
            json!({
                "workspace": id,
                "path": format!("pkg{}/src/m{index}.rs", index % 40),
                "line": 2,
                "body": "Rename this",
            }),
        )
        .await;
        adds.push(started.elapsed());
        let note_id = note["id"].clone();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("note.changed reaches the other window")
                .expect("events open");
            if event.topic == "note.changed" && event.payload["id"] == note_id {
                break;
            }
        }
        seen.push(started.elapsed());
        removes.push(timed(&client, "note.remove", json!({ "id": note_id })).await);
    }
    check(
        "note.add (median)",
        median(&mut adds),
        Duration::from_millis(50),
    );
    check(
        "note.add seen by another window (median)",
        median(&mut seen),
        Duration::from_millis(80),
    );
    check(
        "note.remove (median)",
        median(&mut removes),
        Duration::from_millis(50),
    );
}

#[tokio::test]
async fn a_restarted_server_answers_a_reconnecting_window_quickly() {
    let (_home_dir, home) = tempdir();
    let (_dirs_dir, dirs) = tempdir();
    let repo = large_repo(&dirs, &home);
    let server = start_server(&home, &[]);
    {
        let (client, _events) = Client::connect(&home, "test").await.expect("connect");
        request(&client, "project.add", json!({ "path": repo })).await;
        workspace_id(&client).await;
    }
    drop(server);

    let started = Instant::now();
    let _server = start_server(&home, &[]);
    let (client, _events) = Client::connect(&home, "icarus-app")
        .await
        .expect("reconnect");
    let connected = started.elapsed();
    let listed = request(&client, "workspace.list", json!({})).await;
    let answered = started.elapsed();
    assert!(feature_id(&listed).is_some(), "the card is there at once");
    check(
        "restart to connected",
        connected,
        Duration::from_millis(1500),
    );
    check(
        "restart to workspace.list",
        answered,
        Duration::from_millis(2000),
    );
    let started = Instant::now();
    let (_client, _events) = Client::connect(&home, "icarus-app").await.expect("connect");
    check(
        "connect to a running server",
        started.elapsed(),
        Duration::from_millis(100),
    );
}
