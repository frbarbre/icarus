use icarus_github::merge::RerunScope;
use icarus_github::review::{CommentRef, CommentTarget, DraftComment, Reaction, Side, Verdict};
use icarus_github::stack::{PollPolicy, StackMergeAction, StackMergeStatus};
use icarus_github::{Body, FakeGh, FakeResponse, GhRequest, GitHub, GitHubError, Method};
use icarus_rules::facts::{MergeMethod, MergeQueueEntryState, RepoRef, RequestedReviewer};
use serde_json::{Value, json};
use std::time::Duration;

const HEAD: &str = "1111111111111111111111111111111111111111";

fn repo() -> RepoRef {
    RepoRef::new("acme", "widgets")
}

fn client(fake: FakeGh) -> GitHub<FakeGh> {
    GitHub::new(fake)
}

fn only_request(github: &GitHub<FakeGh>) -> GhRequest {
    let requests = github.runner().requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    requests[0].clone()
}

fn json_body(request: &GhRequest) -> Value {
    request.json_body().cloned().expect("json body")
}

fn instant() -> PollPolicy {
    PollPolicy {
        interval: Duration::ZERO,
        max_polls: 3,
    }
}

#[test]
fn request_args_go_through_gh_api() {
    let request = GhRequest::new(Method::Put, "repos/acme/widgets/pulls/1/merge-async")
        .header("X-GitHub-Api-Version", "2026-03-10")
        .json(json!({"merge_action": "merge_queue"}));
    assert_eq!(
        request.args(),
        vec![
            "api",
            "--include",
            "--method",
            "PUT",
            "repos/acme/widgets/pulls/1/merge-async",
            "--header",
            "X-GitHub-Api-Version: 2026-03-10",
            "--header",
            "Content-Type: application/json",
            "--input",
            "-",
        ]
    );
    assert_eq!(
        request.stdin().expect("stdin"),
        br#"{"merge_action":"merge_queue"}"#.to_vec()
    );
    let get = GhRequest::get("rate_limit");
    assert_eq!(
        get.args(),
        vec!["api", "--include", "--method", "GET", "rate_limit"]
    );
    assert_eq!(get.stdin(), None);
}

#[test]
fn graphql_route_uses_operation_name() {
    let request = GhRequest::graphql("mutation SubmitReview($input: X!) { a }", json!({}));
    assert_eq!(request.route(), "graphql SubmitReview");
    let anonymous = GhRequest::graphql("{ viewer { login } }", json!({}));
    assert_eq!(anonymous.route(), "graphql anonymous");
}

#[tokio::test]
async fn merge_pull_request_puts_method_and_head() {
    let github = client(FakeGh::new().on(
        "PUT repos/acme/widgets/pulls/811/merge",
        FakeResponse::ok(r#"{"sha":"9999999999999999999999999999999999999999","merged":true,"message":"Pull Request successfully merged"}"#),
    ));
    let result = github
        .merge_pull_request(&repo(), 811, MergeMethod::Squash, HEAD)
        .await
        .expect("merge");
    assert!(result.merged);
    assert_eq!(
        json_body(&only_request(&github)),
        json!({"merge_method": "squash", "sha": HEAD})
    );
}

#[tokio::test]
async fn merge_pull_request_reports_refusal() {
    let github = client(FakeGh::new().on(
        "PUT repos/acme/widgets/pulls/811/merge",
        FakeResponse::json(405, r#"{"message":"Pull Request is not mergeable"}"#),
    ));
    let error = github
        .merge_pull_request(&repo(), 811, MergeMethod::Merge, HEAD)
        .await
        .expect_err("refused");
    assert_eq!(
        error.to_string(),
        "GitHub returned HTTP 405: Pull Request is not mergeable"
    );
    assert!(matches!(
        github
            .merge_pull_request(&repo(), 811, MergeMethod::Merge, "main")
            .await,
        Err(GitHubError::Invalid(_))
    ));
}

#[tokio::test]
async fn auto_merge_enable_and_disable() {
    let github = client(
        FakeGh::new()
            .on(
                "graphql EnableAutoMerge",
                FakeResponse::ok(r#"{"data":{"enablePullRequestAutoMerge":{"pullRequest":{"autoMergeRequest":{"mergeMethod":"REBASE"}}}}}"#),
            )
            .on(
                "graphql DisableAutoMerge",
                FakeResponse::ok(r#"{"data":{"disablePullRequestAutoMerge":{"pullRequest":{"autoMergeRequest":null}}}}"#),
            ),
    );
    github
        .enable_auto_merge("PR_kwDOAbc123", MergeMethod::Rebase, Some(HEAD))
        .await
        .expect("enable");
    github
        .disable_auto_merge("PR_kwDOAbc123")
        .await
        .expect("disable");
    let requests = github.runner().requests();
    assert_eq!(
        requests[0].graphql_variables().expect("variables"),
        &json!({"pullRequestId": "PR_kwDOAbc123", "mergeMethod": "REBASE", "expectedHeadOid": HEAD})
    );
    assert_eq!(
        requests[1].graphql_variables().expect("variables"),
        &json!({"pullRequestId": "PR_kwDOAbc123"})
    );
}

#[tokio::test]
async fn auto_merge_on_clean_pr_explains() {
    let github = client(FakeGh::new().on(
        "graphql EnableAutoMerge",
        FakeResponse::ok(r#"{"data":{"enablePullRequestAutoMerge":null},"errors":[{"type":"UNPROCESSABLE","message":"Pull request Pull request is in clean status"}]}"#),
    ));
    let error = github
        .enable_auto_merge("PR_kwDOAbc123", MergeMethod::Squash, None)
        .await
        .expect_err("clean");
    assert!(matches!(error, GitHubError::AlreadyMergeable));
}

#[tokio::test]
async fn enqueue_and_dequeue() {
    let github = client(
        FakeGh::new()
            .on(
                "graphql Enqueue",
                FakeResponse::ok(r#"{"data":{"enqueuePullRequest":{"mergeQueueEntry":{"state":"QUEUED","position":2,"estimatedTimeToMerge":null}}}}"#),
            )
            .on(
                "graphql Dequeue",
                FakeResponse::ok(r#"{"data":{"dequeuePullRequest":{"mergeQueueEntry":{"state":"QUEUED"}}}}"#),
            ),
    );
    let entry = github
        .enqueue("PR_kwDOAbc123", Some(HEAD), true)
        .await
        .expect("enqueue")
        .expect("entry");
    assert_eq!(entry.state, MergeQueueEntryState::Queued);
    assert_eq!(entry.position, Some(2));
    github.dequeue("PR_kwDOAbc123").await.expect("dequeue");
    let requests = github.runner().requests();
    assert_eq!(
        requests[0].graphql_variables().expect("variables"),
        &json!({"pullRequestId": "PR_kwDOAbc123", "expectedHeadOid": HEAD, "jump": true})
    );
    assert_eq!(
        requests[1].graphql_variables().expect("variables"),
        &json!({"id": "PR_kwDOAbc123"})
    );
}

#[tokio::test]
async fn mark_ready_for_review() {
    let github = client(FakeGh::new().on(
        "graphql MarkReady",
        FakeResponse::ok(
            r#"{"data":{"markPullRequestReadyForReview":{"pullRequest":{"isDraft":false}}}}"#,
        ),
    ));
    github
        .mark_ready_for_review("PR_kwDOAbc123")
        .await
        .expect("ready");
    assert_eq!(
        only_request(&github)
            .graphql_variables()
            .expect("variables"),
        &json!({"pullRequestId": "PR_kwDOAbc123"})
    );
}

#[tokio::test]
async fn stack_merge_direct_polls_until_merged() {
    let route = "PUT repos/acme/widgets/pulls/812/merge-async";
    let poll = "GET repos/acme/widgets/pulls/812/merge-async/5f0c9a2e-1d2b-4c3d-9e8f-0a1b2c3d4e5f";
    let github = client(
        FakeGh::new()
            .on(
                route,
                FakeResponse::json(202, r#"{"status":"pending","details":{"uuid":"5f0c9a2e-1d2b-4c3d-9e8f-0a1b2c3d4e5f"}}"#),
            )
            .on(poll, FakeResponse::json(502, "Bad Gateway"))
            .on(
                poll,
                FakeResponse::ok(r#"{"status":"pending","details":{"uuid":"5f0c9a2e-1d2b-4c3d-9e8f-0a1b2c3d4e5f"}}"#),
            )
            .on(poll, FakeResponse::ok(r#"{"status":"merged"}"#)),
    );
    let status = github
        .merge_stack(
            &repo(),
            812,
            StackMergeAction::Direct(MergeMethod::Squash),
            HEAD,
            instant(),
        )
        .await
        .expect("merge");
    assert_eq!(status, StackMergeStatus::Merged);
    let submit = github.runner().request(route).expect("submit");
    assert_eq!(
        json_body(&submit),
        json!({"merge_action": "direct_merge", "merge_method": "squash", "sha": HEAD})
    );
    assert!(
        submit
            .headers
            .contains(&("X-GitHub-Api-Version".to_owned(), "2026-03-10".to_owned()))
    );
    assert_eq!(github.runner().count(poll), 3);
}

#[tokio::test]
async fn stack_merge_queue_omits_method_and_reuses_pending_uuid() {
    let github = client(
        FakeGh::new()
            .on(
                "PUT repos/acme/widgets/pulls/812/merge-async",
                FakeResponse::json(409, r#"{"status":"pending","details":{"uuid":"abc"}}"#),
            )
            .on(
                "GET repos/acme/widgets/pulls/812/merge-async/abc",
                FakeResponse::ok(r#"{"status":"enqueued"}"#),
            ),
    );
    let status = github
        .merge_stack(&repo(), 812, StackMergeAction::Queue, HEAD, instant())
        .await
        .expect("queue");
    assert_eq!(status, StackMergeStatus::Enqueued);
    let submit = &github.runner().requests()[0];
    assert_eq!(
        json_body(submit),
        json!({"merge_action": "merge_queue", "sha": HEAD})
    );
}

#[tokio::test]
async fn stack_merge_failures() {
    let github = client(
        FakeGh::new()
            .on(
                "PUT repos/acme/widgets/pulls/812/merge-async",
                FakeResponse::json(
                    422,
                    r##"{"status":"failed","details":{"message":"#811 has merge conflicts."}}"##,
                ),
            )
            .on(
                "PUT repos/acme/widgets/pulls/9/merge-async",
                FakeResponse::json(404, r#"{"message":"Not Found"}"#),
            )
            .on(
                "PUT repos/acme/widgets/pulls/10/merge-async",
                FakeResponse::json(202, r#"{"status":"pending","details":{"uuid":"slow"}}"#),
            )
            .on(
                "GET repos/acme/widgets/pulls/10/merge-async/slow",
                FakeResponse::ok(r#"{"status":"pending","details":{"uuid":"slow"}}"#),
            ),
    );
    let action = StackMergeAction::Direct(MergeMethod::Merge);
    let error = github
        .merge_stack(&repo(), 812, action, HEAD, instant())
        .await
        .expect_err("conflict");
    assert_eq!(error.to_string(), "#811 has merge conflicts.");
    assert!(matches!(
        github
            .merge_stack(&repo(), 9, action, HEAD, instant())
            .await,
        Err(GitHubError::StacksUnavailable)
    ));
    assert_eq!(
        github
            .merge_stack(&repo(), 10, action, HEAD, instant())
            .await
            .expect("timeout"),
        StackMergeStatus::Pending {
            uuid: "slow".to_owned()
        }
    );
    assert!(matches!(
        github.submit_stack_merge(&repo(), 10, action, "").await,
        Err(GitHubError::StackMetadataUnavailable)
    ));
}

#[tokio::test]
async fn stack_create_and_add() {
    let github = client(
        FakeGh::new()
            .on(
                "POST repos/acme/widgets/stacks",
                FakeResponse::json(
                    201,
                    r#"{"number":43,"open":true,"pull_requests":[{"number":810},{"number":811}]}"#,
                ),
            )
            .on(
                "POST repos/acme/widgets/stacks/43/add",
                FakeResponse::json(404, r#"{"message":"Not Found"}"#),
            ),
    );
    let record = github
        .create_stack(&repo(), &[810, 811])
        .await
        .expect("create");
    assert_eq!(record.number, 43);
    assert_eq!(
        json_body(&github.runner().requests()[0]),
        json!({"pull_requests": [810, 811]})
    );
    assert!(matches!(
        github.add_to_stack(&repo(), 43, &[812]).await,
        Err(GitHubError::StacksUnavailable)
    ));
}

#[tokio::test]
async fn reviewers_split_users_and_team_slugs() {
    let route = "POST repos/acme/widgets/pulls/811/requested_reviewers";
    let github = client(
        FakeGh::new()
            .on(route, FakeResponse::json(201, r#"{"number":811}"#))
            .on(
                "DELETE repos/acme/widgets/pulls/811/requested_reviewers",
                FakeResponse::ok(r#"{"number":811}"#),
            ),
    );
    github
        .request_reviewers(
            &repo(),
            811,
            &[
                RequestedReviewer::user("hubot"),
                RequestedReviewer::team("acme/pm"),
            ],
        )
        .await
        .expect("request");
    github
        .remove_reviewers(&repo(), 811, &[RequestedReviewer::user("monalisa")])
        .await
        .expect("remove");
    let requests = github.runner().requests();
    assert_eq!(
        json_body(&requests[0]),
        json!({"reviewers": ["hubot"], "team_reviewers": ["pm"]})
    );
    assert_eq!(requests[1].method, Method::Delete);
    assert_eq!(
        json_body(&requests[1]),
        json!({"reviewers": ["monalisa"], "team_reviewers": []})
    );
    assert!(matches!(
        github.request_reviewers(&repo(), 811, &[]).await,
        Err(GitHubError::Invalid(_))
    ));
}

#[tokio::test]
async fn submit_review_sends_verdict_and_threads_as_variables() {
    let github = client(FakeGh::new().on(
        "graphql SubmitReview",
        FakeResponse::ok(r#"{"data":{"addPullRequestReview":{"pullRequestReview":{"id":"PRR_kwDO1","state":"CHANGES_REQUESTED","url":"https://github.com/acme/widgets/pull/811#pullrequestreview-1"}}}}"#),
    ));
    let comments = vec![
        DraftComment {
            path: "src/a.rs".to_owned(),
            body: "Use \"quotes\" here\n}".to_owned(),
            target: CommentTarget::Line {
                line: 12,
                side: Side::Right,
                start: Some((12, Side::Right)),
            },
        },
        DraftComment {
            path: "src/a.rs".to_owned(),
            body: "Range".to_owned(),
            target: CommentTarget::Line {
                line: 20,
                side: Side::Right,
                start: Some((15, Side::Left)),
            },
        },
        DraftComment {
            path: "src/b.rs".to_owned(),
            body: "Line 3:\n```\nlet x = 1;\n```\n\nWhy?".to_owned(),
            target: CommentTarget::File,
        },
    ];
    let review = github
        .submit_review(
            "PR_kwDOAbc123",
            Verdict::RequestChanges,
            "Needs work",
            &comments,
        )
        .await
        .expect("submit");
    assert_eq!(review.state, "CHANGES_REQUESTED");
    let request = only_request(&github);
    assert_eq!(
        request.graphql_variables().expect("variables"),
        &json!({"input": {
            "pullRequestId": "PR_kwDOAbc123",
            "event": "REQUEST_CHANGES",
            "body": "Needs work",
            "threads": [
                {"path": "src/a.rs", "body": "Use \"quotes\" here\n}", "line": 12, "side": "RIGHT"},
                {"path": "src/a.rs", "body": "Range", "line": 20, "side": "RIGHT", "startLine": 15, "startSide": "LEFT"},
                {"path": "src/b.rs", "body": "Line 3:\n```\nlet x = 1;\n```\n\nWhy?", "subjectType": "FILE"}
            ]
        }})
    );
}

#[tokio::test]
async fn submit_review_without_drafts_omits_threads_and_surfaces_errors() {
    let github = client(
        FakeGh::new()
            .on(
                "graphql SubmitReview",
                FakeResponse::ok(r#"{"data":{"addPullRequestReview":{"pullRequestReview":{"id":"PRR_1","state":"APPROVED","url":null}}}}"#),
            ),
    );
    github
        .submit_review("PR_kwDOAbc123", Verdict::Approve, "", &[])
        .await
        .expect("approve");
    let variables = only_request(&github)
        .graphql_variables()
        .cloned()
        .expect("variables");
    assert_eq!(
        variables,
        json!({"input": {"pullRequestId": "PR_kwDOAbc123", "event": "APPROVE", "body": ""}})
    );

    let failing = client(FakeGh::new().on(
        "graphql SubmitReview",
        FakeResponse::ok(r#"{"data":{"addPullRequestReview":null},"errors":[{"message":"Line could not be resolved"},{"message":"Path is invalid"}]}"#),
    ));
    let error = failing
        .submit_review("PR_kwDOAbc123", Verdict::Comment, "x", &[])
        .await
        .expect_err("refused");
    assert_eq!(
        error.to_string(),
        "Line could not be resolved; Path is invalid"
    );
}

#[tokio::test]
async fn post_review_comment_now_line_and_file() {
    let route = "POST repos/acme/widgets/pulls/811/comments";
    let github = client(FakeGh::new().on(
        route,
        FakeResponse::json(201, r#"{"id":5001,"node_id":"PRRC_kwDO1","html_url":"https://github.com/acme/widgets/pull/811#discussion_r5001"}"#),
    ));
    let posted = github
        .post_review_comment(
            &repo(),
            811,
            HEAD,
            &DraftComment {
                path: "src/a.rs".to_owned(),
                body: "nit".to_owned(),
                target: CommentTarget::Line {
                    line: 9,
                    side: Side::Right,
                    start: Some((7, Side::Right)),
                },
            },
        )
        .await
        .expect("line");
    assert_eq!(posted.id, 5001);
    github
        .post_review_comment(
            &repo(),
            811,
            HEAD,
            &DraftComment {
                path: "README.md".to_owned(),
                body: "whole file".to_owned(),
                target: CommentTarget::File,
            },
        )
        .await
        .expect("file");
    let requests = github.runner().requests();
    assert_eq!(
        json_body(&requests[0]),
        json!({"body": "nit", "commit_id": HEAD, "path": "src/a.rs", "line": 9, "side": "RIGHT", "start_line": 7, "start_side": "RIGHT"})
    );
    assert_eq!(
        json_body(&requests[1]),
        json!({"body": "whole file", "commit_id": HEAD, "path": "README.md", "subject_type": "file"})
    );
}

#[tokio::test]
async fn reply_and_conversation_comment() {
    let github = client(
        FakeGh::new()
            .on(
                "POST repos/acme/widgets/pulls/811/comments/5001/replies",
                FakeResponse::json(201, r#"{"id":5002}"#),
            )
            .on(
                "POST repos/acme/widgets/issues/811/comments",
                FakeResponse::json(201, r#"{"id":0}"#),
            ),
    );
    let reply = github
        .reply_to_thread(&repo(), 811, 5001, "  Done.  ")
        .await
        .expect("reply");
    assert_eq!(reply.id, 5002);
    assert_eq!(
        json_body(&github.runner().requests()[0]),
        json!({"body": "Done."})
    );
    assert!(matches!(
        github.add_issue_comment(&repo(), 811, "hi").await,
        Err(GitHubError::Malformed(_))
    ));
    assert!(matches!(
        github.reply_to_thread(&repo(), 811, 5001, "   ").await,
        Err(GitHubError::Invalid(_))
    ));
}

#[tokio::test]
async fn resolve_and_unresolve_threads() {
    let github = client(
        FakeGh::new()
            .on(
                "graphql ResolveThread",
                FakeResponse::ok(
                    r#"{"data":{"resolveReviewThread":{"thread":{"isResolved":true}}}}"#,
                ),
            )
            .on(
                "graphql UnresolveThread",
                FakeResponse::ok(
                    r#"{"data":{"unresolveReviewThread":{"thread":{"isResolved":false}}}}"#,
                ),
            ),
    );
    assert!(
        github
            .set_thread_resolved("PRRT_kwDO1", true)
            .await
            .expect("resolve")
    );
    assert!(
        !github
            .set_thread_resolved("PRRT_kwDO1", false)
            .await
            .expect("unresolve")
    );
    for request in github.runner().requests() {
        assert_eq!(
            request.graphql_variables().expect("variables"),
            &json!({"threadId": "PRRT_kwDO1"})
        );
    }
}

#[tokio::test]
async fn edit_and_delete_comments_by_kind() {
    let github = client(
        FakeGh::new()
            .on("graphql UpdateReviewComment", FakeResponse::ok(r#"{"data":{"updatePullRequestReviewComment":{"pullRequestReviewComment":{"body":"b","lastEditedAt":"2026-11-15T12:00:00Z"}}}}"#))
            .on("graphql UpdateReview", FakeResponse::ok(r#"{"data":{"updatePullRequestReview":{"pullRequestReview":{"body":"b","lastEditedAt":null}}}}"#))
            .on("PATCH repos/acme/widgets/issues/comments/77", FakeResponse::ok(r#"{"id":77}"#))
            .on("graphql DeleteReviewComment", FakeResponse::ok(r#"{"data":{"deletePullRequestReviewComment":{"clientMutationId":null}}}"#))
            .on("DELETE repos/acme/widgets/issues/comments/77", FakeResponse::status(204)),
    );
    let review_comment = CommentRef::ReviewComment {
        node_id: "PRRC_kwDO1".to_owned(),
    };
    let summary = CommentRef::ReviewSummary {
        node_id: "PRR_kwDO1".to_owned(),
    };
    let issue = CommentRef::IssueComment { id: 77 };
    github
        .edit_comment(&repo(), &review_comment, " b ")
        .await
        .expect("edit inline");
    github
        .edit_comment(&repo(), &summary, "b")
        .await
        .expect("edit summary");
    github
        .edit_comment(&repo(), &issue, "b")
        .await
        .expect("edit issue");
    github
        .delete_comment(&repo(), &review_comment)
        .await
        .expect("delete inline");
    github
        .delete_comment(&repo(), &issue)
        .await
        .expect("delete issue");
    assert!(matches!(
        github.delete_comment(&repo(), &summary).await,
        Err(GitHubError::Invalid(_))
    ));
    assert!(matches!(
        github.edit_comment(&repo(), &issue, "\n").await,
        Err(GitHubError::Invalid(_))
    ));
    let requests = github.runner().requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(
        requests[0].graphql_variables().expect("variables"),
        &json!({"id": "PRRC_kwDO1", "body": "b"})
    );
    assert_eq!(
        requests[1].graphql_variables().expect("variables"),
        &json!({"id": "PRR_kwDO1", "body": "b"})
    );
    assert_eq!(json_body(&requests[2]), json!({"body": "b"}));
    assert_eq!(
        requests[3].graphql_variables().expect("variables"),
        &json!({"id": "PRRC_kwDO1"})
    );
    assert_eq!(requests[4].method, Method::Delete);
}

#[tokio::test]
async fn reactions_add_and_remove() {
    let github = client(
        FakeGh::new()
            .on(
                "graphql AddReaction",
                FakeResponse::ok(r#"{"data":{"addReaction":{"subject":{"id":"IC_1"}}}}"#),
            )
            .on(
                "graphql RemoveReaction",
                FakeResponse::ok(r#"{"data":{"removeReaction":{"subject":{"id":"IC_1"}}}}"#),
            ),
    );
    github
        .set_reaction("IC_1", Reaction::Rocket, true)
        .await
        .expect("add");
    github
        .set_reaction("IC_1", Reaction::ThumbsUp, false)
        .await
        .expect("remove");
    let requests = github.runner().requests();
    assert_eq!(requests[0].route(), "graphql AddReaction");
    assert_eq!(
        requests[0].graphql_variables().expect("variables"),
        &json!({"subjectId": "IC_1", "content": "ROCKET"})
    );
    assert_eq!(requests[1].route(), "graphql RemoveReaction");
    assert_eq!(
        requests[1].graphql_variables().expect("variables")["content"],
        "THUMBS_UP"
    );
}

#[tokio::test]
async fn rerun_failed_and_all_checks() {
    let list = format!("GET repos/acme/widgets/actions/runs?head_sha={HEAD}&per_page=100");
    let fake = || {
        FakeGh::new()
            .on(
                list.clone(),
                FakeResponse::ok(include_str!("fixtures/workflow_runs.json")),
            )
            .on(
                "POST repos/acme/widgets/actions/runs/101/rerun-failed-jobs",
                FakeResponse::json(201, "{}"),
            )
            .on(
                "POST repos/acme/widgets/actions/runs/104/rerun-failed-jobs",
                FakeResponse::json(201, "{}"),
            )
            .on(
                "POST repos/acme/widgets/actions/runs/101/rerun",
                FakeResponse::json(201, "{}"),
            )
            .on(
                "POST repos/acme/widgets/actions/runs/102/rerun",
                FakeResponse::json(201, "{}"),
            )
            .on(
                "POST repos/acme/widgets/actions/runs/104/rerun",
                FakeResponse::json(201, "{}"),
            )
    };
    let failed = client(fake());
    assert_eq!(
        failed
            .rerun_checks(&repo(), HEAD, RerunScope::Failed)
            .await
            .expect("failed"),
        vec![101, 104]
    );
    let all = client(fake());
    assert_eq!(
        all.rerun_checks(&repo(), HEAD, RerunScope::All)
            .await
            .expect("all"),
        vec![101, 102, 104]
    );
    let check = client(FakeGh::new().on(
        "POST repos/acme/widgets/check-runs/55/rerequest",
        FakeResponse::json(201, "{}"),
    ));
    check
        .rerequest_check_run(&repo(), 55)
        .await
        .expect("rerequest");
}

#[tokio::test]
async fn upload_attachment_posts_raw_bytes_to_uploads_host() {
    let upload = "POST https://uploads.github.com/user-attachments/assets?name=screen%20shot.png&content_type=image%2Fpng&repository_id=1296269";
    let github = client(
        FakeGh::new()
            .on(
                "GET repos/acme/widgets",
                FakeResponse::ok(r#"{"id":1296269,"full_name":"acme/widgets","permissions":{"admin":false,"push":true,"pull":true}}"#),
            )
            .on(
                upload,
                FakeResponse::json(201, r#"{"url":"https://github.com/user-attachments/assets/0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0"}"#),
            ),
    );
    let url = github
        .upload_attachment(
            &repo(),
            "screen shot.png",
            "image/png",
            vec![0x89, b'P', b'N', b'G'],
        )
        .await
        .expect("upload");
    assert_eq!(
        url,
        "https://github.com/user-attachments/assets/0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0"
    );
    let request = github.runner().request(upload).expect("upload request");
    assert_eq!(
        request.body,
        Some(Body::Raw {
            bytes: vec![0x89, b'P', b'N', b'G'],
            content_type: "image/png".to_owned()
        })
    );
    assert!(
        request
            .args()
            .contains(&"Content-Type: image/png".to_owned())
    );
}

#[tokio::test]
async fn attachments_are_fetched_through_the_signed_in_gh_and_only_from_github() {
    let url = "https://github.com/user-attachments/assets/0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0";
    let github = client(
        FakeGh::new().on(
            format!("GET {url}"),
            FakeResponse::status(200)
                .header("Content-Type", "image/png; charset=binary")
                .header("Content-Length", "4"),
        ),
    );
    let asset = github.fetch_asset(url).await.expect("fetched");
    assert_eq!(asset.content_type, "image/png");
    let request = only_request(&github);
    assert_eq!(
        request.endpoint, url,
        "gh signs the github.com request itself"
    );
    assert!(request.args().contains(&"Accept: */*".to_owned()));

    for refused in [
        "http://github.com/user-attachments/assets/x",
        "https://evil.example/user-attachments/assets/x",
        "https://github.com/acme/widgets/settings",
        "https://github.com@evil.example/user-attachments/assets/x",
    ] {
        assert!(github.fetch_asset(refused).await.is_err(), "{refused}");
    }
    assert_eq!(
        github.runner().requests().len(),
        1,
        "no request leaves for a host that isn't GitHub's"
    );
    assert!(icarus_github::attachment::is_github_asset_url(
        "https://private-user-images.githubusercontent.com/1/2.png?jwt=abc"
    ));
}

#[tokio::test]
async fn upload_attachment_refusals() {
    let github = client(FakeGh::new().on(
        "GET repos/acme/widgets",
        FakeResponse::ok(r#"{"id":1296269,"permissions":{"push":false}}"#),
    ));
    let error = github
        .upload_attachment(&repo(), "a.png", "image/png", vec![1])
        .await
        .expect_err("no push");
    assert_eq!(
        error.to_string(),
        "Attaching files needs write access to acme/widgets."
    );
    assert_eq!(
        github
            .upload_attachment(&repo(), "a.pdf", "application/pdf", vec![1])
            .await
            .expect_err("type")
            .to_string(),
        "Only images and videos can be attached."
    );
    assert_eq!(
        github
            .upload_attachment(&repo(), "a.png", "image/png", vec![0; 10 * 1024 * 1024 + 1])
            .await
            .expect_err("size")
            .to_string(),
        "GitHub takes images up to 10 MB."
    );
    assert_eq!(github.runner().requests().len(), 1);
}
