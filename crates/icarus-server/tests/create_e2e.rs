//! `workspace.create` end to end: pull request start points, cancelling, the trust prompt for a
//! repository's committed scripts, `defaultTabs`, and Orca's shared paths.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use icarus_client::Client;
use serde_json::{Value, json};
use tempfile::TempDir;

struct ServerProcess {
    child: Child,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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

fn commit_file(repo: &Path, name: &str, text: &str) {
    std::fs::write(repo.join(name), text).expect("write");
    git(repo, &["add", name]);
    git(repo, &["commit", "-q", "-m", name]);
}

struct World {
    home: TempDir,
    _repos: TempDir,
    repo: PathBuf,
    _server: ServerProcess,
    client: Arc<Client>,
    project: String,
}

impl World {
    /// A project whose GitHub identity is acme/widgets while fetches go to a local bare repository.
    async fn new(files: &[(&str, &str)], fixtures: Value) -> Self {
        let home = tempfile::tempdir().expect("home");
        let repos = tempfile::tempdir().expect("repos");
        let origin = repos.path().join("origin.git");
        let repo = repos.path().join("widgets");
        std::fs::create_dir_all(&repo).expect("repo dir");
        git(
            repos.path(),
            &["init", "-q", "--bare", "-b", "main", "origin.git"],
        );
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "me@example.com"]);
        git(&repo, &["config", "user.name", "Me"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join(".gitignore"), "icarus.local.json\n").expect("ignore");
        for (name, text) in files {
            if let Some(parent) = Path::new(name).parent() {
                std::fs::create_dir_all(repo.join(parent)).expect("dir");
            }
            std::fs::write(repo.join(name), text).expect("write");
        }
        std::fs::write(repo.join("readme.md"), "hello\n").expect("write");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        let settings = json!({
            "agents": { "commands": { "claude": [env!("CARGO_BIN_EXE_icarus-fake-agent")] } },
        });
        std::fs::write(repo.join("icarus.local.json"), settings.to_string()).expect("settings");
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        );
        let fixtures_path = home.path().join("gh-fixtures.json");
        std::fs::write(&fixtures_path, fixtures.to_string()).expect("fixtures");
        let child = Command::new(env!("CARGO_BIN_EXE_icarus-server"))
            .args(["--home", home.path().to_str().expect("utf8"), "serve"])
            .env("ICARUS_WORKSPACE_ROOT", home.path().join("workspaces"))
            .env("ICARUS_CONFIG_DIR", home.path().join("config"))
            .env("ICARUS_GH_FIXTURES", &fixtures_path)
            .env("SHELL", "/bin/sh")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn server");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(home.path().join("server.sock").exists()
            && home.path().join("server.token").exists())
        {
            assert!(Instant::now() < deadline, "server did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        let (client, _events) = Client::connect(home.path(), "test").await.expect("connect");
        let project = client
            .request("project.add", json!({ "path": repo, "name": "widgets" }))
            .await
            .expect("project");
        git(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                origin.to_str().expect("utf8"),
            ],
        );
        git(&repo, &["push", "-q", "origin", "main"]);
        Self {
            project: project["id"].as_str().expect("id").to_owned(),
            home,
            _repos: repos,
            repo,
            _server: ServerProcess { child },
            client,
        }
    }

    async fn create(&self, extra: Value) -> Result<Value, String> {
        let mut params = json!({ "project": self.project, "agent": false });
        for (key, value) in extra.as_object().into_iter().flatten() {
            params[key] = value.clone();
        }
        self.client
            .request_timeout("workspace.create", params, Duration::from_secs(60))
            .await
            .map_err(|e| format!("{e:#}"))
    }

    async fn request(&self, method: &str, params: Value) -> Value {
        self.client
            .request(method, params)
            .await
            .unwrap_or_else(|e| panic!("{method}: {e:#}"))
    }

    async fn workspace_count(&self) -> usize {
        self.request("workspace.list", json!({}))
            .await
            .as_array()
            .map_or(0, Vec::len)
    }

    fn workspaces_dir(&self) -> PathBuf {
        self.home.path().join("workspaces").join("widgets")
    }
}

fn path_of(workspace: &Value) -> PathBuf {
    PathBuf::from(workspace["path"].as_str().expect("path"))
}

fn pull(number: u64, branch: &str, head: &str) -> Value {
    json!({
        "id": format!("PR_{number}"),
        "number": number,
        "title": format!("Pull request {number}"),
        "url": format!("https://github.com/acme/widgets/pull/{number}"),
        "state": "OPEN",
        "isDraft": false,
        "headRefName": branch,
        "baseRefName": "main",
        "headRefOid": head,
        "headRef": { "name": branch },
        "baseRef": { "name": "main" },
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "CLEAN",
        "author": { "login": "me" },
        "repository": { "name": "widgets", "owner": { "login": "acme" } },
    })
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn a_pull_request_start_point_checks_out_its_head_branch_on_its_base() {
    let world = World::new(&[], json!({})).await;
    git(&world.repo, &["checkout", "-q", "-b", "feature/login"]);
    commit_file(&world.repo, "login.txt", "login\n");
    let head = git(&world.repo, &["rev-parse", "HEAD"]);
    git(
        &world.repo,
        &[
            "push",
            "-q",
            "origin",
            "HEAD:refs/pull/7/head",
            "feature/login",
        ],
    );
    git(&world.repo, &["checkout", "-q", "main"]);
    git(&world.repo, &["branch", "-q", "-D", "feature/login"]);
    std::fs::write(
        world.home.path().join("gh-fixtures.json"),
        json!({ "pulls": [pull(7, "feature/login", &head)] }).to_string(),
    )
    .expect("fixtures");

    let created = world.create(json!({ "pr": 7 })).await.expect("create");
    let worktree = path_of(&created);
    assert_eq!(
        created["branch"], "feature/login",
        "the head branch, so the card finds the PR"
    );
    assert_eq!(created["base_ref"], "origin/main");
    assert_eq!(created["title"], "Pull request 7");
    assert_eq!(created["pull_request"]["number"], 7);
    assert_eq!(created["name"], "feature-login");
    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git(&worktree, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/feature/login"
    );

    let by_url = world
        .create(json!({ "pr": "https://github.com/acme/widgets/pull/7", "name": "again", "branch": "login-copy" }))
        .await
        .expect("by url");
    assert_eq!(git(&path_of(&by_url), &["rev-parse", "HEAD"]), head);
    assert_eq!(by_url["base_ref"], "origin/main");

    let wrong = world
        .create(json!({ "pr": "https://github.com/other/repo/pull/7", "name": "wrong" }))
        .await
        .expect_err("another repository");
    assert!(wrong.contains("wrong_repository"), "{wrong}");
    let missing = world
        .create(json!({ "pr": 99, "name": "missing" }))
        .await
        .expect_err("no such PR");
    assert!(missing.contains("99"), "{missing}");
    assert_eq!(world.workspace_count().await, 2);
}

#[tokio::test]
async fn cancelling_a_create_stops_the_fetch_and_leaves_nothing_behind() {
    let world = World::new(&[], json!({})).await;
    git(&world.repo, &["config", "protocol.ext.allow", "always"]);
    git(
        &world.repo,
        &["remote", "add", "slow", "ext::sh -c sleep% 60"],
    );
    let client = Arc::clone(&world.client);
    let project = world.project.clone();
    let started = Instant::now();
    let create = tokio::spawn(async move {
        client
            .request_timeout(
                "workspace.create",
                json!({
                    "project": project, "name": "slow", "agent": false,
                    "base": "slow/main", "fetch": true, "create_id": "c-1",
                }),
                Duration::from_secs(60),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    // The same connection: a create in flight must not block other requests.
    let cancelled = world
        .request("workspace.create_cancel", json!({ "create_id": "c-1" }))
        .await;
    assert_eq!(cancelled["cancelled"], true);
    let error = create
        .await
        .expect("join")
        .expect_err("cancelled create fails");
    assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(!world.workspaces_dir().join("slow").exists());
    assert_eq!(git(&world.repo, &["branch", "--list", "slow"]), "");
    assert_eq!(world.workspace_count().await, 0);

    let unknown = world
        .request("workspace.create_cancel", json!({ "create_id": "nope" }))
        .await;
    assert_eq!(unknown["cancelled"], false);
    world
        .create(json!({ "name": "fine", "create_id": "c-2" }))
        .await
        .expect("a later create works");
}

#[tokio::test]
async fn committed_scripts_run_only_once_trusted_and_ask_again_when_they_change() {
    let world = World::new(
        &[(
            "orca.yaml",
            "scripts:\n  setup: echo set up > setup-ran.txt\n  archive: echo bye\ndefaultTabs:\n  - title: Dev\n    command: echo tab > tab-ran.txt\n",
        )],
        json!({}),
    )
    .await;
    let scripts = world
        .request("project.scripts", json!({ "id": world.project }))
        .await;
    assert_eq!(scripts["trusted"], false);
    assert_eq!(scripts["setup"]["trusted"], false);
    assert_eq!(scripts["setup"]["command"], "echo set up > setup-ran.txt");
    assert_eq!(scripts["default_tabs"][0]["title"], "Dev");
    assert_eq!(scripts["run_policy"], "run-by-default");

    let refused = world
        .create(json!({ "name": "untrusted" }))
        .await
        .expect_err("untrusted");
    assert!(
        refused.contains("untrusted_scripts") && refused.contains("icarus project trust"),
        "{refused}"
    );
    assert_eq!(world.workspace_count().await, 0, "nothing is created");

    let skipped = world
        .create(json!({ "name": "skipped", "setup": "skip", "layout": "default", "agent": true }))
        .await
        .expect("skipping setup needs no trust");
    let terminal = skipped["layout"]["layout"]["tabs"][1]["terminal_id"]
        .as_str()
        .expect("the default tab joins the layout")
        .to_owned();
    let info = world
        .request("terminal.info", json!({ "id": terminal }))
        .await;
    assert_eq!(info["label"], "Dev");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !path_of(&skipped).join("tab-ran.txt").exists(),
        "an untrusted tab opens without its command"
    );
    let removed = world
        .request("workspace.remove", json!({ "id": skipped["id"] }))
        .await;
    assert!(
        removed["archive"]["skipped"]
            .as_str()
            .is_some_and(|reason| reason.contains("trust")),
        "{removed}"
    );

    let trusted = world
        .create(json!({ "name": "trusted", "trust": true, "layout": "default", "agent": true }))
        .await
        .expect("trust and create");
    let worktree = path_of(&trusted);
    wait_for("setup and the default tab", || {
        worktree.join("setup-ran.txt").exists() && worktree.join("tab-ran.txt").exists()
    })
    .await;
    assert_eq!(
        world
            .request("project.scripts", json!({ "id": world.project }))
            .await["trusted"],
        true
    );
    world
        .create(json!({ "name": "later" }))
        .await
        .expect("trusted content needs no flag");

    commit_file(
        &world.repo,
        "orca.yaml",
        "scripts:\n  setup: echo changed > setup-ran.txt\n",
    );
    let again = world
        .create(json!({ "name": "changed" }))
        .await
        .expect_err("a changed script asks again");
    assert!(again.contains("untrusted_scripts"), "{again}");
    let after = world
        .request("project.trust", json!({ "id": world.project }))
        .await;
    assert_eq!(after["trusted"], true);
    world
        .create(json!({ "name": "changed" }))
        .await
        .expect("trusted again");
    world
        .request("project.untrust", json!({ "id": world.project }))
        .await;
    assert_eq!(
        world
            .request("project.scripts", json!({ "id": world.project }))
            .await["trusted"],
        false
    );
}

#[tokio::test]
async fn a_branch_whose_setup_differs_from_the_trusted_one_does_not_run_it() {
    let world = World::new(
        &[(
            "orca.yaml",
            "scripts:\n  setup: echo main > setup-ran.txt\n",
        )],
        json!({}),
    )
    .await;
    world
        .request("project.trust", json!({ "id": world.project }))
        .await;
    git(&world.repo, &["checkout", "-q", "-b", "sneaky"]);
    commit_file(
        &world.repo,
        "orca.yaml",
        "scripts:\n  setup: echo sneaky > setup-ran.txt\n",
    );
    git(&world.repo, &["checkout", "-q", "main"]);
    let created = world
        .create(json!({ "name": "sneaky", "branch": "sneaky" }))
        .await
        .expect("create");
    assert_eq!(created["setup"]["state"], "failed", "{created}");
    assert!(
        created["setup"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("trust")),
        "{created}"
    );
    assert!(!path_of(&created).join("setup-ran.txt").exists());
    let rerun = world
        .request(
            "workspace.setup",
            json!({ "id": created["id"], "trust": true }),
        )
        .await;
    assert_eq!(rerun["state"], "running");
    let worktree = path_of(&created);
    wait_for("the trusted re-run", || {
        std::fs::read_to_string(worktree.join("setup-ran.txt"))
            .is_ok_and(|text| text.trim() == "sneaky")
    })
    .await;
}

#[tokio::test]
async fn shared_directories_are_linked_and_worktreeinclude_paths_copied() {
    let world = World::new(
        &[
            (
                "orca.yaml",
                "worktree:\n  sharedDirectories:\n    - node_modules\n    - src\n    - ../escape\n",
            ),
            (
                ".worktreeinclude",
                "# private copies\n.env\nconfig/local.json\nmissing.txt\n*.key\n",
            ),
            ("src/main.rs", "fn main() {}\n"),
        ],
        json!({}),
    )
    .await;
    std::fs::write(
        world.repo.join(".gitignore"),
        "icarus.local.json\nnode_modules\n.env\nconfig/local.json\n",
    )
    .expect("ignore");
    git(&world.repo, &["commit", "-q", "-am", "ignore"]);
    std::fs::create_dir_all(world.repo.join("node_modules/left-pad")).expect("modules");
    std::fs::write(world.repo.join("node_modules/left-pad/index.js"), "pad\n").expect("write");
    std::fs::write(world.repo.join(".env"), "SECRET=1\n").expect("env");
    std::fs::create_dir_all(world.repo.join("config")).expect("config");
    std::fs::write(world.repo.join("config/local.json"), "{}\n").expect("local");

    let created = world
        .create(json!({ "name": "shared" }))
        .await
        .expect("create");
    let worktree = path_of(&created);
    let modules = worktree.join("node_modules");
    assert!(
        modules
            .symlink_metadata()
            .expect("modules")
            .file_type()
            .is_symlink(),
        "shared directories link to the root's"
    );
    assert!(modules.join("left-pad/index.js").exists());
    let env = worktree.join(".env");
    assert!(
        !env.symlink_metadata()
            .expect("env")
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_to_string(&env).expect("env"), "SECRET=1\n");
    assert_eq!(
        std::fs::read_to_string(worktree.join("config/local.json")).expect("local"),
        "{}\n"
    );
    std::fs::write(&env, "SECRET=2\n").expect("edit the copy");
    assert_eq!(
        std::fs::read_to_string(world.repo.join(".env")).expect("root env"),
        "SECRET=1\n",
        "a copy, not a link"
    );
    assert!(
        !worktree
            .join("src")
            .symlink_metadata()
            .expect("src")
            .file_type()
            .is_symlink(),
        "tracked directories are never shared"
    );
    assert!(
        created["warnings"].as_array().is_some_and(|w| w
            .iter()
            .any(|w| w.as_str().is_some_and(|w| w.contains("src")))),
        "{created}"
    );

    world
        .request("workspace.remove", json!({ "id": created["id"] }))
        .await;
    assert!(!worktree.exists());
    assert!(
        world.repo.join("node_modules/left-pad/index.js").exists(),
        "removing the workspace keeps the shared directory"
    );
}

#[tokio::test]
async fn always_trusting_a_repository_runs_later_changes_until_untrusted() {
    let world = World::new(
        &[(
            "orca.yaml",
            "scripts:\n  setup: echo one > setup-ran.txt\ndefaultTabs:\n  - title: Dev\n    color: '#22aa88'\n",
        )],
        json!({}),
    )
    .await;
    let first = world
        .create(json!({ "name": "first", "trust": "always", "layout": "default", "agent": true }))
        .await
        .expect("always trust and create");
    let mut labelled = Vec::new();
    for tab in first["layout"]["layout"]["tabs"].as_array().expect("tabs") {
        if let Some(id) = tab["terminal_id"].as_str() {
            let info = world.request("terminal.info", json!({ "id": id })).await;
            labelled.push((info["label"].clone(), info["color"].clone()));
        }
    }
    assert!(
        labelled.contains(&(json!("Dev"), json!("#22aa88"))),
        "the default tab keeps its orca.yaml colour: {labelled:?}"
    );
    let scripts = world
        .request("project.scripts", json!({ "id": world.project }))
        .await;
    assert_eq!(scripts["always_trusted"], true);

    commit_file(
        &world.repo,
        "orca.yaml",
        "scripts:\n  setup: echo two > setup-ran.txt\n",
    );
    let changed = world
        .create(json!({ "name": "changed" }))
        .await
        .expect("a changed script runs without asking");
    let worktree = path_of(&changed);
    wait_for("the changed setup", || {
        std::fs::read_to_string(worktree.join("setup-ran.txt")).is_ok_and(|t| t == "two\n")
    })
    .await;

    world
        .request("project.untrust", json!({ "id": world.project }))
        .await;
    let refused = world
        .create(json!({ "name": "after" }))
        .await
        .expect_err("untrusting revokes always");
    assert!(refused.contains("untrusted_scripts"), "{refused}");
    world
        .request(
            "project.trust",
            json!({ "id": world.project, "always": true }),
        )
        .await;
    commit_file(
        &world.repo,
        "orca.yaml",
        "scripts:\n  setup: echo three > setup-ran.txt\n",
    );
    world
        .create(json!({ "name": "after" }))
        .await
        .expect("project.trust --always");
}

#[tokio::test]
async fn the_issue_command_runs_beside_the_agent_for_a_linked_github_item_once_trusted() {
    let world = World::new(
        &[(
            "orca.yaml",
            "issueCommand: |\n  echo \"{{artifact_url}} #{{issue}}\" > issue-ran.txt\n",
        )],
        json!({}),
    )
    .await;
    let url = "https://github.com/acme/widgets/issues/12";
    let untrusted = world
        .create(json!({ "name": "untrusted", "issue": { "provider": "github", "id": "#12", "url": url } }))
        .await
        .expect("an untrusted issue command doesn't stop the create");
    assert!(
        untrusted["warnings"][0]
            .as_str()
            .is_some_and(|w| w.contains("issue command")),
        "{untrusted}"
    );
    let plain = world
        .create(json!({ "name": "plain", "trust": true }))
        .await
        .expect("no linked item");
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(!path_of(&plain).join("issue-ran.txt").exists());

    let linked = world
        .create(json!({ "name": "linked", "layout": "default", "agent": true, "issue": { "provider": "github", "id": "#12", "url": url } }))
        .await
        .expect("trusted now");
    let worktree = path_of(&linked);
    wait_for("the issue command", || {
        std::fs::read_to_string(worktree.join("issue-ran.txt"))
            .is_ok_and(|t| t == format!("{url} #12\n"))
    })
    .await;
    let tabs = linked["layout"]["layout"]["tabs"]
        .as_array()
        .expect("tabs")
        .clone();
    let terminal = tabs
        .iter()
        .find_map(|tab| tab["terminal_id"].as_str())
        .expect("the issue command's terminal is in the layout");
    let info = world
        .request("terminal.info", json!({ "id": terminal }))
        .await;
    assert_eq!(info["label"], "Issue command");

    let opted_out = world
        .create(json!({ "name": "opted-out", "issue_command": false, "issue": { "provider": "github", "url": url } }))
        .await
        .expect("create");
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(!path_of(&opted_out).join("issue-ran.txt").exists());
}

#[tokio::test]
async fn an_environment_recipe_runs_with_orcas_contract_and_is_destroyed_with_its_instance() {
    let world = World::new(
        &[(
            "orca.yaml",
            "environmentRecipes:\n  - id: sandbox\n    name: Sandbox\n    create: ./create.sh\n    suspend: cat > suspended.json\n    destroy: cat > destroyed.json\n",
        ), (
            "create.sh",
            "#!/bin/sh\necho \"$ORCA_VM_MODE $ORCA_WORKSPACE_NAME\" > created.txt\nprintf '{\"schemaVersion\":1,\"pairingCode\":\"icarus://pair?address=127.0.0.1:1&code=ABCD-EFGH\",\"projectRoot\":\"/srv/widgets\",\"userData\":{\"vm\":\"v1\"}}'\n",
        )],
        json!({}),
    )
    .await;
    std::fs::set_permissions(
        world.repo.join("create.sh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("chmod");
    let listed = world
        .request("recipe.list", json!({ "project": world.project }))
        .await;
    assert_eq!(listed["recipes"][0]["id"], "sandbox");
    assert_eq!(listed["trusted"], false);
    let refused = world
        .client
        .request(
            "recipe.create",
            json!({ "project": world.project, "recipe": "sandbox", "name": "feat" }),
        )
        .await
        .expect_err("recipes need trust");
    assert!(format!("{refused:#}").contains("untrusted_scripts"));

    let created = world
        .request(
            "recipe.create",
            json!({ "project": world.project, "recipe": "sandbox", "name": "feat", "trust": true }),
        )
        .await;
    let environment = &created["environment"];
    assert_eq!(environment["connection"]["type"], "icarus-server");
    assert_eq!(environment["connection"]["projectRoot"], "/srv/widgets");
    assert_eq!(
        std::fs::read_to_string(world.repo.join("created.txt")).expect("ran"),
        "create feat\n"
    );
    let instance = environment["context"]["instance_id"]
        .as_str()
        .expect("instance")
        .to_owned();
    world
        .request(
            "recipe.bind",
            json!({ "instance": instance, "server": "env-feat" }),
        )
        .await;
    let suspended = world
        .request("recipe.suspend", json!({ "instance": instance }))
        .await;
    assert_eq!(suspended["environment"]["suspended"], true);
    let payload: Value = serde_json::from_str(
        &std::fs::read_to_string(world.repo.join("suspended.json")).expect("payload"),
    )
    .expect("json");
    assert_eq!(payload["mode"], "suspend");
    assert_eq!(payload["instanceId"], instance.as_str());
    assert_eq!(payload["recipeResult"]["userData"]["vm"], "v1");
    let resumed = world
        .request("recipe.resume", json!({ "instance": instance }))
        .await;
    assert_eq!(resumed["skipped"], true, "no resume command");
    let envs = world.request("recipe.environments", json!({})).await;
    assert_eq!(envs[0]["server"], "env-feat");
    world
        .request("recipe.destroy", json!({ "instance": instance }))
        .await;
    let payload: Value = serde_json::from_str(
        &std::fs::read_to_string(world.repo.join("destroyed.json")).expect("payload"),
    )
    .expect("json");
    assert_eq!(payload["mode"], "destroy");
    assert_eq!(
        world.request("recipe.environments", json!({})).await,
        json!([])
    );
}
