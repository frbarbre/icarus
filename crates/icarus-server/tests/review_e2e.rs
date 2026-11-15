//! Review mode end to end against fixture GitHub: drafts, persistence across restarts, submit, threads, media, base.

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

struct World {
    home: TempDir,
    config: TempDir,
    _repos: TempDir,
    repo: PathBuf,
    fixtures: PathBuf,
    server: Option<ServerProcess>,
    client: Arc<Client>,
    project: String,
}

fn spawn(home: &Path, config: &Path, fixtures: &Path) -> ServerProcess {
    let _ = std::fs::remove_file(home.join("server.token"));
    let _ = std::fs::remove_file(home.join("server.sock"));
    let child = Command::new(env!("CARGO_BIN_EXE_icarus-server"))
        .args(["--home", home.to_str().expect("utf8"), "serve"])
        .env("ICARUS_WORKSPACE_ROOT", home.join("workspaces"))
        .env("XDG_CONFIG_HOME", config)
        .env("ICARUS_GH_FIXTURES", fixtures)
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

impl World {
    async fn new(fixtures: Value) -> Self {
        let home = tempfile::tempdir().expect("home");
        let config = tempfile::tempdir().expect("config");
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
        std::fs::write(repo.join("readme.md"), "hello\n").expect("write");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        );
        let settings_path = config.path().join("icarus").join("settings.json");
        std::fs::create_dir_all(settings_path.parent().expect("parent")).expect("settings dir");
        std::fs::write(&settings_path, "{}").expect("settings");
        let fixtures_path = home.path().join("gh-fixtures.json");
        std::fs::write(&fixtures_path, fixtures.to_string()).expect("fixtures");
        let server = spawn(home.path(), config.path(), &fixtures_path);
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
        git(&repo, &["fetch", "-q", "origin"]);
        Self {
            project: project["id"].as_str().expect("id").to_owned(),
            home,
            config,
            _repos: repos,
            repo,
            fixtures: fixtures_path,
            server: Some(server),
            client,
        }
    }

    async fn restart(&mut self) {
        drop(self.server.take());
        self.server = Some(spawn(self.home.path(), self.config.path(), &self.fixtures));
        let (client, _events) = Client::connect(self.home.path(), "test")
            .await
            .expect("reconnect");
        self.client = client;
    }

    fn set_fixtures(&self, fixtures: &Value) {
        std::fs::write(&self.fixtures, fixtures.to_string()).expect("fixtures");
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

    async fn request(&self, method: &str, params: Value) -> Value {
        self.client
            .request(method, params)
            .await
            .unwrap_or_else(|e| panic!("{method}: {e:#}"))
    }

    async fn error(&self, method: &str, params: Value) -> String {
        format!(
            "{:#}",
            self.client
                .request(method, params)
                .await
                .expect_err(&format!("{method} should fail"))
        )
    }

    /// A workspace on branch `name` with `src/a.rs` (20 lines) committed; returns (id, path, head).
    async fn workspace(&self, name: &str) -> (String, PathBuf, String) {
        let ws = self
            .request(
                "workspace.create",
                json!({ "project": self.project, "name": name, "agent": false }),
            )
            .await;
        let path = PathBuf::from(ws["path"].as_str().expect("path"));
        std::fs::create_dir_all(path.join("src")).expect("src");
        let text: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        std::fs::write(path.join("src/a.rs"), text).expect("write");
        git(&path, &["add", "."]);
        git(&path, &["commit", "-q", "-m", "add a.rs"]);
        let head = git(&path, &["rev-parse", "HEAD"]);
        (ws["id"].as_str().expect("id").to_owned(), path, head)
    }
}

fn pull(number: u64, branch: &str, head: &str, author: &str) -> Value {
    json!({
        "id": format!("PR_{number}"),
        "number": number,
        "title": format!("Pull request {number}"),
        "url": format!("https://github.com/acme/widgets/pull/{number}"),
        "state": "OPEN",
        "isDraft": false,
        "updatedAt": "2026-11-15T12:00:00Z",
        "headRefName": branch,
        "baseRefName": "main",
        "headRefOid": head,
        "headRef": { "name": branch },
        "baseRef": { "name": "main" },
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "CLEAN",
        "author": { "login": author },
        "repository": { "name": "widgets", "owner": { "login": "acme" } },
        "commits": { "nodes": [] },
    })
}

/// Lines 1–10 of src/a.rs are in the PR's diff; 11–20 are not.
fn pr_files() -> Value {
    let patch: String = std::iter::once("@@ -0,0 +1,10 @@".to_owned())
        .chain((1..=10).map(|i| format!("+line {i}")))
        .collect::<Vec<_>>()
        .join("\n");
    json!({ "5": [ { "filename": "src/a.rs", "patch": patch } ] })
}

fn fixtures(head: &str, author: &str) -> Value {
    json!({
        "viewer": "me",
        "pulls": [pull(5, "rev", head, author)],
        "pr_files": pr_files(),
    })
}

#[tokio::test]
async fn drafts_survive_a_restart_and_submit_as_one_review() {
    let mut world = World::new(json!({ "viewer": "me" })).await;
    let (ws, _path, head) = world.workspace("rev").await;
    world.set_fixtures(&fixtures(&head, "alice"));
    let at = |extra: Value| {
        let mut params = json!({ "workspace": ws });
        for (k, v) in extra.as_object().into_iter().flatten() {
            params[k] = v.clone();
        }
        params
    };

    let single = world
        .request(
            "review.comment.add",
            at(json!({ "path": "src/a.rs", "line": 3, "body": "Why **three**?" })),
        )
        .await;
    assert_eq!(single["draft"]["subject"], "line");
    assert_eq!(single["draft"]["side"], "RIGHT");
    assert_eq!(single["file_comment"], false);
    let range = world
        .request(
            "review.comment.add",
            at(json!({ "path": "src/a.rs", "start_line": 2, "line": 5, "body": "This block\n![shot](https://github.com/user-attachments/assets/x)" })),
        )
        .await;
    assert_eq!(range["draft"]["start_line"], 2);
    let outside = world
        .request(
            "review.comment.add",
            at(json!({ "path": "src/a.rs", "start_line": 14, "line": 15, "body": "Unrelated?" })),
        )
        .await;
    assert_eq!(outside["file_comment"], true);
    assert_eq!(outside["draft"]["subject"], "file");
    assert_eq!(outside["draft"]["line"], Value::Null);
    assert_eq!(
        outside["draft"]["body"],
        "Lines 14\u{2013}15:\n```\nline 14\nline 15\n```\n\nUnrelated?"
    );
    let same = world
        .request(
            "review.comment.add",
            at(json!({ "path": "src/a.rs", "start_line": 7, "line": 7, "body": "dropme" })),
        )
        .await;
    assert_eq!(
        same["draft"]["start_line"],
        Value::Null,
        "an equal start is dropped"
    );
    let not_in_pr = world
        .error(
            "review.comment.add",
            at(json!({ "path": "other.rs", "line": 1, "body": "x" })),
        )
        .await;
    assert!(
        not_in_pr.contains("not changed in pull request #5"),
        "{not_in_pr}"
    );
    let empty = world
        .error(
            "review.comment.add",
            at(json!({ "path": "src/a.rs", "line": 1, "body": "  " })),
        )
        .await;
    assert!(empty.contains("cannot be empty"), "{empty}");
    let backwards = world
        .error(
            "review.comment.add",
            at(json!({ "path": "src/a.rs", "start_line": 5, "line": 2, "body": "x" })),
        )
        .await;
    assert!(backwards.contains("start_line"), "{backwards}");

    let dropped = same["draft"]["id"].as_str().expect("id");
    world
        .request("review.comment.remove", at(json!({ "id": dropped })))
        .await;
    let first = single["draft"]["id"].as_str().expect("id");
    world
        .request(
            "review.comment.edit",
            at(json!({ "id": first, "body": "Why **three**? :eyes:" })),
        )
        .await;
    world
        .request(
            "review.set_summary",
            at(json!({ "body": "Looks close.", "verdict": "comment" })),
        )
        .await;

    let before = world.request("review.get", at(json!({}))).await;
    assert_eq!(before["drafts"].as_array().expect("drafts").len(), 3);
    assert_eq!(before["standing"], "pending");
    assert_eq!(before["viewer_did_author"], false);
    assert_eq!(
        before["verdicts"],
        json!(["comment", "request-changes", "approve"])
    );
    assert_eq!(before["can_submit"]["comment"], true);
    assert_eq!(before["pr"]["id"], "PR_5");

    world.restart().await;
    let after = world.request("review.get", at(json!({}))).await;
    assert_eq!(
        after["drafts"], before["drafts"],
        "drafts survive a restart"
    );
    assert_eq!(after["summary"], "Looks close.");
    assert_eq!(after["verdict"], "comment");
    assert_eq!(after["drafts"][0]["body"], "Why **three**? :eyes:");
    assert_eq!(
        after["drafts"][2]["subject"], "file",
        "file drafts keep their subject"
    );

    let removal = world.error("workspace.remove", json!({ "id": ws })).await;
    assert!(removal.contains("unsent review"), "{removal}");

    let submitted = world.request("review.submit", at(json!({}))).await;
    assert_eq!(submitted["state"], "COMMENTED");
    assert_eq!(submitted["comments"], 3);
    let sent = world.requests("graphql SubmitReview");
    assert_eq!(sent.len(), 1);
    let input = &sent[0]["variables"]["input"];
    assert_eq!(input["pullRequestId"], "PR_5");
    assert_eq!(input["event"], "COMMENT");
    assert_eq!(input["body"], "Looks close.");
    assert_eq!(
        input["threads"],
        json!([
            { "path": "src/a.rs", "line": 3, "side": "RIGHT", "body": "Why **three**? :eyes:" },
            { "path": "src/a.rs", "line": 5, "side": "RIGHT", "startLine": 2, "startSide": "RIGHT",
              "body": "This block\n![shot](https://github.com/user-attachments/assets/x)" },
            { "path": "src/a.rs", "subjectType": "FILE",
              "body": "Lines 14\u{2013}15:\n```\nline 14\nline 15\n```\n\nUnrelated?" },
        ])
    );
    let cleared = world.request("review.get", at(json!({}))).await;
    assert_eq!(cleared["drafts"], json!([]));
    assert_eq!(cleared["summary"], "");
    assert_eq!(cleared["verdict"], Value::Null);
    let nothing = world
        .error("review.submit", at(json!({ "verdict": "comment" })))
        .await;
    assert!(nothing.contains("Nothing to submit"), "{nothing}");
}

#[tokio::test]
async fn own_pull_request_is_comment_only_and_a_refused_submit_keeps_drafts() {
    let world = World::new(json!({ "viewer": "me" })).await;
    let (ws, _path, head) = world.workspace("rev").await;
    let mut fx = fixtures(&head, "me");
    world.set_fixtures(&fx);
    world
        .request(
            "review.comment.add",
            json!({ "workspace": ws, "path": "src/a.rs", "line": 1, "body": "note" }),
        )
        .await;
    let view = world
        .request("review.get", json!({ "workspace": ws }))
        .await;
    assert_eq!(view["viewer_did_author"], true);
    assert_eq!(view["verdicts"], json!(["comment"]));
    let approve = world
        .error(
            "review.submit",
            json!({ "workspace": ws, "verdict": "approve" }),
        )
        .await;
    assert!(approve.contains("does not let the author"), "{approve}");
    let unknown = world
        .error(
            "review.submit",
            json!({ "workspace": ws, "verdict": "lgtm" }),
        )
        .await;
    assert!(unknown.contains("verdict must be"), "{unknown}");

    fx["routes"] = json!({ "graphql SubmitReview": { "body": {
        "data": { "addPullRequestReview": null },
        "errors": [ { "message": "Pull request review thread line must be part of the diff" } ] } } });
    world.set_fixtures(&fx);
    let refused = world
        .error(
            "review.submit",
            json!({ "workspace": ws, "verdict": "comment", "body": "hm" }),
        )
        .await;
    assert!(
        refused.contains("Pull request review thread line must be part of the diff"),
        "GitHub's message comes back verbatim: {refused}"
    );
    let kept = world
        .request("review.get", json!({ "workspace": ws }))
        .await;
    assert_eq!(kept["drafts"].as_array().expect("drafts").len(), 1);

    world
        .request("review.comment.clear", json!({ "workspace": ws }))
        .await;
    world
        .request("workspace.remove", json!({ "id": ws, "force": true }))
        .await;
}

#[tokio::test]
async fn threads_replies_resolution_reactions_and_media() {
    let world = World::new(json!({ "viewer": "me" })).await;
    let (ws, path, head) = world.workspace("rev").await;
    let mut fx = fixtures(&head, "alice");
    fx["review_threads"] = json!({ "5": [
        { "id": "PRRT_1", "isResolved": false, "isOutdated": false, "path": "src/a.rs", "line": 4, "startLine": 2,
          "originalLine": 4, "diffSide": "RIGHT", "subjectType": "LINE", "viewerCanResolve": true, "viewerCanReply": true,
          "comments": { "nodes": [
            { "id": "PRRC_1", "databaseId": 11, "body": "Off by one.", "url": "u1", "viewerCanUpdate": false,
              "author": { "__typename": "User", "login": "alice", "avatarUrl": "https://a/alice" },
              "reactionGroups": [ { "content": "THUMBS_UP", "viewerHasReacted": true,
                "reactors": { "totalCount": 1, "nodes": [ { "login": "me" } ] } } ] } ] } },
        { "id": "PRRT_2", "isResolved": true, "isOutdated": true, "path": "src/a.rs", "line": null,
          "originalLine": 9, "diffSide": "RIGHT", "subjectType": "LINE",
          "comments": { "nodes": [ { "id": "PRRC_2", "body": "old", "author": { "__typename": "Bot", "login": "lint[bot]" } } ] } },
        { "id": "PRRT_3", "isResolved": false, "isOutdated": false, "path": "src/a.rs", "line": 40,
          "originalLine": 40, "diffSide": "RIGHT", "subjectType": "LINE", "comments": { "nodes": [] } },
        { "id": "PRRT_4", "isResolved": false, "isOutdated": false, "path": "readme.md", "line": null,
          "diffSide": "RIGHT", "subjectType": "FILE", "comments": { "nodes": [] } }
    ] });
    world.set_fixtures(&fx);

    let all = world
        .request("review.threads", json!({ "workspace": ws }))
        .await;
    let threads = all["threads"].as_array().expect("threads");
    assert_eq!(threads.len(), 4);
    assert_eq!(
        threads[0]["placement"],
        json!({ "file": false, "outdated": false, "line": 4, "start_line": 2, "label": "Lines 2-4" })
    );
    assert_eq!(threads[0]["comments"][0]["reactions"][0]["content"], "+1");
    assert_eq!(threads[0]["comments"][0]["database_id"], 11);
    assert_eq!(threads[1]["placement"]["label"], "Outdated · was on line 9");
    assert_eq!(threads[1]["comments"][0]["is_bot"], true);
    assert_eq!(
        threads[2]["placement"]["outdated"], true,
        "past the end of the local file"
    );
    assert_eq!(threads[3]["placement"]["file"], true);
    let open = world
        .request(
            "review.threads",
            json!({ "workspace": ws, "unresolved": true, "path": "src/a.rs" }),
        )
        .await;
    let ids: Vec<&str> = open["threads"]
        .as_array()
        .expect("threads")
        .iter()
        .filter_map(|t| t["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["PRRT_1", "PRRT_3"]);

    let reply = world
        .request(
            "review.reply",
            json!({ "workspace": ws, "thread": "PRRT_1", "body": "Fixed in the next push." }),
        )
        .await;
    assert_eq!(reply["comment"]["id"], "PRRC_reply");
    assert_eq!(
        world.requests("graphql ReplyInThread")[0]["variables"],
        json!({ "threadId": "PRRT_1", "body": "Fixed in the next push." })
    );
    let resolved = world
        .request("review.resolve", json!({ "thread": "PRRT_1" }))
        .await;
    assert_eq!(resolved["resolved"], true);
    let reopened = world
        .request("review.unresolve", json!({ "thread": "PRRT_1" }))
        .await;
    assert_eq!(reopened["resolved"], false);
    assert_eq!(
        world.requests("graphql ResolveThread")[0]["variables"],
        json!({ "threadId": "PRRT_1" })
    );
    world
        .request(
            "review.react",
            json!({ "subject": "PRRC_1", "reaction": "heart" }),
        )
        .await;
    world
        .request(
            "review.react",
            json!({ "subject": "PRRC_1", "reaction": "+1", "remove": true }),
        )
        .await;
    assert_eq!(
        world.requests("graphql AddReaction")[0]["variables"],
        json!({ "subjectId": "PRRC_1", "content": "HEART" })
    );
    assert_eq!(
        world.requests("graphql RemoveReaction")[0]["variables"],
        json!({ "subjectId": "PRRC_1", "content": "THUMBS_UP" })
    );
    let bad = world
        .error(
            "review.react",
            json!({ "subject": "PRRC_1", "reaction": "tada" }),
        )
        .await;
    assert!(bad.contains("reaction must be"), "{bad}");

    std::fs::write(path.join("shot.png"), b"\x89PNG\r\n\x1a\nfake").expect("png");
    let image = world
        .request(
            "review.upload",
            json!({ "workspace": ws, "path": "shot.png" }),
        )
        .await;
    assert_eq!(image["kind"], "image");
    assert_eq!(
        image["markdown"],
        "![shot.png](https://github.com/user-attachments/assets/fixture-shot.png)"
    );
    let video = world
        .request(
            "review.upload",
            json!({ "workspace": ws, "name": "clip.mp4", "data_base64": "AAAAGGZ0eXBtcDQy" }),
        )
        .await;
    assert_eq!(video["kind"], "video");
    assert_eq!(
        video["markdown"],
        "\nhttps://github.com/user-attachments/assets/fixture-clip.mp4\n"
    );
    let uploads = world.requests("GET repos/acme/widgets");
    assert_eq!(
        uploads.len(),
        2,
        "write access is checked before each upload"
    );
    std::fs::write(path.join("notes.txt"), "x").expect("txt");
    let refused = world
        .error(
            "review.upload",
            json!({ "workspace": ws, "path": "notes.txt" }),
        )
        .await;
    assert!(refused.contains("Only images and videos"), "{refused}");
    fx["repo_push"] = json!(false);
    world.set_fixtures(&fx);
    let read_only = world
        .error(
            "review.upload",
            json!({ "workspace": ws, "path": "shot.png" }),
        )
        .await;
    assert!(
        read_only.contains("needs write access to acme/widgets"),
        "{read_only}"
    );

    let with_media = world
        .request(
            "review.comment.add",
            json!({ "workspace": ws, "path": "src/a.rs", "line": 2,
                "body": format!("See {}{}", image["markdown"].as_str().unwrap_or_default(), video["markdown"].as_str().unwrap_or_default()) }),
        )
        .await;
    assert!(
        with_media["draft"]["body"]
            .as_str()
            .unwrap_or_default()
            .contains("fixture-clip.mp4")
    );
    let blob = world
        .error(
            "review.comment.add",
            json!({ "workspace": ws, "path": "src/a.rs", "line": 2, "body": "![x](blob:http://a/b)" }),
        )
        .await;
    assert!(blob.contains("upload in progress"), "{blob}");
}

fn rows(listing: &Value) -> Vec<String> {
    let mut paths: Vec<String> = listing["rows"]
        .as_array()
        .or_else(|| listing["files"].as_array())
        .expect("rows")
        .iter()
        .filter_map(|r| r["path"].as_str().map(str::to_owned))
        .collect();
    paths.sort();
    paths
}

#[tokio::test]
async fn the_base_picker_follows_reviews_rebases_and_commit_picks() {
    let world = World::new(json!({ "viewer": "me" })).await;
    let (ws, path, first) = world.workspace("rev").await;
    std::fs::write(path.join("b.rs"), "b\n").expect("b");
    git(&path, &["add", "."]);
    git(&path, &["commit", "-q", "-m", "after review"]);
    let second = git(&path, &["rev-parse", "HEAD"]);
    let mut fx = fixtures(&second, "alice");
    fx["review_context"] = json!({ "5": {
        "viewerLatestReview": { "state": "COMMENTED", "commit": { "oid": first } },
        "commits": { "nodes": [
            { "commit": { "oid": first, "messageHeadline": "add a.rs" } },
            { "commit": { "oid": second, "messageHeadline": "after review" } } ] },
        "reviews": { "nodes": [ { "state": "COMMENTED", "body": "first pass", "submittedAt": "2026-11-14T00:00:00Z",
            "author": { "login": "me" }, "commit": { "oid": first }, "comments": { "nodes": [] } } ] } } });
    world.set_fixtures(&fx);

    let view = world
        .request("review.get", json!({ "workspace": ws }))
        .await;
    let base = &view["base"];
    assert_eq!(base["default"], "since-review");
    assert_eq!(
        base["options"][0]["label"],
        "Since your last review (Review 1)"
    );
    assert_eq!(base["options"][1]["value"], format!("commit:{second}"));
    assert_eq!(base["options"][2]["tags"], json!(["Review 1"]));
    assert_eq!(base["options"][3]["value"], "whole");

    let applied = world
        .request(
            "review.set_base",
            json!({ "workspace": ws, "base": "since-review" }),
        )
        .await;
    assert_eq!(applied["kind"], "reviewed");
    assert_eq!(applied["base_ref"], first);
    let info = world.request("workspace.info", json!({ "id": ws })).await;
    assert_eq!(info["base_ref"], first);
    let changes = world
        .request("workspace.changes", json!({ "id": ws }))
        .await;
    assert_eq!(rows(&changes), vec!["b.rs"]);

    let picked = world
        .request(
            "review.set_base",
            json!({ "workspace": ws, "base": format!("commit:{first}") }),
        )
        .await;
    assert_eq!(picked["kind"], "reviewed");
    let changes = world
        .request("workspace.changes", json!({ "id": ws }))
        .await;
    assert_eq!(
        rows(&changes),
        vec!["b.rs", "src/a.rs"],
        "a picked commit's own changes are included"
    );

    world
        .request(
            "review.set_base",
            json!({ "workspace": ws, "base": "since-review" }),
        )
        .await;
    std::fs::write(world.repo.join("upstream.md"), "u\n").expect("upstream");
    git(&world.repo, &["add", "."]);
    git(&world.repo, &["commit", "-q", "-m", "upstream"]);
    git(&world.repo, &["push", "-q", "origin", "main"]);
    git(&path, &["fetch", "-q", "origin"]);
    git(&path, &["rebase", "-q", "origin/main"]);
    let after_rebase = world
        .request("review.get", json!({ "workspace": ws }))
        .await;
    assert_eq!(after_rebase["base"]["applied"]["kind"], "interdiff");
    let replay = after_rebase["base"]["applied_ref"]
        .as_str()
        .expect("ref")
        .to_owned();
    assert!(replay.starts_with("refs/icarus/review-base/"), "{replay}");
    let changes = world
        .request("workspace.changes", json!({ "id": ws }))
        .await;
    assert_eq!(
        rows(&changes),
        vec!["b.rs"],
        "only what changed since the review, not upstream.md"
    );

    let whole = world
        .request(
            "review.set_base",
            json!({ "workspace": ws, "base": "whole" }),
        )
        .await;
    assert_eq!(whole["base_ref"], "origin/main");
    let changes = world
        .request("workspace.changes", json!({ "id": ws }))
        .await;
    assert_eq!(rows(&changes), vec!["b.rs", "src/a.rs"]);
    let invalid = world
        .error(
            "review.set_base",
            json!({ "workspace": ws, "base": "main" }),
        )
        .await;
    assert!(invalid.contains("since-review, whole"), "{invalid}");
    let missing = world
        .error(
            "review.set_base",
            json!({ "workspace": ws, "base": format!("review:{}", "1".repeat(40)) }),
        )
        .await;
    assert!(missing.contains("not in this repository"), "{missing}");
}

#[tokio::test]
async fn a_workspace_without_a_pull_request_says_so() {
    let world = World::new(json!({ "viewer": "me", "pulls": [] })).await;
    let (ws, path, _) = world.workspace("lonely").await;
    let error = world.error("review.get", json!({ "workspace": ws })).await;
    assert!(error.contains("no pull request"), "{error}");
    let by_path = world
        .error(
            "review.get",
            json!({ "workspace": path.join("src").to_string_lossy() }),
        )
        .await;
    assert!(
        by_path.contains("no pull request"),
        "a path inside the workspace resolves it: {by_path}"
    );
}
