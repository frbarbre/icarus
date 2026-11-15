use icarus_github::snapshot::PrTarget;
use icarus_github::stack::StackMergeAction;
use icarus_github::{FakeGh, FakeResponse, GhOutput, GitHub, GitHubError};
use icarus_rules::facts::{CheckKind, CheckRunStatus, MergeMethod, RepoRef};

const HEAD: &str = "1111111111111111111111111111111111111111";

fn repo() -> RepoRef {
    RepoRef::new("acme", "widgets")
}

fn snapshot_with(body: &str) -> GitHub<FakeGh> {
    GitHub::new(FakeGh::new().on("graphql PrSnapshot", FakeResponse::ok(body)))
}

#[tokio::test]
async fn non_json_body_is_an_error() {
    let github = snapshot_with("<html>oops</html>");
    let error = github
        .pr_snapshot(&repo(), &[PrTarget::Number(1)])
        .await
        .expect_err("html");
    assert!(matches!(error, GitHubError::Malformed(_)));
}

#[tokio::test]
async fn truncated_json_is_an_error() {
    let truncated = &include_str!("fixtures/snapshot.json")[..400];
    let github = snapshot_with(truncated);
    assert!(matches!(
        github.pr_snapshot(&repo(), &[PrTarget::Number(811)]).await,
        Err(GitHubError::Malformed(_))
    ));
}

#[tokio::test]
async fn gh_failure_without_http_output_keeps_stderr() {
    let github = GitHub::new(FakeGh::new().on_raw(
        "graphql Viewer",
        GhOutput {
            success: false,
            stdout: Vec::new(),
            stderr: "To get started with GitHub CLI, please run:  gh auth login\n".to_owned(),
        },
    ));
    let error = github.viewer_login().await.expect_err("logged out");
    assert!(matches!(&error, GitHubError::Gh(message) if message.contains("gh auth login")));
}

#[tokio::test]
async fn missing_fixture_is_an_error_not_a_panic() {
    let github = GitHub::new(FakeGh::new());
    assert!(matches!(
        github.viewer_login().await,
        Err(GitHubError::Gh(_))
    ));
}

#[tokio::test]
async fn successful_exit_without_http_is_malformed() {
    let github = GitHub::new(FakeGh::new().on_raw(
        "graphql Viewer",
        GhOutput {
            success: true,
            stdout: b"{\"data\":{}}".to_vec(),
            stderr: String::new(),
        },
    ));
    assert!(matches!(
        github.viewer_login().await,
        Err(GitHubError::Malformed(_))
    ));
}

#[tokio::test]
async fn repository_of_wrong_type_is_malformed() {
    let github =
        snapshot_with(r#"{"data":{"viewer":{"login":"octocat"},"repository":"acme/widgets"}}"#);
    assert!(matches!(
        github.pr_snapshot(&repo(), &[PrTarget::Number(1)]).await,
        Err(GitHubError::Malformed(_))
    ));
}

#[tokio::test]
async fn missing_repository_is_not_found() {
    let github = snapshot_with(
        r#"{"data":{"viewer":{"login":"octocat"},"repository":null},"errors":[{"type":"NOT_FOUND","path":["repository"],"message":"Could not resolve to a Repository with the name 'acme/widgets'."}]}"#,
    );
    assert!(matches!(
        github.pr_snapshot(&repo(), &[PrTarget::Number(1)]).await,
        Err(GitHubError::NotFound(_))
    ));
}

#[tokio::test]
async fn errors_without_data_are_graphql_errors() {
    let github = snapshot_with(
        r#"{"errors":[{"message":"Field 'stackEntry' doesn't exist on type 'PullRequest'"}]}"#,
    );
    let error = github
        .pr_snapshot(&repo(), &[PrTarget::Number(1)])
        .await
        .expect_err("schema");
    assert!(matches!(&error, GitHubError::GraphQl(messages) if messages[0].contains("stackEntry")));
}

#[tokio::test]
async fn partial_pr_node_is_reported_not_fatal() {
    let github = snapshot_with(
        r#"{"data":{"viewer":null,"repository":{
            "t0":{"number":1,"state":"OPEN"},
            "t1":{"id":"PR_2","number":2,"state":"OPEN","mergeable":"MAYBE"},
            "t2":{"id":"PR_3","number":3,"state":"CLOSED","commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"FAILURE","contexts":{"nodes":[
              {"__typename":"CheckRun","name":"new","status":"SOMETHING_NEW","conclusion":null},
              {"__typename":"FutureContext","id":"x"}
            ]}}}}]}}
        }}}"#,
    );
    let snapshot = github
        .pr_snapshot(
            &repo(),
            &[
                PrTarget::Number(1),
                PrTarget::Number(2),
                PrTarget::Number(3),
            ],
        )
        .await
        .expect("partial");
    assert_eq!(snapshot.viewer_login, None);
    assert_eq!(snapshot.errors.len(), 2);
    let pulls: Vec<_> = snapshot.pulls().collect();
    assert_eq!(pulls.len(), 1);
    let pr = &pulls[0].pr;
    assert_eq!(pr.number, 3);
    assert_eq!(pr.checks.len(), 1);
    assert_eq!(
        pr.checks[0].kind,
        CheckKind::Run {
            status: CheckRunStatus::Unknown,
            conclusion: None
        }
    );
    assert!(pulls[0].head_ref_deleted);
}

#[tokio::test]
async fn malformed_stack_metadata_is_refused() {
    for stack in [
        r#"{"number":42,"position":4,"size":3,"base":{"ref":"main"}}"#,
        r#"{"number":42,"position":1,"size":3,"base":{"ref":""}}"#,
        r#"{"number":0,"position":1,"size":3,"base":{"ref":"main"}}"#,
        r#"{"number":42,"position":1,"size":3,"base":{"ref":"main","sha":"not-a-sha"}}"#,
        r#""stack""#,
    ] {
        let github = GitHub::new(FakeGh::new().on(
            "GET repos/acme/widgets/pulls/811",
            FakeResponse::ok(format!(r#"{{"number":811,"stack":{stack}}}"#)),
        ));
        assert!(
            matches!(
                github.probe_pull_stack(&repo(), 811).await,
                Err(GitHubError::StackMetadataUnavailable)
            ),
            "{stack}"
        );
    }
}

#[tokio::test]
async fn garbled_merge_async_response_is_an_error() {
    let github = GitHub::new(
        FakeGh::new()
            .on(
                "PUT repos/acme/widgets/pulls/1/merge-async",
                FakeResponse::ok("{\"status\":"),
            )
            .on(
                "PUT repos/acme/widgets/pulls/2/merge-async",
                FakeResponse::ok(r#"{"status":"weird"}"#),
            ),
    );
    let action = StackMergeAction::Direct(MergeMethod::Squash);
    assert!(matches!(
        github.submit_stack_merge(&repo(), 1, action, HEAD).await,
        Err(GitHubError::Malformed(_))
    ));
    let error = github
        .merge_stack(&repo(), 2, action, HEAD, Default::default())
        .await
        .expect_err("unknown status");
    assert_eq!(error.to_string(), "GitHub could not merge this stack.");
}

#[tokio::test]
async fn http_304_without_cache_is_malformed() {
    let github = GitHub::new(FakeGh::new().on(
        "GET repos/acme/widgets/pulls/811",
        FakeResponse::status(304),
    ));
    assert!(matches!(
        github.probe_pull_stack(&repo(), 811).await,
        Err(GitHubError::Malformed(_))
    ));
}

#[tokio::test]
async fn wrong_shape_in_mutation_payload_is_malformed() {
    let github = GitHub::new(FakeGh::new().on(
        "graphql ResolveThread",
        FakeResponse::ok(r#"{"data":{"resolveReviewThread":{"thread":{"isResolved":"yes"}}}}"#),
    ));
    assert!(matches!(
        github.set_thread_resolved("PRRT_1", true).await,
        Err(GitHubError::Malformed(_))
    ));
}
