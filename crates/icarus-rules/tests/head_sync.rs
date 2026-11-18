use icarus_rules::head_sync::*;

const HEAD: &str = "1111111111111111111111111111111111111111";
const NEW: &str = "2222222222222222222222222222222222222222";

fn observed() -> HeadSyncObservation {
    HeadSyncObservation {
        head: HEAD.to_owned(),
        symbolic_branch: Some("feature".to_owned()),
        target_reachable: true,
        viewer_email: Some("me@example.test".to_owned()),
        local_only_authors: vec!["author@example.test".to_owned()],
    }
}

#[test]
fn decision_table() {
    let skip = |reason| HeadSyncDecision::Skipped { reason };
    let reset = HeadSyncDecision::ResetHard {
        previous_head: HEAD.to_owned(),
    };
    let cases: Vec<(&str, &str, HeadSyncObservation, HeadSyncDecision)> = vec![
        (
            "moves onto a force-pushed head",
            NEW,
            observed(),
            reset.clone(),
        ),
        (
            "already on the head",
            HEAD,
            observed(),
            HeadSyncDecision::Current,
        ),
        (
            "keeps a commit of the reviewer",
            NEW,
            HeadSyncObservation {
                local_only_authors: vec!["Me@Example.test".to_owned()],
                ..observed()
            },
            skip(HeadSyncSkip::OwnCommits),
        ),
        (
            "keeps local commits when the reviewer's email is unknown",
            NEW,
            HeadSyncObservation {
                viewer_email: None,
                ..observed()
            },
            skip(HeadSyncSkip::OwnCommits),
        ),
        (
            "keeps a checkout on another branch",
            NEW,
            HeadSyncObservation {
                symbolic_branch: Some("other".to_owned()),
                ..observed()
            },
            skip(HeadSyncSkip::OtherBranch),
        ),
        (
            "keeps a detached checkout",
            NEW,
            HeadSyncObservation {
                symbolic_branch: None,
                ..observed()
            },
            skip(HeadSyncSkip::OtherBranch),
        ),
        (
            "keeps a checkout when the head cannot be fetched",
            NEW,
            HeadSyncObservation {
                target_reachable: false,
                ..observed()
            },
            skip(HeadSyncSkip::Unreachable),
        ),
        (
            "refuses a malformed oid",
            "not-an-oid",
            observed(),
            skip(HeadSyncSkip::Unreachable),
        ),
        (
            "resets when no commit is local-only",
            NEW,
            HeadSyncObservation {
                local_only_authors: vec![" ".to_owned()],
                viewer_email: None,
                ..observed()
            },
            reset,
        ),
    ];
    for (name, target, observation, expected) in cases {
        assert_eq!(
            decide_head_sync("feature", target, &observation),
            expected,
            "{name}"
        );
    }
}
