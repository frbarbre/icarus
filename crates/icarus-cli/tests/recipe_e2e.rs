//! `icarus workspace create --recipe`: the local server runs the recipe, the CLI pairs with the
//! server it reports and creates the workspace there, and `icarus recipe destroy` tears it down.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn sibling(name: &str) -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_icarus")).with_file_name(name)
}

struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

fn repo(dir: &Path, files: &[(&str, &str)]) {
    std::fs::create_dir_all(dir).expect("repo dir");
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "me@example.com"]);
    git(dir, &["config", "user.name", "Me"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("readme.md"), "hi\n").expect("write");
    for (name, text) in files {
        std::fs::write(dir.join(name), text).expect("write");
    }
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

struct Machine {
    home: PathBuf,
    config: PathBuf,
}

impl Machine {
    fn new(root: &Path, name: &str) -> Self {
        let home = root.join(name).join("home");
        let config = root.join(name).join("config");
        std::fs::create_dir_all(&config).expect("config");
        Self { home, config }
    }

    fn serve(&self, listen: bool, env: &[(&str, &Path)]) -> (Guard, Option<String>) {
        let mut command = Command::new(sibling("icarus-server"));
        command
            .args(["--home", self.home.to_str().expect("utf8"), "serve"])
            .env("ICARUS_WORKSPACE_ROOT", self.home.join("workspaces"))
            .env("ICARUS_CONFIG_DIR", &self.config)
            .env("SHELL", "/bin/sh")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if listen {
            command.args(["--listen", "127.0.0.1:0"]);
        }
        for (key, value) in env {
            command.env(key, value);
        }
        let guard = Guard(command.spawn().expect("server"));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let ready =
                self.home.join("server.token").exists() && self.home.join("server.sock").exists();
            let addr = std::fs::read_to_string(self.home.join("server.addr"))
                .ok()
                .and_then(|text| text.lines().next().map(str::to_owned));
            if ready && (!listen || addr.is_some()) {
                return (guard, addr);
            }
            assert!(Instant::now() < deadline, "server did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn icarus(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_icarus"))
            .args(["--home", self.home.to_str().expect("utf8"), "--json"])
            .args(args)
            .env("ICARUS_CONFIG_DIR", &self.config)
            .output()
            .expect("run icarus")
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.icarus(args);
        assert!(
            output.status.success(),
            "icarus {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("json output")
    }
}

#[test]
fn a_recipe_environment_is_paired_used_and_destroyed() {
    let root = tempfile::tempdir().expect("root");
    let vm = Machine::new(root.path(), "vm");
    let laptop = Machine::new(root.path(), "laptop");
    let (_vm_server, addr) = vm.serve(true, &[]);
    let addr = addr.expect("listen address");
    let vm_repo = root.path().join("vm-repo");
    repo(&vm_repo, &[]);

    let result = root.path().join("result.json");
    let local_repo = root.path().join("widgets");
    repo(
        &local_repo,
        &[(
            "orca.yaml",
            "environmentRecipes:\n  - id: sandbox\n    name: Sandbox\n    create: cat \"$RECIPE_RESULT\"\n    destroy: cat > destroyed.json\n",
        )],
    );
    let (_laptop_server, _) = laptop.serve(false, &[("RECIPE_RESULT", &result)]);
    let project = laptop.json(&["project", "add", local_repo.to_str().expect("utf8")]);
    let project = project["id"].as_str().expect("project").to_owned();

    let code = vm.json(&["server", "pair"])["code"]
        .as_str()
        .expect("code")
        .to_owned();
    std::fs::write(
        &result,
        json!({
            "schemaVersion": 1,
            "pairingCode": format!("icarus://pair?address={addr}&code={code}"),
            "projectRoot": vm_repo,
        })
        .to_string(),
    )
    .expect("result");

    let refused = laptop.icarus(&[
        "workspace",
        "create",
        "--project",
        &project,
        "--recipe",
        "sandbox",
        "--no-agent",
        "--base",
        "main",
        "feat",
    ]);
    assert!(!refused.status.success(), "recipes need trust");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("trust"));

    let created = laptop.json(&[
        "workspace",
        "create",
        "--project",
        &project,
        "--recipe",
        "sandbox",
        "--no-agent",
        "--base",
        "main",
        "--trust",
        "feat",
    ]);
    assert_eq!(created["server"], "env-feat");
    assert_eq!(created["workspace"]["name"], "feat");
    let on_vm = vm.json(&["workspace", "list"]);
    assert_eq!(
        on_vm[0]["name"], "feat",
        "the workspace lives on the environment's server"
    );
    let through_saved = laptop.json(&["--server", "env-feat", "workspace", "list"]);
    assert_eq!(through_saved[0]["name"], "feat");

    let instance = created["environment"]
        .as_str()
        .expect("instance")
        .to_owned();
    let envs = laptop.json(&["recipe", "envs"]);
    assert_eq!(envs[0]["server"], "env-feat");
    laptop.json(&["recipe", "destroy", &instance]);
    let payload: Value = serde_json::from_str(
        &std::fs::read_to_string(local_repo.join("destroyed.json")).expect("destroy ran"),
    )
    .expect("payload");
    assert_eq!(payload["instanceId"], instance.as_str());
    assert_eq!(laptop.json(&["recipe", "envs"]), json!([]));
    assert_eq!(
        laptop.json(&["remote", "list"]),
        json!([]),
        "the saved server is forgotten"
    );
}
