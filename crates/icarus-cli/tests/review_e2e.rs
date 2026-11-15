//! `icarus review …` against a server with fixture GitHub, run from inside the workspace like an agent would.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

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

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

struct Env {
    home: tempfile::TempDir,
    fixtures: PathBuf,
    _server: Guard,
}

impl Env {
    fn start() -> Self {
        let home = tempfile::tempdir().expect("home");
        let config = home.path().join("config");
        std::fs::create_dir_all(&config).expect("config");
        std::fs::write(config.join("icarus.json"), "{}").expect("settings");
        let fixtures = home.path().join("gh.json");
        std::fs::write(&fixtures, json!({ "viewer": "me" }).to_string()).expect("fixtures");
        let child = Command::new(sibling("icarus-server"))
            .args(["--home", home.path().to_str().expect("utf8"), "serve"])
            .env("ICARUS_WORKSPACE_ROOT", home.path().join("workspaces"))
            .env("ICARUS_CONFIG_DIR", &config)
            .env("ICARUS_GH_FIXTURES", &fixtures)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("server");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !home.path().join("server.token").exists() {
            assert!(Instant::now() < deadline, "server did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        Self {
            home,
            fixtures,
            _server: Guard(child),
        }
    }

    fn icarus_in(&self, dir: &Path, args: &[&str], stdin: Option<&str>) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_icarus"))
            .current_dir(dir)
            .arg("--home")
            .arg(self.home.path())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("icarus");
        let mut pipe = child.stdin.take().expect("stdin");
        pipe.write_all(stdin.unwrap_or_default().as_bytes())
            .expect("write stdin");
        drop(pipe);
        child.wait_with_output().expect("icarus")
    }

    fn ok_in(&self, dir: &Path, args: &[&str], stdin: Option<&str>) -> String {
        let output = self.icarus_in(dir, args, stdin);
        assert!(
            output.status.success(),
            "icarus {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn json_in(&self, dir: &Path, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&self.ok_in(dir, &all, None)).expect("json output")
    }

    fn requests(&self, route: &str) -> Vec<Value> {
        let log = PathBuf::from(format!("{}.requests.jsonl", self.fixtures.display()));
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|r| r["route"] == route)
            .collect()
    }
}

#[tokio::test]
async fn an_agent_reviews_from_inside_the_workspace() {
    let env = Env::start();
    let repo = tempfile::tempdir().expect("repo");
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["config", "user.email", "t@example.com"],
        &["config", "user.name", "T"],
        &["config", "commit.gpgsign", "false"],
        &["commit", "-q", "--allow-empty", "-m", "init"],
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widgets.git",
        ],
    ] {
        git(repo.path(), args);
    }
    let project = env.json_in(
        repo.path(),
        &["project", "add", repo.path().to_str().expect("utf8")],
    );
    let id = project["id"].as_str().expect("project id");
    let ws = env.json_in(
        repo.path(),
        &["workspace", "create", "--project", id, "--no-agent", "rev"],
    );
    let path = PathBuf::from(ws["path"].as_str().expect("path"));
    std::fs::create_dir_all(path.join("src")).expect("src");
    let text: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    std::fs::write(path.join("src/a.rs"), text).expect("a.rs");
    git(&path, &["add", "."]);
    git(&path, &["commit", "-q", "-m", "a"]);
    let head = git(&path, &["rev-parse", "HEAD"]);
    let patch: String = std::iter::once("@@ -0,0 +1,10 @@".to_owned())
        .chain((1..=10).map(|i| format!("+line {i}")))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        &env.fixtures,
        json!({
            "viewer": "me",
            "pulls": [{ "id": "PR_5", "number": 5, "title": "Add a", "url": "https://github.com/acme/widgets/pull/5",
                "state": "OPEN", "isDraft": false, "headRefName": "rev", "baseRefName": "main", "headRefOid": head,
                "author": { "login": "alice" }, "repository": { "name": "widgets", "owner": { "login": "acme" } },
                "commits": { "nodes": [] } }],
            "pr_files": { "5": [ { "filename": "src/a.rs", "patch": patch } ] },
            "review_threads": { "5": [ { "id": "PRRT_1", "isResolved": false, "path": "src/a.rs", "line": 2,
                "originalLine": 2, "diffSide": "RIGHT", "subjectType": "LINE",
                "comments": { "nodes": [ { "id": "PRRC_1", "body": "Hm?", "author": { "login": "alice" },
                  "reactionGroups": [ { "content": "ROCKET", "reactors": { "totalCount": 2, "nodes": [] } } ] } ] } } ] },
        })
        .to_string(),
    )
    .expect("fixtures");
    let src = path.join("src");

    let added = env.ok_in(
        &src,
        &[
            "review",
            "comment",
            "add",
            "--file",
            "src/a.rs",
            "--line",
            "2",
            "--end-line",
            "3",
            "--body",
            "Range",
        ],
        None,
    );
    assert!(added.contains("src/a.rs:2-3"), "{added}");
    let outside = env.ok_in(
        &src,
        &[
            "review",
            "comment",
            "add",
            "--file",
            "src/a.rs",
            "--line",
            "15",
            "--body-file",
            "-",
        ],
        Some("From stdin\n"),
    );
    assert!(outside.contains("file comment"), "{outside}");

    std::fs::write(path.join("shot.png"), b"\x89PNG\r\n\x1a\n").expect("png");
    let markdown = env.ok_in(&path, &["review", "upload", "shot.png"], None);
    assert_eq!(
        markdown.trim(),
        "![shot.png](https://github.com/user-attachments/assets/fixture-shot.png)"
    );

    env.ok_in(
        &src,
        &[
            "review",
            "summary",
            "--body",
            "Nice work",
            "--verdict",
            "comment",
        ],
        None,
    );
    let shown = env.ok_in(&src, &["review", "show"], None);
    assert!(shown.contains("acme/widgets#5 Add a"), "{shown}");
    assert!(shown.contains("standing: pending"), "{shown}");
    assert!(shown.contains("summary: Nice work"), "{shown}");
    let listed = env.json_in(&src, &["review", "comment", "list"]);
    assert_eq!(listed["drafts"].as_array().expect("drafts").len(), 2);

    let base = env.ok_in(&src, &["review", "base"], None);
    assert!(base.contains("main · target branch"), "{base}");

    let submitted = env.ok_in(&src, &["review", "submit"], None);
    assert!(
        submitted.contains("Review submitted (commented)"),
        "{submitted}"
    );
    let input = &env.requests("graphql SubmitReview")[0]["variables"]["input"];
    assert_eq!(input["event"], "COMMENT");
    assert_eq!(input["body"], "Nice work");
    assert_eq!(input["threads"].as_array().expect("threads").len(), 2);
    assert_eq!(
        input["threads"][1]["body"],
        "Line 15:\n```\nline 15\n```\n\nFrom stdin\n"
    );

    let threads = env.ok_in(&src, &["review", "threads"], None);
    assert!(
        threads.contains("PRRT_1  src/a.rs Line 2  [open]"),
        "{threads}"
    );
    assert!(threads.contains("rocket 2"), "{threads}");
    let replied = env.ok_in(&src, &["review", "reply", "PRRT_1", "--body", "Done"], None);
    assert!(replied.contains("replied in PRRT_1"), "{replied}");
    let resolved = env.ok_in(&src, &["review", "resolve", "PRRT_1"], None);
    assert_eq!(resolved.trim(), "PRRT_1 is resolved");
    let reacted = env.ok_in(&src, &["review", "react", "PRRC_1", "eyes"], None);
    assert_eq!(reacted.trim(), "added eyes on PRRC_1");

    let empty = env.icarus_in(&src, &["review", "submit", "comment"], None);
    assert!(!empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stderr).contains("Nothing to submit"));
    let approved = env.ok_in(&src, &["review", "submit", "approve"], None);
    assert!(
        approved.contains("Review submitted (approved)"),
        "approve needs no drafts: {approved}"
    );
}
