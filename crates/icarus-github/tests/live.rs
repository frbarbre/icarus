//! Read-only checks against real GitHub through the user's `gh` login.
//!
//! Ignored by default. Run with:
//! `ICARUS_LIVE_REPO=owner/name cargo test -p icarus-github --test live -- --ignored --nocapture`
//! It never calls a mutation.

use icarus_github::previews::PreviewConfig;
use icarus_github::snapshot::PrTarget;
use icarus_github::{GhCli, GitHub};
use icarus_rules::facts::RepoRef;

fn live_repo() -> Option<RepoRef> {
    let value = std::env::var("ICARUS_LIVE_REPO").ok()?;
    let (owner, name) = value.split_once('/')?;
    Some(RepoRef::new(owner, name))
}

fn recent_pull_numbers(repo: &RepoRef, state: &str) -> Vec<u64> {
    let output = std::process::Command::new("gh")
        .args([
            "pr",
            "list",
            "--repo",
            &format!("{}/{}", repo.owner, repo.name),
            "--state",
            state,
            "--limit",
            "15",
            "--json",
            "number",
            "--jq",
            ".[].number",
        ])
        .output()
        .expect("gh pr list");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

#[tokio::test]
#[ignore = "talks to GitHub; set ICARUS_LIVE_REPO"]
async fn live_queries_parse_real_pull_requests() {
    let Some(repo) = live_repo() else {
        eprintln!("ICARUS_LIVE_REPO not set; skipping");
        return;
    };
    let github = GitHub::new(GhCli::new());

    let viewer = github.viewer_login().await.expect("viewer login");
    println!("viewer: {viewer}");

    let mut numbers = recent_pull_numbers(&repo, "open");
    numbers.extend(recent_pull_numbers(&repo, "merged").into_iter().take(5));
    assert!(
        !numbers.is_empty(),
        "the repo has no pull requests to check"
    );
    let targets: Vec<PrTarget> = numbers.iter().copied().map(PrTarget::Number).collect();
    let snapshot = github
        .pr_snapshot(&repo, &targets)
        .await
        .expect("pr snapshot");
    assert!(
        snapshot.errors.is_empty(),
        "GraphQL errors: {:?}",
        snapshot.errors
    );
    let pulls: Vec<_> = snapshot.pulls().collect();
    println!(
        "snapshot: {} pull requests, rate limit {:?}",
        pulls.len(),
        snapshot.rate_limit
    );
    for details in &pulls {
        let pr = &details.pr;
        println!(
            "  #{} {:?} draft={} mergeable={:?} merge_state={:?} checks={} reviews={} queue={:?}",
            pr.number,
            pr.state,
            pr.is_draft,
            pr.mergeable,
            pr.merge_state_status,
            pr.checks.len(),
            pr.latest_reviews.len(),
            pr.merge_queue_entry.as_ref().map(|e| e.position),
        );
    }
    assert_eq!(pulls.len(), numbers.len(), "every requested PR maps");
    for details in pulls.iter().take(3) {
        let pr = &details.pr;
        println!(
            "  #{} author {:?} avatar {:?}; reviewer avatars {:?}",
            pr.number,
            pr.author,
            pr.author_avatar_url,
            pr.latest_reviews
                .iter()
                .map(|review| review.author_avatar_url.as_deref())
                .collect::<Vec<_>>()
        );
    }
    assert!(
        pulls
            .iter()
            .filter(|details| details.pr.author.is_some())
            .all(|details| details.pr.author_avatar_url.is_some()),
        "every authored PR carries an avatar"
    );

    let inbox = github
        .review_inbox(std::slice::from_ref(&repo))
        .await
        .expect("review inbox");
    println!("review requests for {viewer}: {}", inbox.pulls.len());

    let metadata = github
        .repo_merge_metadata(&repo, "main")
        .await
        .expect("merge metadata");
    println!("merge metadata: {metadata:?}");

    for number in numbers.iter().take(8) {
        match github.stack_view(&repo, *number).await {
            Ok(Some(view)) => {
                println!(
                    "  #{number} is in a stack of {}; out of date: {}; avatars {:?}",
                    view.entries.as_ref().map_or(0, Vec::len),
                    view.out_of_date(),
                    view.entries
                        .iter()
                        .flatten()
                        .map(|entry| entry.author_avatar_url.as_deref())
                        .collect::<Vec<_>>()
                );
            }
            Ok(None) => {}
            Err(error) => println!("  #{number} stack lookup error: {error}"),
        }
    }
}

#[tokio::test]
#[ignore = "talks to GitHub; set ICARUS_LIVE_REPO and ICARUS_LIVE_PREVIEW_PR"]
async fn live_previews_of_a_real_pull_request() {
    let (Some(repo), Some(number)) = (
        live_repo(),
        std::env::var("ICARUS_LIVE_PREVIEW_PR")
            .ok()
            .and_then(|n| n.trim().parse::<u64>().ok()),
    ) else {
        eprintln!("ICARUS_LIVE_REPO or ICARUS_LIVE_PREVIEW_PR not set; skipping");
        return;
    };
    let github = GitHub::new(GhCli::new());
    let found = github
        .pr_previews(&repo, number, &PreviewConfig::default())
        .await
        .expect("pr previews");
    for preview in &found.previews {
        println!(
            "  {:<12} {:?} {:<11} head={} {:?} {:?}",
            preview.app,
            preview.source,
            preview.state,
            preview.is_head,
            preview.environment,
            preview.url
        );
    }
    println!("primary: {:?}", found.primary.as_ref().map(|p| &p.url));
    assert!(
        found
            .previews
            .iter()
            .all(|p| p.url.is_some() || p.log_url.is_some() || p.state != "success"),
        "a successful preview without any link: {:?}",
        found.previews
    );
}
