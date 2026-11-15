use icarus_github::http::HttpResponse;
use icarus_github::rate::PollTier;
use icarus_github::snapshot::{PrTarget, build_snapshot};
use icarus_github::stack::{StackSummary, StacksForPr};
use icarus_github::{FakeGh, FakeResponse, GitHub, GitHubError};
use icarus_rules::facts::{
    CheckConclusion, CheckKind, CheckRunStatus, CheckState, MergeMethod, MergeQueueEntryState,
    MergeStateStatus, Mergeable, PrLifecycle, PrState, RepoRef, RequestedReviewer, ReviewDecision,
    ReviewState, StatusContextState,
};
use serde_json::json;
use std::collections::BTreeMap;

fn repo() -> RepoRef {
    RepoRef::new("acme", "widgets")
}

fn client(fake: FakeGh) -> GitHub<FakeGh> {
    GitHub::new(fake)
}

fn snapshot_targets() -> Vec<PrTarget> {
    vec![
        PrTarget::Number(811),
        PrTarget::Branch("billing-export-ui".to_owned()),
        PrTarget::Number(999),
        PrTarget::Branch("gone".to_owned()),
    ]
}

#[tokio::test]
async fn snapshot_sends_one_aliased_query_with_variables() {
    let github = client(FakeGh::new().on(
        "graphql PrSnapshot",
        FakeResponse::ok(include_str!("fixtures/snapshot.json")),
    ));
    github
        .pr_snapshot(&repo(), &snapshot_targets())
        .await
        .expect("snapshot");
    let requests = github.runner().requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.endpoint, "graphql");
    let variables = request.graphql_variables().expect("variables");
    assert_eq!(
        variables,
        &json!({"owner": "acme", "name": "widgets", "n0": 811, "b1": "billing-export-ui", "n2": 999, "b3": "gone"})
    );
    let query = request.json_body().expect("body")["query"]
        .as_str()
        .expect("query")
        .to_owned();
    for needle in [
        "t0: pullRequest(number: $n0)",
        "t1: pullRequests(headRefName: $b1, first: 1",
        "mergeStateStatus",
        "reviewDecision",
        "isInMergeQueue",
        "mergeQueueEntry { state position estimatedTimeToMerge }",
        "autoMergeRequest { mergeMethod enabledBy { login } }",
        "latestOpinionatedReviews",
        "... on User { login avatarUrl(size: 64) }",
        "... on Team { slug combinedSlug avatarUrl(size: 64) }",
        "author { login avatarUrl(size: 64) }",
        "latestReviews(first: 30) { nodes { state author { login avatarUrl(size: 64) }",
        "... on CheckRun { name status conclusion detailsUrl startedAt completedAt }",
        "... on StatusContext { context state targetUrl createdAt }",
        "updatedAt",
        "additions deletions",
        "headRefOid baseRefOid",
        "viewer { login }",
        "rateLimit",
    ] {
        assert!(query.contains(needle), "query lacks {needle}");
    }
}

#[tokio::test]
async fn snapshot_maps_pull_request_facts() {
    let github = client(FakeGh::new().on(
        "graphql PrSnapshot",
        FakeResponse::ok(include_str!("fixtures/snapshot.json")),
    ));
    let snapshot = github
        .pr_snapshot(&repo(), &snapshot_targets())
        .await
        .expect("snapshot");
    assert_eq!(snapshot.viewer_login.as_deref(), Some("octocat"));
    assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);
    assert_eq!(
        snapshot.rate_limit.as_ref().map(|limit| limit.remaining),
        Some(4987)
    );
    assert_eq!(
        snapshot.missing().cloned().collect::<Vec<_>>(),
        vec![PrTarget::Number(999), PrTarget::Branch("gone".to_owned())]
    );

    let pulls: Vec<_> = snapshot.pulls().collect();
    assert_eq!(pulls.len(), 2);
    let details = pulls[0];
    let pr = &details.pr;
    assert_eq!(details.node_id, "PR_kwDOAbc123");
    assert_eq!(
        (details.additions, details.deletions, details.changed_files),
        (120, 14, 6)
    );
    assert!(!details.head_ref_deleted && !details.base_ref_deleted);
    assert_eq!(pr.repo, repo());
    assert_eq!(pr.number, 811);
    assert_eq!(pr.match_key(), "acme/widgets#811");
    assert_eq!(pr.state, PrState::Open);
    assert_eq!(pr.lifecycle(), PrLifecycle::Open);
    assert_eq!(pr.author.as_deref(), Some("octocat"));
    assert_eq!(
        pr.author_avatar_url.as_deref(),
        Some("https://avatars.githubusercontent.com/u/583231?s=64&v=4")
    );
    assert_eq!(pr.head_ref_oid, "1111111111111111111111111111111111111111");
    assert_eq!(
        pr.base_ref_oid.as_deref(),
        Some("2222222222222222222222222222222222222222")
    );
    assert_eq!(pr.mergeable, Some(Mergeable::Mergeable));
    assert_eq!(pr.merge_state_status, Some(MergeStateStatus::Blocked));
    assert_eq!(pr.review_decision, Some(ReviewDecision::ReviewRequired));
    assert_eq!(
        pr.requested_reviewers,
        vec![
            RequestedReviewer::user("hubot").with_avatar_url(Some(
                "https://avatars.githubusercontent.com/u/480938?s=64&v=4".to_owned()
            )),
            RequestedReviewer::team("acme/pm").with_avatar_url(Some(
                "https://avatars.githubusercontent.com/t/42?s=64&v=4".to_owned()
            ))
        ]
    );
    assert_eq!(
        details
            .code_owner_requests
            .iter()
            .map(|reviewer| reviewer.avatar_url())
            .collect::<Vec<_>>(),
        vec![Some("https://avatars.githubusercontent.com/t/42?s=64&v=4")]
    );
    assert_eq!(pr.latest_reviews.len(), 1);
    assert_eq!(pr.latest_reviews[0].login, "monalisa");
    assert_eq!(pr.latest_reviews[0].state, ReviewState::Approved);
    assert_eq!(
        pr.latest_reviews[0].author_avatar_url.as_deref(),
        Some("https://avatars.githubusercontent.com/u/2?s=64&v=4")
    );
    let opinionated = pr.latest_opinionated_reviews.as_ref().expect("opinionated");
    assert_eq!(opinionated[0].state, ReviewState::ChangesRequested);
    assert_eq!(
        opinionated[0].author_avatar_url.as_deref(),
        Some("https://avatars.githubusercontent.com/u/480938?s=64&v=4")
    );
    assert_eq!(opinionated[1].author_avatar_url, None);
    assert_eq!(
        serde_json::to_value(&pr.requested_reviewers[1]).expect("json"),
        json!({"kind": "team", "slug": "acme/pm", "avatar_url": "https://avatars.githubusercontent.com/t/42?s=64&v=4"})
    );
    assert_eq!(
        opinionated[0].commit_oid.as_deref(),
        Some("3333333333333333333333333333333333333333")
    );
    assert_eq!(pr.viewer_latest_review, None);
    assert!(pr.is_in_merge_queue);
    let entry = pr.merge_queue_entry.as_ref().expect("queue entry");
    assert_eq!(entry.state, MergeQueueEntryState::AwaitingChecks);
    assert_eq!((entry.position, entry.eta_seconds), (Some(3), Some(720)));
    let auto_merge = pr.auto_merge.as_ref().expect("auto merge");
    assert_eq!(auto_merge.method, Some(MergeMethod::Squash));
    assert_eq!(auto_merge.enabled_by.as_deref(), Some("octocat"));
    assert_eq!(pr.auto_merge_allowed, Some(true));
    assert_eq!(pr.merge_queue_required, None);
    assert_eq!(pr.checks_rollup, Some(CheckState::Pending));
    assert_eq!(pr.updated_at.as_deref(), Some("2026-11-15T11:58:00Z"));
    let build = pr.checks.iter().find(|c| c.name == "build").expect("build");
    assert_eq!(build.started_at.as_deref(), Some("2026-11-15T11:50:00Z"));
    assert_eq!(build.completed_at.as_deref(), Some("2026-11-15T11:54:30Z"));
    let legacy = pr
        .checks
        .iter()
        .find(|c| c.name == "ci/legacy")
        .expect("legacy");
    assert_eq!(legacy.started_at.as_deref(), Some("2026-11-15T11:51:00Z"));
    assert_eq!(legacy.completed_at, None);
    let lint = pr.checks.iter().find(|c| c.name == "lint").expect("lint");
    assert_eq!(
        (lint.started_at.as_ref(), lint.completed_at.as_ref()),
        (None, None)
    );
}

#[tokio::test]
async fn snapshot_keeps_waiting_requested_and_pending_distinct() {
    let github = client(FakeGh::new().on(
        "graphql PrSnapshot",
        FakeResponse::ok(include_str!("fixtures/snapshot.json")),
    ));
    let snapshot = github
        .pr_snapshot(&repo(), &snapshot_targets())
        .await
        .expect("snapshot");
    let pr = &snapshot.pulls().next().expect("pr").pr;
    let kinds: Vec<(&str, &CheckKind)> = pr
        .checks
        .iter()
        .map(|check| (check.name.as_str(), &check.kind))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (
                "build",
                &CheckKind::Run {
                    status: CheckRunStatus::Completed,
                    conclusion: Some(CheckConclusion::Success)
                }
            ),
            (
                "deploy-staging",
                &CheckKind::Run {
                    status: CheckRunStatus::Waiting,
                    conclusion: None
                }
            ),
            (
                "lint",
                &CheckKind::Run {
                    status: CheckRunStatus::Requested,
                    conclusion: None
                }
            ),
            (
                "e2e",
                &CheckKind::Run {
                    status: CheckRunStatus::Pending,
                    conclusion: None
                }
            ),
            (
                "flaky",
                &CheckKind::Run {
                    status: CheckRunStatus::Completed,
                    conclusion: Some(CheckConclusion::Stale)
                }
            ),
            (
                "ci/legacy",
                &CheckKind::Status {
                    state: StatusContextState::Error
                }
            ),
            (
                "preview",
                &CheckKind::Status {
                    state: StatusContextState::Expected
                }
            ),
        ]
    );
    assert!(pr.checks[1].is_waiting_approval());
    assert_eq!(pr.checks[1].state(), CheckState::Pending);
    assert!(pr.checks[4].is_triage_failure());
    assert_eq!(
        pr.checks[0].url.as_deref(),
        Some("https://github.com/acme/widgets/actions/runs/1/job/1")
    );
    assert_eq!(
        pr.checks[5].url.as_deref(),
        Some("https://ci.example.com/build/9")
    );
}

#[tokio::test]
async fn snapshot_maps_branch_lookup_merged_pr() {
    let github = client(FakeGh::new().on(
        "graphql PrSnapshot",
        FakeResponse::ok(include_str!("fixtures/snapshot.json")),
    ));
    let snapshot = github
        .pr_snapshot(&repo(), &snapshot_targets())
        .await
        .expect("snapshot");
    let entry = &snapshot.entries[1];
    assert_eq!(
        entry.target,
        PrTarget::Branch("billing-export-ui".to_owned())
    );
    let details = entry.pull.as_ref().expect("branch pr");
    assert!(details.head_ref_deleted);
    assert!(details.is_cross_repository);
    let pr = &details.pr;
    assert_eq!(pr.lifecycle(), PrLifecycle::Merged);
    assert_eq!(pr.mergeable, Some(Mergeable::Unknown));
    assert_eq!(pr.merge_state_status, Some(MergeStateStatus::Unknown));
    assert_eq!(pr.review_decision, None);
    assert_eq!(pr.checks_rollup, None);
    assert!(pr.checks.is_empty());
    let viewer = pr.viewer_latest_review.as_ref().expect("viewer review");
    assert_eq!(viewer.state, "COMMENTED");
}

#[tokio::test]
async fn snapshot_chunks_large_target_lists() {
    let github = client(FakeGh::new().on(
        "graphql PrSnapshot",
        FakeResponse::ok(include_str!("fixtures/snapshot.json")),
    ));
    let targets: Vec<_> = (1..=51).map(PrTarget::Number).collect();
    let snapshot = github
        .pr_snapshot(&repo(), &targets)
        .await
        .expect("snapshot");
    assert_eq!(github.runner().count("graphql PrSnapshot"), 2);
    assert_eq!(snapshot.entries.len(), 51);
}

#[tokio::test]
async fn review_inbox_scopes_search_to_repos_and_skips_issues() {
    let github = client(FakeGh::new().on(
        "graphql ReviewInbox",
        FakeResponse::ok(include_str!("fixtures/inbox.json")),
    ));
    let inbox = github.review_inbox(&[repo()]).await.expect("inbox");
    let request = github
        .runner()
        .request("graphql ReviewInbox")
        .expect("request");
    assert_eq!(
        request.graphql_variables().expect("variables"),
        &json!({"q": "is:pr is:open review-requested:@me archived:false repo:acme/widgets", "first": 30})
    );
    assert_eq!(inbox.viewer_login.as_deref(), Some("octocat"));
    assert_eq!(inbox.pulls.len(), 1);
    let pr = &inbox.pulls[0].pr;
    assert_eq!(pr.repo, repo());
    assert_eq!(pr.number, 42);
    assert_eq!(pr.mergeable, Some(Mergeable::Conflicting));
    assert_eq!(pr.merge_state_status, Some(MergeStateStatus::Dirty));
    assert_eq!(pr.pending_user_reviewers(), vec!["octocat".to_owned()]);
    assert_eq!(pr.auto_merge_allowed, Some(false));
}

#[tokio::test]
async fn review_inbox_without_repos_makes_no_call() {
    let github = client(FakeGh::new());
    let inbox = github.review_inbox(&[]).await.expect("inbox");
    assert!(inbox.pulls.is_empty());
    assert!(github.runner().requests().is_empty());
}

#[tokio::test]
async fn review_threads_map_paths_lines_and_authors() {
    let github = client(FakeGh::new().on(
        "graphql ReviewThreads",
        FakeResponse::ok(include_str!("fixtures/review_threads.json")),
    ));
    let threads = github.review_threads(&repo(), 811).await.expect("threads");
    let variables = github
        .runner()
        .request("graphql ReviewThreads")
        .expect("request")
        .graphql_variables()
        .cloned();
    assert_eq!(
        variables,
        Some(json!({"owner": "acme", "name": "widgets", "number": 811, "after": null}))
    );
    assert_eq!(threads.len(), 3);
    let first = &threads[0];
    assert_eq!(first.path.as_deref(), Some("src/billing.rs"));
    assert_eq!(first.line, Some(42));
    assert!(!first.is_resolved && !first.is_outdated);
    assert_eq!(first.comments.len(), 2);
    assert_eq!(first.comments[0].author.as_deref(), Some("monalisa"));
    assert_eq!(
        first.url(),
        Some("https://github.com/acme/widgets/pull/811#discussion_r1")
    );
    assert_eq!(
        threads[1].line,
        Some(7),
        "outdated threads fall back to the original line"
    );
    assert!(threads[1].is_resolved && threads[1].is_outdated);
    assert!(threads[2].comments[0].is_bot);
    assert!(!threads[2].has_human_comment());
    assert_eq!(threads[2].start_line, Some(1));
}

#[tokio::test]
async fn viewer_login_reads_viewer() {
    let github = client(FakeGh::new().on(
        "graphql Viewer",
        FakeResponse::ok(r#"{"data":{"viewer":{"login":"octocat"}}}"#),
    ));
    assert_eq!(github.viewer_login().await.expect("viewer"), "octocat");
}

#[tokio::test]
async fn team_members_follow_pagination() {
    let page = |logins: &[&str], next: Option<&str>| {
        let nodes: Vec<_> = logins.iter().map(|login| json!({"login": login})).collect();
        json!({"data": {"organization": {"team": {"members": {
            "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next},
            "nodes": nodes
        }}}}})
        .to_string()
    };
    let github = client(
        FakeGh::new()
            .on(
                "graphql TeamMembers",
                FakeResponse::ok(page(&["Monalisa", "hubot"], Some("Y3Vyc29y"))),
            )
            .on(
                "graphql TeamMembers",
                FakeResponse::ok(page(&["octocat"], None)),
            ),
    );
    let roster = github
        .team_roster(&["acme/pm".to_owned()])
        .await
        .expect("roster");
    assert_eq!(
        roster.get("acme/pm"),
        Some(&vec![
            "Monalisa".to_owned(),
            "hubot".to_owned(),
            "octocat".to_owned()
        ])
    );
    let requests = github.runner().requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].graphql_variables().expect("variables"),
        &json!({"org": "acme", "slug": "pm", "after": null})
    );
    assert_eq!(
        requests[1].graphql_variables().expect("variables")["after"],
        "Y3Vyc29y"
    );
}

#[tokio::test]
async fn team_members_rejects_bad_names_and_missing_teams() {
    let github = client(FakeGh::new().on(
        "graphql TeamMembers",
        FakeResponse::ok(r#"{"data":{"organization":{"team":null}}}"#),
    ));
    assert!(matches!(
        github.team_members("pm").await,
        Err(GitHubError::Invalid(_))
    ));
    assert!(matches!(
        github.team_members("acme/pm").await,
        Err(GitHubError::NotFound(_))
    ));
}

#[tokio::test]
async fn build_snapshot_feeds_rules_facts() {
    let github = client(
        FakeGh::new()
            .on(
                "graphql PrSnapshot",
                FakeResponse::ok(include_str!("fixtures/snapshot.json")),
            )
            .on(
                "graphql ReviewInbox",
                FakeResponse::ok(include_str!("fixtures/inbox.json")),
            ),
    );
    let linked = github
        .pr_snapshot(&repo(), &snapshot_targets())
        .await
        .expect("snapshot");
    let inbox = github.review_inbox(&[repo()]).await.expect("inbox");
    let linked: Vec<_> = linked.pulls().cloned().collect();
    let mut teams = BTreeMap::new();
    teams.insert("acme/pm".to_owned(), vec!["hubot".to_owned()]);
    let snapshot = build_snapshot(Some("octocat".to_owned()), &linked, &inbox.pulls, teams);
    assert_eq!(snapshot.linked.len(), 2);
    assert_eq!(snapshot.review_requested[0].number, 42);
    assert_eq!(snapshot.team_members["acme/pm"], vec!["hubot".to_owned()]);
    let encoded = serde_json::to_value(&snapshot).expect("serialize");
    assert_eq!(
        encoded["linked"][0]["checks"][1]["kind"]["status"],
        "WAITING"
    );
}

#[tokio::test]
async fn repo_merge_metadata_resolves_methods_and_queue() {
    let github = client(FakeGh::new().on(
        "graphql RepoMergeMetadata",
        FakeResponse::ok(include_str!("fixtures/repo_merge_metadata.json")),
    ));
    let metadata = github
        .repo_merge_metadata(&repo(), "main")
        .await
        .expect("metadata");
    assert_eq!(metadata.allowed_methods, vec![MergeMethod::Squash]);
    assert_eq!(metadata.default_method(), Some(MergeMethod::Squash));
    assert_eq!(metadata.merge_queue_required, Some(true));
    assert_eq!(metadata.auto_merge_allowed, Some(true));
    let request = github
        .runner()
        .request("graphql RepoMergeMetadata")
        .expect("request");
    assert_eq!(
        request.graphql_variables().expect("variables"),
        &json!({"owner": "acme", "name": "widgets", "branch": "main", "qualified": "refs/heads/main"})
    );
    let mut pr = icarus_rules::facts::PullRequest::default();
    metadata.apply(&mut pr);
    assert_eq!(pr.merge_queue_required, Some(true));
}

#[tokio::test]
async fn stack_probe_uses_api_version_and_etag() {
    let github = client(
        FakeGh::new()
            .on(
                "GET repos/acme/widgets/pulls/811",
                FakeResponse::ok(include_str!("fixtures/pull_with_stack.json"))
                    .header("ETag", "W/\"abc\""),
            )
            .on(
                "GET repos/acme/widgets/pulls/811",
                FakeResponse::status(304),
            ),
    );
    let first = github.probe_pull_stack(&repo(), 811).await.expect("probe");
    assert!(first.changed);
    assert_eq!(
        first.head_sha.as_deref(),
        Some("1111111111111111111111111111111111111111")
    );
    assert_eq!(
        first.stack,
        Some(StackSummary {
            number: 42,
            position: 2,
            size: 3,
            base_ref: "main".to_owned(),
            base_sha: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()),
        })
    );
    let second = github.probe_pull_stack(&repo(), 811).await.expect("probe");
    assert!(!second.changed);
    assert_eq!(second.stack, first.stack);

    let requests = github.runner().requests();
    let version = ("X-GitHub-Api-Version".to_owned(), "2026-03-10".to_owned());
    assert!(requests[0].headers.contains(&version));
    assert!(
        !requests[0]
            .headers
            .iter()
            .any(|(name, _)| name == "If-None-Match")
    );
    assert!(
        requests[1]
            .headers
            .contains(&("If-None-Match".to_owned(), "W/\"abc\"".to_owned()))
    );
}

#[tokio::test]
async fn pull_without_stack_has_no_stack() {
    let github = client(FakeGh::new().on(
        "GET repos/acme/widgets/pulls/7",
        FakeResponse::ok(r#"{"number":7,"head":{"sha":"1111111111111111111111111111111111111111"},"stack":null}"#),
    ));
    assert_eq!(github.stack_view(&repo(), 7).await.expect("view"), None);
}

#[tokio::test]
async fn stack_view_maps_entries_and_out_of_date() {
    let github = client(
        FakeGh::new()
            .on(
                "GET repos/acme/widgets/pulls/811",
                FakeResponse::ok(include_str!("fixtures/pull_with_stack.json")),
            )
            .on(
                "graphql StackDetails",
                FakeResponse::ok(include_str!("fixtures/stack_details.json")),
            ),
    );
    let view = github
        .stack_view(&repo(), 811)
        .await
        .expect("view")
        .expect("stack");
    let details = github
        .runner()
        .request("graphql StackDetails")
        .expect("request");
    assert_eq!(
        details.graphql_variables().expect("variables"),
        &json!({"owner": "acme", "name": "widgets", "pr": 811, "baseRef": "refs/heads/main"})
    );
    assert!(view.out_of_date());
    let entries = view.entries.as_ref().expect("entries");
    assert_eq!(
        entries.iter().map(|entry| entry.number).collect::<Vec<_>>(),
        vec![810, 811, 812]
    );
    assert_eq!(entries[0].state, PrLifecycle::Merged);
    assert_eq!(entries[1].state, PrLifecycle::Draft);
    assert!(entries[1].in_merge_queue);
    assert_eq!(entries[1].merge_state_status, Some(MergeStateStatus::Clean));
    assert_eq!(entries[1].title.as_deref(), Some("Billing export UI"));
    assert_eq!(
        entries[1].head_ref_name.as_deref(),
        Some("billing-export-ui")
    );
    assert_eq!(
        entries[1].url.as_deref(),
        Some("https://github.com/acme/widgets/pull/811")
    );
    assert!(entries[1].is_draft && !entries[2].is_draft);
    assert_eq!(entries[1].checks_rollup, Some(CheckState::Failure));
    assert_eq!(entries[2].checks_rollup, None);
    assert_eq!(entries[2].title, None);
    assert_eq!(entries[1].author.as_deref(), Some("dependabot"));
    assert_eq!(
        entries[1].author_avatar_url.as_deref(),
        Some("https://avatars.githubusercontent.com/in/29110?s=64&v=4")
    );
    assert_eq!(
        (&entries[2].author, &entries[2].author_avatar_url),
        (&None, &None)
    );
    assert!(
        details.json_body().expect("body")["query"]
            .as_str()
            .is_some_and(|q| q.contains("statusCheckRollup")
                && q.contains("headRefName")
                && q.contains("author { login avatarUrl(size: 64) }"))
    );
    assert_eq!(
        entries[2].review_decision,
        Some(ReviewDecision::ReviewRequired)
    );
    let fact = view.to_fact(Some(true), None);
    assert_eq!((fact.number, fact.position, fact.size), (42, 2, 3));
    assert_eq!(fact.base_ref_name, "main");
    assert_eq!(fact.merge_queue_required, Some(true));
    assert_eq!(fact.entries.len(), 3);
}

#[tokio::test]
async fn stack_view_degrades_when_details_fail() {
    let github = client(
        FakeGh::new()
            .on(
                "GET repos/acme/widgets/pulls/811",
                FakeResponse::ok(include_str!("fixtures/pull_with_stack.json")),
            )
            .on(
                "graphql StackDetails",
                FakeResponse::ok(r#"{"data":{"repository":{"baseTip":null,"pullRequest":{"stack":{"number":7,"entries":{"nodes":[]}}}}}}"#),
            ),
    );
    let view = github
        .stack_view(&repo(), 811)
        .await
        .expect("view")
        .expect("stack");
    assert_eq!(view.entries, None);
    assert!(!view.out_of_date());
}

#[tokio::test]
async fn stacks_for_pr_treats_404_as_no_stacks() {
    let github = client(FakeGh::new().on(
        "GET repos/acme/widgets/stacks?pull_request=811",
        FakeResponse::json(404, r#"{"message":"Not Found"}"#),
    ));
    assert_eq!(
        github.stacks_for_pr(&repo(), 811).await.expect("stacks"),
        StacksForPr::Unavailable
    );
}

#[tokio::test]
async fn stacks_for_pr_lists_stacks() {
    let github = client(FakeGh::new().on(
        "GET repos/acme/widgets/stacks?pull_request=811",
        FakeResponse::ok(
            r#"[{"number":42,"open":true,"pull_requests":[{"number":810},{"number":811}]}]"#,
        ),
    ));
    let StacksForPr::Stacks(stacks) = github.stacks_for_pr(&repo(), 811).await.expect("stacks")
    else {
        panic!("expected stacks");
    };
    assert_eq!(stacks[0].number, 42);
    assert!(stacks[0].open);
    assert_eq!(stacks[0].pull_requests, vec![810, 811]);
    let request = &github.runner().requests()[0];
    assert!(
        request
            .headers
            .contains(&("X-GitHub-Api-Version".to_owned(), "2026-03-10".to_owned()))
    );
}

#[tokio::test]
async fn rate_limits_drive_poll_tier() {
    let github = client(FakeGh::new().on(
        "GET rate_limit",
        FakeResponse::ok(include_str!("fixtures/rate_limit.json")),
    ));
    let limits = github.rate_limits().await.expect("limits");
    assert_eq!(limits["graphql"].remaining, 380);
    let budget = github.budget();
    assert_eq!(budget.tier_at(0), PollTier::FingerprintOnly);
    assert!(!budget.allows_at("search", 0));
    assert!(budget.allows_at("core", 0));
    assert_eq!(budget.tier_at(4_102_444_801), PollTier::Full);
}

#[tokio::test]
async fn response_headers_update_budget_and_backoff() {
    let github = client(
        FakeGh::new()
            .on(
                "graphql Viewer",
                FakeResponse::ok(r#"{"data":{"viewer":{"login":"octocat"}}}"#)
                    .header("X-RateLimit-Limit", "5000")
                    .header("X-RateLimit-Remaining", "40")
                    .header("X-RateLimit-Reset", "4102444800")
                    .header("X-RateLimit-Resource", "graphql"),
            )
            .on(
                "GET rate_limit",
                FakeResponse::json(
                    429,
                    r#"{"message":"You have exceeded a secondary rate limit."}"#,
                )
                .header("Retry-After", "60"),
            ),
    );
    github.viewer_login().await.expect("viewer");
    assert_eq!(
        github.budget().get("graphql").map(|limit| limit.remaining),
        Some(40)
    );
    assert_eq!(github.budget().tier_at(0), PollTier::Stopped);
    let error = github.rate_limits().await.expect_err("secondary limit");
    assert!(matches!(
        error,
        GitHubError::Http { status: 429, retry_after: Some(wait), .. } if wait.as_secs() == 60
    ));
    assert!(github.budget().backoff_until().is_some());
    assert_eq!(github.budget().tier(), PollTier::Stopped);
}

#[test]
fn parses_included_headers() {
    let raw = b"HTTP/2.0 200 OK\r\nEtag: \"x\"\r\nX-Ratelimit-Remaining: 7\r\n\r\n{\"a\":1}";
    let response = HttpResponse::parse(raw).expect("response");
    assert_eq!(response.status, 200);
    assert_eq!(response.header("ETag"), Some("\"x\""));
    assert_eq!(response.header("x-ratelimit-remaining"), Some("7"));
    assert_eq!(response.body, b"{\"a\":1}");
    let no_body = HttpResponse::parse(b"HTTP/1.1 304 Not Modified\nETag: y\n").expect("304");
    assert_eq!(no_body.status, 304);
    assert!(HttpResponse::parse(b"{\"not\":\"http\"}").is_none());
}
