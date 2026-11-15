//! Review mode against real GitHub, on a sandbox repository only. It creates and then closes real PRs.
//!
//! Ignored by default. Run with:
//! `ICARUS_LIVE_SANDBOX=owner/name cargo test -p icarus-server --test review_live -- --ignored --nocapture`
//! It refuses any repository that is not private and named like a sandbox.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use icarus_client::Client;
use icarus_github::review::Verdict;
use icarus_github::{GhCli, GitHub, GitHubError};
use icarus_rules::facts::{MergeMethod, RepoRef};
use serde_json::{Value, json};

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run(dir: &Path, program: &str, args: &[&str]) -> String {
    let output = Command::new(program)
        .current_dir(dir)
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .output()
        .unwrap_or_else(|e| panic!("{program}: {e}"));
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Closes the test's PRs and deletes their branches, even when an assertion fails.
struct Cleanup {
    slug: String,
    prs: Vec<u64>,
    branches: Vec<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for pr in self.prs.iter().rev() {
            let _ = Command::new("gh")
                .args(["pr", "close", &pr.to_string(), "--repo", &self.slug])
                .output();
        }
        for branch in &self.branches {
            let _ = Command::new("gh")
                .args([
                    "api",
                    "-X",
                    "DELETE",
                    &format!("repos/{}/git/refs/heads/{branch}", self.slug),
                ])
                .output();
        }
        eprintln!(
            "cleanup: closed PRs {:?}, deleted branches {:?}",
            self.prs, self.branches
        );
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A valid 2x2 RGBA PNG, zlib "stored" so no compressor is needed.
fn tiny_png() -> Vec<u8> {
    let mut raw = Vec::new();
    for _ in 0..2 {
        raw.push(0);
        raw.extend_from_slice(&[0xe0, 0x40, 0x20, 0xff, 0x20, 0x80, 0xe0, 0xff]);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    let len = raw.len() as u16;
    let mut zlib = vec![0x78, 0x01, 0x01];
    zlib.extend_from_slice(&len.to_le_bytes());
    zlib.extend_from_slice(&(!len).to_le_bytes());
    zlib.extend_from_slice(&raw);
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let chunk = |kind: &[u8], data: &[u8]| {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
        out
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&2u32.to_be_bytes());
    ihdr.extend_from_slice(&2u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend(chunk(b"IHDR", &ihdr));
    png.extend(chunk(b"IDAT", &zlib));
    png.extend(chunk(b"IEND", &[]));
    png
}

fn tiny_mp4(dir: &Path) -> Option<PathBuf> {
    let out = dir.join("clip.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=16x16:d=0.2:r=5",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-movflags",
            "+faststart",
        ])
        .arg(&out)
        .status()
        .ok()?;
    status.success().then_some(out)
}

async fn call(client: &Client, method: &str, params: Value) -> Value {
    client
        .request_timeout(method, params, Duration::from_secs(300))
        .await
        .unwrap_or_else(|e| panic!("{method}: {e:#}"))
}

async fn call_err(client: &Client, method: &str, params: Value) -> String {
    format!(
        "{:#}",
        client
            .request_timeout(method, params, Duration::from_secs(300))
            .await
            .expect_err(&format!("{method} should fail"))
    )
}

fn thread_with_line(threads: &Value, line: u64) -> &Value {
    threads["threads"]
        .as_array()
        .expect("threads")
        .iter()
        .find(|t| t["line"] == line)
        .unwrap_or_else(|| panic!("no thread on line {line}: {threads:#}"))
}

#[tokio::test]
#[ignore = "mutates a GitHub sandbox repository; set ICARUS_LIVE_SANDBOX"]
async fn live_review_round_trip_on_the_sandbox() {
    let Ok(slug) = std::env::var("ICARUS_LIVE_SANDBOX") else {
        eprintln!("ICARUS_LIVE_SANDBOX not set; skipping");
        return;
    };
    let (owner, name) = slug.split_once('/').expect("owner/name");
    let repo = RepoRef::new(owner, name);
    let scratch = tempfile::tempdir().expect("scratch");
    let info: Value = serde_json::from_str(&run(
        scratch.path(),
        "gh",
        &["api", &format!("repos/{slug}")],
    ))
    .expect("repo json");
    assert!(
        info["private"] == true && name.to_lowercase().contains("sandbox"),
        "refusing to mutate {slug}: only private sandbox repositories"
    );
    let mut passed: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_secs();
    let branch = format!("icarus-review-live-{stamp}");
    let mut cleanup = Cleanup {
        slug: slug.clone(),
        prs: Vec::new(),
        branches: Vec::new(),
    };

    let clone = scratch.path().join("clone");
    run(
        scratch.path(),
        "gh",
        &[
            "repo",
            "clone",
            &slug,
            clone.to_str().expect("utf8"),
            "--",
            "-q",
        ],
    );
    let home = scratch.path().join("home");
    let config = scratch.path().join("config");
    std::fs::create_dir_all(&config).expect("config");
    std::fs::write(config.join("settings.json"), "{}").expect("settings");
    let _server = ServerProcess(
        Command::new(env!("CARGO_BIN_EXE_icarus-server"))
            .args(["--home", home.to_str().expect("utf8"), "serve"])
            .env("ICARUS_WORKSPACE_ROOT", scratch.path().join("workspaces"))
            .env("ICARUS_CONFIG_DIR", &config)
            .env_remove("ICARUS_GH_FIXTURES")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("server"),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(home.join("server.sock").exists() && home.join("server.token").exists()) {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (client, _events): (Arc<Client>, _) =
        Client::connect(&home, "live").await.expect("connect");
    let project = call(
        &client,
        "project.add",
        json!({ "path": clone, "name": "sandbox" }),
    )
    .await;
    let ws = call(
        &client,
        "workspace.create",
        json!({ "project": project["id"], "name": branch, "branch": branch, "agent": false }),
    )
    .await;
    let ws_id = ws["id"].as_str().expect("id").to_owned();
    let path = PathBuf::from(ws["path"].as_str().expect("path"));
    let text: String = (1..=30)
        .map(|i| format!("Line {i} of the live review file.\n"))
        .collect();
    std::fs::write(path.join("review-live.md"), text).expect("write");
    run(&path, "git", &["add", "."]);
    run(
        &path,
        "git",
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "Live review test file",
        ],
    );
    run(&path, "git", &["push", "-q", "-u", "origin", &branch]);
    cleanup.branches.push(branch.clone());
    let url = run(
        &path,
        "gh",
        &[
            "pr",
            "create",
            "--repo",
            &slug,
            "--head",
            &branch,
            "--base",
            "main",
            "--title",
            "Icarus live review test",
            "--body",
            "Opened and closed by the Icarus live test.",
        ],
    );
    let number: u64 = url
        .rsplit('/')
        .next()
        .and_then(|n| n.parse().ok())
        .expect("pr number");
    cleanup.prs.push(number);
    passed.push(format!("created branch {branch} and PR #{number}"));

    let at = |extra: Value| {
        let mut params = json!({ "workspace": ws_id });
        for (k, v) in extra.as_object().into_iter().flatten() {
            params[k] = v.clone();
        }
        params
    };

    let view = call(&client, "review.get", at(json!({ "refresh": true }))).await;
    assert_eq!(view["pr"]["number"], number);
    assert_eq!(view["viewer_did_author"], true);
    assert_eq!(view["verdicts"], json!(["comment"]));
    passed.push("review.get: own PR found, verdicts limited to comment".into());

    std::fs::write(scratch.path().join("dot.png"), tiny_png()).expect("png");
    let image = call(
        &client,
        "review.upload",
        at(json!({ "path": scratch.path().join("dot.png") })),
    )
    .await;
    let image_url = image["url"].as_str().expect("url").to_owned();
    assert!(
        image_url.starts_with("https://github.com/user-attachments/assets/"),
        "{image}"
    );
    passed.push(format!(
        "uploaded PNG ({} bytes): {image_url}",
        image["size"]
    ));
    let video_markdown = match tiny_mp4(scratch.path()) {
        Some(mp4) => {
            let video = call(&client, "review.upload", at(json!({ "path": mp4 }))).await;
            let video_url = video["url"].as_str().expect("url");
            assert!(
                video_url.starts_with("https://github.com/user-attachments/assets/"),
                "{video}"
            );
            passed.push(format!(
                "uploaded MP4 ({} bytes): {video_url}",
                video["size"]
            ));
            video["markdown"].as_str().expect("markdown").to_owned()
        }
        None => {
            notes.push("ffmpeg missing: MP4 upload skipped".into());
            String::new()
        }
    };

    let single = call(
        &client,
        "review.comment.add",
        at(json!({ "path": "review-live.md", "line": 3,
            "body": format!("Single-line note with an image:\n\n{}", image["markdown"].as_str().expect("md")) })),
    )
    .await;
    assert_eq!(single["file_comment"], false);
    let range = call(
        &client,
        "review.comment.add",
        at(
            json!({ "path": "review-live.md", "start_line": 5, "line": 8,
            "body": format!("Multi-line note with a video:\n{video_markdown}") }),
        ),
    )
    .await;
    assert_eq!(range["draft"]["start_line"], 5);
    call(
        &client,
        "review.set_summary",
        at(json!({ "body": "Live summary from Icarus.", "verdict": "comment" })),
    )
    .await;
    passed.push("queued two drafts (one multi-line) with media, summary and verdict".into());

    let approve = call_err(
        &client,
        "review.submit",
        at(json!({ "verdict": "approve" })),
    )
    .await;
    assert!(approve.contains("own_pull_request"), "{approve}");
    let github = GitHub::new(GhCli::new());
    let context = github.review_context(&repo, number).await.expect("context");
    for verdict in [Verdict::Approve, Verdict::RequestChanges] {
        let error = github
            .submit_review(
                &context.pull_request_id,
                verdict,
                "Verdict on my own PR.",
                &[],
            )
            .await
            .expect_err("GitHub refuses verdicts on your own PR");
        let message = error.to_string();
        assert!(matches!(error, GitHubError::GraphQl(_)), "{message}");
        assert!(
            message.to_lowercase().contains("your own pull request"),
            "{message}"
        );
        passed.push(format!(
            "GitHub refused {} on own PR: {message:?}",
            verdict.name()
        ));
    }

    let submitted = call(&client, "review.submit", at(json!({}))).await;
    assert_eq!(submitted["state"], "COMMENTED");
    assert_eq!(submitted["comments"], 2);
    passed.push(format!(
        "submitted one COMMENT review with 2 comments: {}",
        submitted["url"]
    ));
    let after = call(&client, "review.get", at(json!({}))).await;
    assert_eq!(after["drafts"], json!([]));
    assert_eq!(after["summary"], "");
    assert_eq!(after["viewer_reviews"].as_array().map(Vec::len), Some(1));

    let threads = call(&client, "review.threads", at(json!({}))).await;
    assert_eq!(
        threads["threads"].as_array().map(Vec::len),
        Some(2),
        "{threads:#}"
    );
    let first = thread_with_line(&threads, 3).clone();
    let multi = thread_with_line(&threads, 8);
    assert_eq!(multi["start_line"], 5);
    assert_eq!(multi["placement"]["label"], "Lines 5-8");
    assert!(
        first["comments"][0]["body"]
            .as_str()
            .expect("body")
            .contains(&image_url)
    );
    if !video_markdown.is_empty() {
        assert!(
            multi["comments"][0]["body"]
                .as_str()
                .expect("body")
                .contains("user-attachments")
        );
    }
    passed.push("review.threads read back both threads (range 5-8) with media links".into());

    let thread_id = first["id"].as_str().expect("thread id").to_owned();
    let comment_id = first["comments"][0]["id"]
        .as_str()
        .expect("comment id")
        .to_owned();
    call(
        &client,
        "review.reply",
        at(json!({ "thread": thread_id, "body": "Reply from Icarus." })),
    )
    .await;
    let resolved = call(
        &client,
        "review.resolve",
        at(json!({ "thread": thread_id })),
    )
    .await;
    assert_eq!(resolved["resolved"], true);
    let threads = call(&client, "review.threads", at(json!({}))).await;
    let first = thread_with_line(&threads, 3);
    assert_eq!(first["is_resolved"], true);
    assert_eq!(first["comments"].as_array().map(Vec::len), Some(2));
    assert_eq!(first["comments"][1]["body"], "Reply from Icarus.");
    passed.push("replied and resolved; read back resolved with the reply".into());
    let reopened = call(
        &client,
        "review.unresolve",
        at(json!({ "thread": thread_id })),
    )
    .await;
    assert_eq!(reopened["resolved"], false);
    call(
        &client,
        "review.react",
        at(json!({ "subject": comment_id, "reaction": "+1" })),
    )
    .await;
    call(
        &client,
        "review.react",
        at(json!({ "subject": comment_id, "reaction": "rocket" })),
    )
    .await;
    call(
        &client,
        "review.react",
        at(json!({ "subject": comment_id, "reaction": "rocket", "remove": true })),
    )
    .await;
    let threads = call(&client, "review.threads", at(json!({}))).await;
    let first = thread_with_line(&threads, 3);
    assert_eq!(first["is_resolved"], false);
    let reactions = &first["comments"][0]["reactions"];
    assert_eq!(reactions.as_array().map(Vec::len), Some(1), "{reactions}");
    assert_eq!(reactions[0]["content"], "+1");
    assert_eq!(reactions[0]["viewer_has_reacted"], true);
    passed.push("unresolved; reacted +1, added and removed rocket; read back exactly +1".into());

    let base = call(
        &client,
        "review.set_base",
        at(json!({ "base": "since-review" })),
    )
    .await;
    assert_eq!(base["kind"], "reviewed", "{base}");
    passed.push(format!("base since-review → {}", base["base_ref"]));

    if info["allow_auto_merge"] != true {
        run(
            scratch.path(),
            "gh",
            &[
                "api",
                "-X",
                "PATCH",
                &format!("repos/{slug}"),
                "-F",
                "allow_auto_merge=true",
            ],
        );
        notes.push("enabled \"Allow auto-merge\" on the sandbox".into());
    }
    let meta = github
        .repo_merge_metadata(&repo, "main")
        .await
        .expect("metadata");
    let method = meta.default_method().unwrap_or(MergeMethod::Squash);
    match github
        .enable_auto_merge(&context.pull_request_id, method, Some(&context.head_ref_oid))
        .await
    {
        Ok(()) => {
            github
                .disable_auto_merge(&context.pull_request_id)
                .await
                .expect("disable auto-merge");
            passed.push("auto-merge enabled and disabled".into());
        }
        Err(GitHubError::AlreadyMergeable) => passed.push(
            "auto-merge: GitHub answered \"clean status\" (no required checks), surfaced as AlreadyMergeable".into(),
        ),
        Err(error) => notes.push(format!("auto-merge not available: {error}")),
    }

    let upper = format!("{branch}-top");
    run(&path, "git", &["checkout", "-q", "-b", &upper]);
    std::fs::write(
        path.join("review-live-top.md"),
        "Stacked on the review test.\n",
    )
    .expect("write");
    run(&path, "git", &["add", "."]);
    run(
        &path,
        "git",
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "Stacked change",
        ],
    );
    run(&path, "git", &["push", "-q", "-u", "origin", &upper]);
    cleanup.branches.push(upper.clone());
    let top_url = run(
        &path,
        "gh",
        &[
            "pr",
            "create",
            "--repo",
            &slug,
            "--head",
            &upper,
            "--base",
            &branch,
            "--title",
            "Icarus live stack top",
            "--body",
            "Opened and closed by the Icarus live test.",
        ],
    );
    let top: u64 = top_url
        .rsplit('/')
        .next()
        .and_then(|n| n.parse().ok())
        .expect("pr number");
    cleanup.prs.push(top);
    match github.create_stack(&repo, &[number, top]).await {
        Ok(stack) => {
            let view = github.stack_view(&repo, top).await.expect("stack view");
            passed.push(format!(
                "created stack #{} with PRs {:?}; stack_view size {:?}",
                stack.number,
                stack.pull_requests,
                view.map(|v| v.summary.size)
            ));
        }
        Err(error) => notes.push(format!(
            "stacks API not available to this account/repo: {error}"
        )),
    }

    drop(cleanup);
    passed.push("closed both PRs and deleted both branches".into());
    println!("\nLIVE REVIEW RESULTS ({slug})");
    for line in &passed {
        println!("  PASS {line}");
    }
    for line in &notes {
        println!("  NOTE {line}");
    }
}
