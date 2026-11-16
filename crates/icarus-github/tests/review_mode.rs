use icarus_github::attachment::{attachment_markdown, content_type_for};
use icarus_github::diff_lines::Placement;
use icarus_github::review::{Reaction, Side, Verdict};
use icarus_github::{FakeGh, FakeResponse, GitHub, GitHubError};
use icarus_rules::facts::RepoRef;
use serde_json::json;

fn repo() -> RepoRef {
    RepoRef::new("acme", "widgets")
}

fn review(
    state: &str,
    body: &str,
    at: &str,
    login: &str,
    oid: &str,
    replies: &[bool],
) -> serde_json::Value {
    let comments: Vec<_> = replies
        .iter()
        .map(|reply| json!({ "replyTo": if *reply { json!({ "id": "X" }) } else { json!(null) } }))
        .collect();
    json!({ "state": state, "body": body, "submittedAt": at, "author": { "login": login },
            "commit": { "oid": oid }, "comments": { "nodes": comments } })
}

#[tokio::test]
async fn review_context_numbers_the_viewers_real_reviews() {
    let data = json!({ "data": {
        "viewer": { "login": "Me" },
        "repository": { "pullRequest": {
            "id": "PR_kw1", "number": 5, "title": "T", "url": "https://github.com/acme/widgets/pull/5",
            "state": "OPEN", "isDraft": false, "headRefName": "feat", "headRefOid": "b".repeat(40),
            "baseRefName": "main", "viewerDidAuthor": false, "author": { "login": "alice" },
            "viewerLatestReviewRequest": { "__typename": "ReviewRequest" },
            "viewerLatestReview": { "state": "COMMENTED", "commit": { "oid": "a".repeat(40) } },
            "commits": { "nodes": [ { "commit": { "oid": "a".repeat(40), "messageHeadline": "first", "committedDate": "2026-11-01T00:00:00Z" } } ] },
            "reviews": { "nodes": [
                review("COMMENTED", "", "2026-11-03T00:00:00Z", "me", &"c".repeat(40), &[true, true]),
                review("APPROVED", "", "2026-11-02T00:00:00Z", "me", &"a".repeat(40), &[]),
                review("COMMENTED", "looks off", "2026-11-01T00:00:00Z", "ME", &"a".repeat(40), &[false]),
                review("PENDING", "", "2026-11-04T00:00:00Z", "me", &"a".repeat(40), &[]),
                review("APPROVED", "", "2026-11-05T00:00:00Z", "bob", &"a".repeat(40), &[]),
            ] }
        } }
    } });
    let github =
        GitHub::new(FakeGh::new().on("graphql ReviewContext", FakeResponse::ok(data.to_string())));
    let context = github.review_context(&repo(), 5).await.expect("context");
    assert_eq!(context.pull_request_id, "PR_kw1");
    assert_eq!(context.viewer_login.as_deref(), Some("Me"));
    assert!(context.viewer_has_review_request);
    assert_eq!(context.viewer_latest_review_commit, Some("a".repeat(40)));
    assert_eq!(context.commits.len(), 1);
    let numbered: Vec<(usize, &str, &str)> = context
        .viewer_reviews
        .iter()
        .map(|r| (r.ordinal, r.state.as_str(), r.submitted_at.as_str()))
        .collect();
    assert_eq!(
        numbered,
        vec![
            (1, "COMMENTED", "2026-11-01T00:00:00Z"),
            (2, "APPROVED", "2026-11-02T00:00:00Z"),
        ],
        "reply-only, pending and other people's reviews are not numbered"
    );
    let request = github
        .runner()
        .request("graphql ReviewContext")
        .expect("request");
    assert_eq!(
        request.graphql_variables().expect("variables"),
        &json!({ "owner": "acme", "name": "widgets", "number": 5 })
    );
}

#[tokio::test]
async fn review_context_reports_a_missing_pull_request() {
    let github = GitHub::new(FakeGh::new().on(
        "graphql ReviewContext",
        FakeResponse::ok(json!({ "data": { "viewer": { "login": "me" }, "repository": { "pullRequest": null } } }).to_string()),
    ));
    let error = github
        .review_context(&repo(), 9)
        .await
        .expect_err("missing");
    assert_eq!(error.to_string(), "Could not find pull request #9.");
}

#[tokio::test]
async fn detailed_threads_carry_side_subject_ids_and_reactions() {
    let page = |nodes: serde_json::Value, next: Option<&str>| {
        json!({ "data": { "repository": { "pullRequest": { "reviewThreads": {
            "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next }, "nodes": nodes } } } } })
        .to_string()
    };
    let first = json!([{ "id": "PRRT_1", "isResolved": false, "isOutdated": false, "path": "src/a.rs",
        "line": 12, "startLine": 10, "originalLine": 12, "originalStartLine": 10, "diffSide": "RIGHT",
        "startDiffSide": "RIGHT", "subjectType": "LINE", "viewerCanResolve": true, "viewerCanReply": true,
        "comments": { "nodes": [ { "id": "PRRC_1", "databaseId": 101, "body": "Hm", "url": "u1",
            "viewerCanUpdate": true, "viewerCanDelete": true,
            "author": { "__typename": "User", "login": "me", "avatarUrl": "https://a/me" },
            "reactionGroups": [
                { "content": "HEART", "viewerHasReacted": false, "reactors": { "totalCount": 0, "nodes": [] } },
                { "content": "EYES", "viewerHasReacted": true, "reactors": { "totalCount": 4, "nodes": [ { "login": "me" }, { "login": "bob" }, {} ] } },
                { "content": "THUMBS_UP", "viewerHasReacted": false, "reactors": { "totalCount": 1, "nodes": [ { "login": "bob" } ] } }
            ] } ] } }]);
    let second = json!([{ "id": "PRRT_2", "isResolved": true, "isOutdated": true, "path": "b.md",
        "line": null, "originalLine": 3, "diffSide": "LEFT", "subjectType": "FILE",
        "comments": { "nodes": [ { "id": "PRRC_2", "body": "bot", "author": { "__typename": "Bot", "login": "ci" } } ] } }]);
    let github = GitHub::new(
        FakeGh::new()
            .on(
                "graphql PrReviewThreads",
                FakeResponse::ok(page(first, Some("c1"))),
            )
            .on(
                "graphql PrReviewThreads",
                FakeResponse::ok(page(second, None)),
            ),
    );
    let threads = github.pr_review_threads(&repo(), 5).await.expect("threads");
    assert_eq!(threads.len(), 2);
    let a = &threads[0];
    assert_eq!(
        (a.line, a.start_line, a.side.as_str(), a.subject.as_str()),
        (Some(12), Some(10), "RIGHT", "line")
    );
    let comment = &a.comments[0];
    assert_eq!(comment.database_id, Some(101));
    assert!(comment.viewer_can_update && !comment.is_bot);
    let reactions: Vec<(&str, u64, bool, usize)> = comment
        .reactions
        .iter()
        .map(|r| {
            (
                r.content.as_str(),
                r.count,
                r.viewer_has_reacted,
                r.reactors.len(),
            )
        })
        .collect();
    assert_eq!(
        reactions,
        vec![("+1", 1, false, 1), ("eyes", 4, true, 2)],
        "fixed order, zero counts dropped"
    );
    let b = &threads[1];
    assert_eq!(
        (b.line, b.original_line, b.side.as_str(), b.subject.as_str()),
        (None, Some(3), "LEFT", "file")
    );
    assert!(b.is_resolved && b.is_outdated && b.comments[0].is_bot);
    let requests = github.runner().requests();
    assert_eq!(
        requests[1].graphql_variables().expect("vars")["after"],
        "c1"
    );
}

#[tokio::test]
async fn replies_go_through_graphql_and_name_the_thread() {
    let github = GitHub::new(FakeGh::new().on(
        "graphql ReplyInThread",
        FakeResponse::ok(json!({ "data": { "addPullRequestReviewThreadReply": { "comment": { "id": "PRRC_9", "databaseId": 9, "url": "u" } } } }).to_string()),
    ));
    let reply = github
        .reply_in_thread("PRRT_1", "  thanks \n")
        .await
        .expect("reply");
    assert_eq!(reply.id, "PRRC_9");
    let request = github
        .runner()
        .request("graphql ReplyInThread")
        .expect("request");
    assert_eq!(
        request.graphql_variables().expect("vars"),
        &json!({ "threadId": "PRRT_1", "body": "thanks" })
    );
    let empty = github
        .reply_in_thread("PRRT_1", "  ")
        .await
        .expect_err("empty");
    assert_eq!(empty.to_string(), "A comment cannot be empty.");
}

#[tokio::test]
async fn graphql_refusals_come_back_verbatim() {
    let github = GitHub::new(
        FakeGh::new().on(
            "graphql SubmitReview",
            FakeResponse::ok(
                json!({ "data": { "addPullRequestReview": null }, "errors": [
            { "message": "Can not approve your own pull request" } ] })
                .to_string(),
            ),
        ),
    );
    let error = github
        .submit_review("PR_1", Verdict::Approve, "", &[])
        .await
        .expect_err("refused");
    assert!(matches!(error, GitHubError::GraphQl(_)));
    assert_eq!(error.to_string(), "Can not approve your own pull request");
}

#[tokio::test]
async fn pr_diff_pages_through_files_and_maps_patches() {
    let full: Vec<_> = (0..100)
        .map(|i| json!({ "filename": format!("f{i}.rs"), "patch": "@@ -1 +1 @@\n-a\n+b" }))
        .collect();
    let github = GitHub::new(
        FakeGh::new()
            .on(
                "GET repos/acme/widgets/pulls/5/files?per_page=100&page=1",
                FakeResponse::ok(json!(full).to_string()),
            )
            .on(
                "GET repos/acme/widgets/pulls/5/files?per_page=100&page=2",
                FakeResponse::ok(json!([{ "filename": "img.png" }]).to_string()),
            ),
    );
    let diff = github.pr_diff(&repo(), 5).await.expect("diff");
    assert_eq!(diff.files.len(), 101);
    assert_eq!(diff.place("f3.rs", Side::Right, 1, 1), Placement::InDiff);
    assert_eq!(
        diff.place("f3.rs", Side::Right, 2, 2),
        Placement::OutsideDiff
    );
    assert_eq!(diff.place("img.png", Side::Right, 1, 1), Placement::Unknown);
}

#[test]
fn names_parse_both_spellings() {
    assert_eq!(Reaction::parse("+1"), Some(Reaction::ThumbsUp));
    assert_eq!(Reaction::parse("THUMBS_DOWN"), Some(Reaction::ThumbsDown));
    assert_eq!(Reaction::parse("Hooray"), Some(Reaction::Hooray));
    assert_eq!(Reaction::parse("tada"), None);
    assert_eq!(
        Verdict::parse("request_changes"),
        Some(Verdict::RequestChanges)
    );
    assert_eq!(Verdict::parse("APPROVE"), Some(Verdict::Approve));
    assert_eq!(Verdict::parse("only comment"), None);
    assert_eq!(Side::parse("left"), Some(Side::Left));
}

#[test]
fn attachments_become_image_links_or_bare_video_urls() {
    assert_eq!(content_type_for("Shot.PNG"), Some("image/png"));
    assert_eq!(content_type_for("clip.mov"), Some("video/quicktime"));
    assert_eq!(content_type_for("notes.txt"), None);
    assert_eq!(
        attachment_markdown(
            "a [b].png",
            "image/png",
            "https://github.com/user-attachments/assets/1"
        ),
        "![a b.png](https://github.com/user-attachments/assets/1)"
    );
    assert_eq!(
        attachment_markdown(
            "c.mp4",
            "video/mp4",
            "https://github.com/user-attachments/assets/2"
        ),
        "\nhttps://github.com/user-attachments/assets/2\n"
    );
}

#[tokio::test]
async fn reviewer_suggestions_put_githubs_own_first_then_frequent_reviewers() {
    let reviewer = |kind: &str, login: &str| json!({ "author": { "__typename": kind, "login": login, "avatarUrl": "https://a" } });
    let data = json!({ "data": { "repository": {
        "pullRequest": {
            "author": { "login": "alice" },
            "suggestedReviewers": [ { "reviewer": { "login": "carol", "name": "Carol" } } ],
        },
        "pullRequests": { "nodes": [
            { "reviews": { "nodes": [ reviewer("User", "dave"), reviewer("User", "erin"), reviewer("Bot", "copilot") ] } },
            { "reviews": { "nodes": [ reviewer("User", "erin"), reviewer("User", "Alice"), reviewer("User", "carol") ] } },
        ] },
    } } });
    let github = GitHub::new(FakeGh::new().on(
        "graphql ReviewerSuggestions",
        FakeResponse::ok(data.to_string()),
    ));
    let users = github
        .reviewer_suggestions(&repo(), 5)
        .await
        .expect("suggestions");
    let logins: Vec<&str> = users.iter().map(|u| u.login.as_str()).collect();
    assert_eq!(logins, ["carol", "erin", "dave"]);
    assert_eq!(users[0].name.as_deref(), Some("Carol"));
}
