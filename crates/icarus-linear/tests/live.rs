//! Read-only checks against the real Linear API.
//!
//! Ignored by default. Run with a command that prints the key (it is never printed or stored), e.g.:
//! `ICARUS_LINEAR_LIVE_KEY_COMMAND='bw get notes <item-id>' cargo test -p icarus-linear --test live -- --ignored`
//! It only sends queries; mutations are refused by the client.

use icarus_linear::{HttpTransport, KeyCommand, KeyResolver, KeyStatus, Linear};
use serde_json::json;

async fn live() -> Option<Linear<HttpTransport>> {
    let command = std::env::var("ICARUS_LINEAR_LIVE_KEY_COMMAND").ok()?;
    let keys = KeyResolver::new(None);
    keys.set_command(KeyCommand::from_setting(&json!(command)))
        .await;
    Some(Linear::new(HttpTransport::new(), keys))
}

#[tokio::test]
#[ignore = "talks to the real Linear API; needs ICARUS_LINEAR_LIVE_KEY_COMMAND"]
async fn reads_the_viewer_a_team_and_recent_issues() {
    let Some(linear) = live().await else {
        panic!("set ICARUS_LINEAR_LIVE_KEY_COMMAND");
    };

    let viewer = linear
        .query("query Viewer { viewer { id name } }", json!({}))
        .await
        .expect("viewer");
    assert!(
        viewer["viewer"]["id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(linear.keys().status().await, KeyStatus::Command);

    let teams = linear
        .query(
            "query Teams { teams(first: 5) { nodes { id key name } } }",
            json!({}),
        )
        .await
        .expect("teams");
    let team_keys: Vec<String> = teams["teams"]["nodes"]
        .as_array()
        .expect("team nodes")
        .iter()
        .filter_map(|team| team["key"].as_str().map(str::to_owned))
        .collect();
    assert!(
        !team_keys.is_empty(),
        "the viewer belongs to at least one team"
    );

    let recent = linear
        .query(
            "query Recent { issues(first: 2, orderBy: updatedAt) { nodes { identifier title } } }",
            json!({}),
        )
        .await
        .expect("recent issues");
    let identifiers: Vec<String> = recent["issues"]["nodes"]
        .as_array()
        .expect("issue nodes")
        .iter()
        .filter_map(|issue| issue["identifier"].as_str().map(str::to_owned))
        .collect();
    assert!(!identifiers.is_empty(), "the workspace has issues");

    let found = linear.issues(&identifiers).await.expect("batch");
    for (index, identifier) in identifiers.iter().enumerate() {
        let issue = found[identifier]
            .as_ref()
            .expect("issue resolved by identifier");
        assert_eq!(&issue.identifier, identifier);
        assert_eq!(
            Some(issue.title.as_str()),
            recent["issues"]["nodes"][index]["title"].as_str()
        );
        assert!(issue.url.starts_with("https://linear.app/"));
        let state = issue.state.as_ref().expect("state");
        assert!(!state.name.is_empty() && !state.kind.is_empty() && state.color.starts_with('#'));
        let team = issue.team_key.as_deref().expect("team key");
        assert!(identifier.starts_with(&format!("{team}-")));
        assert!(team_keys.iter().any(|key| key == team) || !team_keys.is_empty());
        assert!(issue.branch_name.as_deref().is_some_and(|b| !b.is_empty()));
        assert!(issue.updated_at.is_some());
        assert!(issue.priority.is_some_and(|p| p <= 4));
        for label in &issue.labels {
            assert!(!label.name.is_empty() && label.color.starts_with('#'));
        }
    }

    let single = linear
        .issue(&identifiers[0].to_lowercase())
        .await
        .expect("single");
    assert_eq!(single.identifier, identifiers[0]);

    let missing = linear.issue("ZZZZZZ-999999").await;
    assert!(
        matches!(missing, Err(icarus_linear::LinearError::NotFound(_))),
        "an unknown identifier is not found, got {missing:?}"
    );
}
