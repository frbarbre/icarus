//! Worktree discovery: worktrees made by other tools become workspaces, tagged and shown or hidden
//! as Orca does, and removing one never deletes a worktree Icarus didn't create.

use std::path::{Path, PathBuf};
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
    start_server_with(home, &[])
}

fn start_server_with(home: &Path, env: &[(&str, std::ffi::OsString)]) -> ServerProcess {
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
        std::thread::sleep(Duration::from_millis(20));
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

fn add_worktree(repo: &Path, path: &Path, branch: Option<&str>) {
    let path = path.to_str().expect("utf8");
    match branch {
        Some(branch) => git(repo, &["worktree", "add", "-q", "-b", branch, path]),
        None => git(repo, &["worktree", "add", "-q", "--detach", path]),
    };
}

struct Fixture {
    client: std::sync::Arc<Client>,
    project: String,
}

impl Fixture {
    async fn request(&self, method: &str, params: Value) -> Value {
        self.client
            .request_timeout(method, params, Duration::from_secs(60))
            .await
            .unwrap_or_else(|e| panic!("{method}: {e:#}"))
    }

    async fn list(&self, include_hidden: bool) -> Vec<Value> {
        self.request(
            "workspace.list",
            json!({ "project": self.project, "refresh": true, "include_hidden": include_hidden }),
        )
        .await
        .as_array()
        .expect("array")
        .clone()
    }

    async fn names(&self, include_hidden: bool) -> Vec<String> {
        let mut names: Vec<String> = self
            .list(include_hidden)
            .await
            .iter()
            .map(|w| w["name"].as_str().expect("name").to_owned())
            .collect();
        names.sort();
        names
    }

    async fn by_name(&self, name: &str) -> Value {
        self.list(true)
            .await
            .into_iter()
            .find(|w| w["name"] == name)
            .unwrap_or_else(|| panic!("no workspace {name}"))
    }

    async fn inbox(&self) -> Vec<String> {
        let inbox = self
            .request("workspace.inbox", json!({ "project": self.project }))
            .await;
        let mut names: Vec<String> = inbox
            .as_array()
            .expect("array")
            .iter()
            .flat_map(|entry| entry["workspaces"].as_array().cloned().unwrap_or_default())
            .map(|w| w["name"].as_str().expect("name").to_owned())
            .collect();
        names.sort();
        names
    }
}

#[tokio::test]
async fn worktrees_from_other_tools_are_discovered_and_never_deleted_by_accident() {
    let home = tempfile::tempdir().expect("home");
    let home = std::fs::canonicalize(home.path()).expect("canonical home");
    let dirs = tempfile::tempdir().expect("dirs");
    let dirs = std::fs::canonicalize(dirs.path()).expect("canonical");
    let repo = dirs.join("widgets");
    std::fs::create_dir_all(&repo).expect("repo dir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "T"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("readme.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    std::fs::write(
        repo.join(".git").join("info").join("exclude"),
        ".claude/\nicarus.local.json\n",
    )
    .expect("exclude");
    let conductor = dirs.join("conductor");
    std::fs::create_dir_all(home.join("config")).expect("config dir");
    std::fs::write(
        home.join("config").join("icarus.json"),
        json!({ "workspace": { "worktreeSources": [{ "id": "conductor", "root": conductor }] } })
            .to_string(),
    )
    .expect("user settings");

    let _server = start_server(&home);
    let (client, _events) = Client::connect(&home, "test").await.expect("connect");
    let project = client
        .request("project.add", json!({ "path": repo }))
        .await
        .expect("add project");
    let fixture = Fixture {
        client,
        project: project["id"].as_str().expect("id").to_owned(),
    };

    let created = fixture
        .request(
            "workspace.create",
            json!({ "project": fixture.project, "name": "made-here", "agent": false, "base": "main" }),
        )
        .await;
    let root = home.join("workspaces");
    assert_eq!(
        PathBuf::from(created["path"].as_str().expect("path")),
        root.join("widgets").join("made-here"),
        "Icarus still creates in its own folder"
    );
    assert_eq!(created["origin"], "native");
    assert_eq!(created["discovered"], false);

    add_worktree(
        &repo,
        &root.join("widgets").join("by-hand"),
        Some("by-hand"),
    );
    add_worktree(
        &repo,
        &repo.join(".claude").join("worktrees").join("agent-a"),
        Some("agent-a"),
    );
    add_worktree(&repo, &conductor.join("widgets").join("c1"), Some("c1"));
    let fix_login = dirs.join("orca").join("fix-login");
    add_worktree(&repo, &fix_login, Some("fix/login"));
    add_worktree(&repo, &dirs.join("orca").join("poke"), None);

    assert_eq!(
        fixture.names(false).await,
        ["by-hand", "made-here", "widgets"],
        "external and source worktrees are hidden by default"
    );
    assert_eq!(
        fixture.names(true).await,
        [
            "agent-a",
            "by-hand",
            "c1",
            "fix-login",
            "made-here",
            "poke",
            "widgets"
        ],
    );
    let expect = [
        ("widgets", "primary", true, false),
        ("made-here", "native", false, false),
        ("by-hand", "native", true, false),
        ("agent-a", "claude", true, true),
        ("c1", "custom:conductor", true, true),
        ("fix-login", "external", true, true),
        ("poke", "external", true, true),
    ];
    for (name, origin, discovered, hidden) in expect {
        let workspace = fixture.by_name(name).await;
        assert_eq!(workspace["origin"], origin, "{name}");
        assert_eq!(workspace["discovered"], discovered, "{name}");
        assert_eq!(workspace["hidden"], hidden, "{name}");
    }
    let found = fixture.by_name("fix-login").await;
    assert_eq!(found["branch"], "fix/login");
    assert_eq!(
        found["source_label"],
        json!(dirs.join("orca").display().to_string()),
        "external worktrees are grouped by their parent folder"
    );
    assert_eq!(
        fixture.by_name("poke").await["branch"],
        Value::Null,
        "detached"
    );
    assert_eq!(
        fixture.by_name("agent-a").await["source_label"],
        "Claude Code"
    );

    assert_eq!(
        fixture.inbox().await,
        ["c1", "fix-login", "poke"],
        "Claude Code scratch worktrees stay out of the inbox"
    );
    fixture
        .request(
            "workspace.visibility",
            json!({ "id": found["id"], "visibility": "show" }),
        )
        .await;
    fixture
        .request(
            "workspace.inbox_dismiss",
            json!({ "project": fixture.project, "ids": ["poke"] }),
        )
        .await;
    assert_eq!(fixture.inbox().await, ["c1"]);
    assert_eq!(
        fixture.names(false).await,
        ["by-hand", "fix-login", "made-here", "widgets"]
    );

    let shown = fixture
        .request(
            "workspace.show_source",
            json!({ "project": fixture.project, "source": "custom:conductor" }),
        )
        .await;
    assert_eq!(
        shown["key"],
        "project.worktreeVisibilitySourcePreferences.custom.conductor"
    );
    assert!(fixture.inbox().await.is_empty());
    assert!(fixture.names(false).await.contains(&"c1".to_owned()));

    fixture
        .request(
            "config.set",
            json!({ "key": "workspace.external", "value": "show" }),
        )
        .await;
    assert!(fixture.names(false).await.contains(&"poke".to_owned()));
    fixture
        .request(
            "config.set",
            json!({
                "key": "project.externalWorktreeVisibility",
                "value": "hide",
                "layer": "local",
                "project": fixture.project,
            }),
        )
        .await;
    let names = fixture.names(false).await;
    assert!(
        !names.contains(&"poke".to_owned()),
        "the project overrides the default"
    );
    assert!(
        names.contains(&"fix-login".to_owned()),
        "an explicit show stays"
    );

    git(
        &repo,
        &[
            "worktree",
            "remove",
            "--force",
            fix_login.to_str().expect("utf8"),
        ],
    );
    assert!(!fixture.names(true).await.contains(&"fix-login".to_owned()));
    let gone = fixture
        .request("workspace.info", json!({ "id": found["id"] }))
        .await;
    assert!(gone["missing_at_ms"].is_u64(), "kept, by id: {gone}");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            fix_login.to_str().expect("utf8"),
            "fix/login",
        ],
    );
    let back = fixture.by_name("fix-login").await;
    assert_eq!(
        back["id"], found["id"],
        "a re-added worktree gets its record back"
    );
    assert_eq!(back["visibility"], "show");
    assert_eq!(back["missing_at_ms"], Value::Null);

    let removed = fixture
        .request(
            "workspace.remove",
            json!({ "id": found["id"], "force": true }),
        )
        .await;
    assert_eq!(removed["worktree_deleted"], false);
    assert!(fix_login.is_dir(), "a discovered worktree is only hidden");
    assert_eq!(fixture.by_name("fix-login").await["hidden"], true);
    assert!(!fixture.names(false).await.contains(&"fix-login".to_owned()));

    let deleted = fixture
        .request(
            "workspace.remove",
            json!({ "id": found["id"], "force": true, "delete_worktree": true }),
        )
        .await;
    assert_eq!(deleted["worktree_deleted"], true);
    assert!(!fix_login.exists(), "deleted when asked");
    assert_eq!(
        git(&repo, &["branch", "--list", "fix/login"]),
        "fix/login",
        "its branch is kept unless asked"
    );

    let by_hand = fixture.by_name("by-hand").await;
    fixture
        .request("workspace.remove", json!({ "id": by_hand["id"] }))
        .await;
    assert!(
        root.join("widgets").join("by-hand").is_dir(),
        "found in Icarus's folder but not made by Icarus, so kept"
    );

    fixture
        .request("workspace.remove", json!({ "id": created["id"] }))
        .await;
    assert!(!root.join("widgets").join("made-here").exists());
    assert_eq!(
        git(&repo, &["branch", "--list", "made-here"]),
        "",
        "a workspace Icarus created is deleted with its branch, as before"
    );
}

/// A repository macOS hasn't let the server into makes `git worktree list` block; nothing may wait on it.
#[tokio::test]
async fn a_hanging_git_never_blocks_the_list_and_marks_nothing_missing() {
    let home = tempfile::tempdir().expect("home");
    let home = std::fs::canonicalize(home.path()).expect("canonical home");
    let dirs = tempfile::tempdir().expect("dirs");
    let dirs = std::fs::canonicalize(dirs.path()).expect("canonical");
    let repo = dirs.join("widgets");
    std::fs::create_dir_all(&repo).expect("repo dir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "T"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("readme.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let outside = dirs.join("orca").join("fix-login");
    add_worktree(&repo, &outside, Some("fix/login"));

    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("which git");
    let real_git = String::from_utf8_lossy(&real_git.stdout).trim().to_owned();
    let shims = dirs.join("bin");
    std::fs::create_dir_all(&shims).expect("shims");
    let shim = shims.join("git");
    std::fs::write(
        &shim,
        "#!/bin/sh\ncase \"$*\" in *\"worktree list\"*) [ -e \"$HANG_MARKER\" ] && sleep 60 ;; esac\nexec \"$REAL_GIT\" \"$@\"\n",
    )
    .expect("shim");
    let mut permissions = std::fs::metadata(&shim).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&shim, permissions).expect("chmod");
    let marker = dirs.join("hang");
    let path = format!(
        "{}:{}",
        shims.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let _server = start_server_with(
        &home,
        &[
            ("PATH", path.into()),
            ("ICARUS_KEEP_PATH", "1".into()),
            ("REAL_GIT", real_git.into()),
            ("HANG_MARKER", marker.clone().into()),
        ],
    );
    let (client, _events) = Client::connect(&home, "test").await.expect("connect");
    let project = client
        .request("project.add", json!({ "path": repo }))
        .await
        .expect("add project");
    let fixture = Fixture {
        client,
        project: project["id"].as_str().expect("id").to_owned(),
    };
    assert_eq!(fixture.names(true).await, ["fix-login", "widgets"]);

    std::fs::write(&marker, "").expect("marker");
    git(
        &repo,
        &[
            "worktree",
            "remove",
            "--force",
            outside.to_str().expect("utf8"),
        ],
    );
    let started = Instant::now();
    let listed = fixture
        .request(
            "workspace.list",
            json!({ "project": fixture.project, "include_hidden": true }),
        )
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the list is served from what is stored: {:?}",
        started.elapsed()
    );
    assert_eq!(listed.as_array().map(Vec::len), Some(2));

    let started = Instant::now();
    assert_eq!(
        fixture.names(true).await,
        ["fix-login", "widgets"],
        "a scan that timed out marks nothing missing"
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "a refresh waits only until its deadline: {:?}",
        started.elapsed()
    );

    std::fs::remove_file(&marker).expect("unmark");
    assert_eq!(
        fixture.names(true).await,
        ["widgets"],
        "once git answers, the removed worktree drops out"
    );
}

#[tokio::test]
async fn the_main_checkout_is_listed_as_the_primary_workspace_and_cannot_be_removed() {
    let home = tempfile::tempdir().expect("home");
    let home = std::fs::canonicalize(home.path()).expect("canonical home");
    let dirs = tempfile::tempdir().expect("dirs");
    let dirs = std::fs::canonicalize(dirs.path()).expect("canonical");
    let repo = dirs.join("widgets");
    std::fs::create_dir_all(&repo).expect("repo dir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "T"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("readme.md"), "hello\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    let _server = start_server(&home);
    let (client, _events) = Client::connect(&home, "test").await.expect("connect");
    let project = client
        .request("project.add", json!({ "path": repo }))
        .await
        .expect("add project");
    let fixture = Fixture {
        client,
        project: project["id"].as_str().expect("id").to_owned(),
    };

    let primary = fixture.by_name("widgets").await;
    assert_eq!(primary["path"], json!(repo.display().to_string()));
    assert_eq!(primary["branch"], "main");
    assert_eq!(primary["primary"], true);
    assert_eq!(primary["origin"], "primary");
    assert_eq!(primary["hidden"], false, "shown without any setting");
    assert_eq!(primary["source_label"], "Primary");

    for params in [
        json!({ "id": primary["id"] }),
        json!({ "id": primary["id"], "force": true, "delete_worktree": true }),
    ] {
        let refused = fixture
            .client
            .request("workspace.remove", params)
            .await
            .expect_err("the primary workspace can't be removed");
        assert!(
            format!("{refused:#}").contains("main checkout"),
            "{refused:#}"
        );
    }
    assert!(
        repo.join("readme.md").is_file(),
        "the checkout is untouched"
    );
    assert_eq!(fixture.names(false).await, ["widgets"], "and stays listed");
}
