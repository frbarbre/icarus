use icarus_github::conversation::ConversationKind;
use icarus_github::{FakeGh, FakeResponse, GitHub};
use icarus_rules::facts::RepoRef;
use serde_json::{Value, json};

const ROUTE: &str = "graphql PrConversation";

fn repo() -> RepoRef {
    RepoRef::new("acme", "widgets")
}

fn comment(id: u64, login: &str, kind: &str, at: &str) -> Value {
    json!({ "id": format!("IC_{id}"), "databaseId": id, "body": format!("comment {id}"),
        "url": format!("https://github.com/acme/widgets/pull/7#issuecomment-{id}"),
        "createdAt": at, "viewerCanUpdate": login == "me", "viewerCanDelete": login == "me",
        "author": { "__typename": kind, "login": login, "avatarUrl": "https://a/x" },
        "reactionGroups": [
            { "content": "THUMBS_UP", "viewerHasReacted": true,
              "reactors": { "totalCount": 2, "nodes": [{ "login": "me" }, { "login": "bob" }] } },
            { "content": "HEART", "viewerHasReacted": false, "reactors": { "totalCount": 0, "nodes": [] } },
        ] })
}

fn review(id: u64, state: &str, body: &str, at: &str) -> Value {
    json!({ "id": format!("PRR_{id}"), "databaseId": id, "body": body, "state": state,
        "submittedAt": at, "viewerCanUpdate": true,
        "author": { "__typename": "User", "login": "rev" } })
}

fn page(
    comments: Option<(Vec<Value>, Option<&str>)>,
    reviews: Option<(Vec<Value>, Option<&str>)>,
) -> FakeResponse {
    let connection = |(nodes, cursor): (Vec<Value>, Option<&str>)| json!({ "pageInfo": { "hasNextPage": cursor.is_some(), "endCursor": cursor }, "nodes": nodes });
    let mut pull = json!({});
    if let Some(comments) = comments {
        pull["comments"] = connection(comments);
    }
    if let Some(reviews) = reviews {
        pull["reviews"] = connection(reviews);
    }
    FakeResponse::ok(json!({ "data": { "repository": { "pullRequest": pull } } }).to_string())
}

#[tokio::test]
async fn conversation_pages_both_lists_with_the_viewers_reactions() {
    let first: Vec<Value> = (1..=100)
        .map(|id| comment(id, "farm-bot", "Bot", "2026-11-10T10:00:00Z"))
        .collect();
    let fake = FakeGh::new()
        .on(
            ROUTE,
            page(
                Some((first, Some("c1"))),
                Some((
                    vec![
                        review(1, "APPROVED", "", "2026-11-11T09:00:00Z"),
                        review(2, "CHANGES_REQUESTED", "Please fix", "2026-11-11T10:00:00Z"),
                        review(3, "COMMENTED", "  ", "2026-11-11T11:00:00Z"),
                        review(4, "APPROVED", "Ship it", "2026-11-13T10:00:00Z"),
                    ],
                    None,
                )),
            ),
        )
        .on(
            ROUTE,
            page(
                Some((
                    vec![comment(101, "me", "User", "2026-11-12T10:00:00Z")],
                    None,
                )),
                None,
            ),
        );
    let github = GitHub::new(fake);
    let entries = github
        .pr_conversation(&repo(), 7)
        .await
        .expect("conversation");

    assert_eq!(
        entries.len(),
        103,
        "100 + 1 comments, 2 reviews with a summary"
    );
    let requests = github.runner().requests();
    let variables: Vec<&Value> = requests
        .iter()
        .filter_map(|request| request.graphql_variables())
        .collect();
    assert_eq!(variables.len(), 2);
    assert_eq!(variables[1]["commentsAfter"], "c1");
    assert_eq!(variables[1]["withReviews"], false, "reviews were done");

    let first = &entries[0];
    assert_eq!(first.kind, ConversationKind::Comment);
    assert_eq!(first.author.as_deref(), Some("farm-bot"));
    assert!(first.is_bot);
    assert_eq!((first.id.as_str(), first.database_id), ("IC_1", Some(1)));
    assert_eq!(first.reactions.len(), 1, "empty groups are dropped");
    assert_eq!(first.reactions[0].content, "+1");
    assert_eq!(first.reactions[0].count, 2);
    assert!(first.reactions[0].viewer_has_reacted);

    let tail: Vec<(&str, Option<&str>)> = entries[100..]
        .iter()
        .map(|e| (e.id.as_str(), e.review_state.as_deref()))
        .collect();
    assert_eq!(
        tail,
        [
            ("PRR_2", Some("CHANGES_REQUESTED")),
            ("IC_101", None),
            ("PRR_4", Some("APPROVED")),
        ],
        "sorted by when each was posted or submitted"
    );
    assert!(entries[101].viewer_can_update && entries[101].viewer_can_delete);
    assert!(!entries[101].is_bot);
}
