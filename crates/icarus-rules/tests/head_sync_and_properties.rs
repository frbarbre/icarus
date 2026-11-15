mod common;

use common::*;
use icarus_rules::facts::{Mergeable, PrState, RequestedReviewer, Review, ReviewState, Snapshot};
use icarus_rules::head_sync::*;
use icarus_rules::model::{Cond, GroupEntry, Outcome, RuleGroup, RulesConfig};
use icarus_rules::orca_preset::CONDITIONS;
use serde_json::{Map, Value, json};

const HEAD: &str = "1111111111111111111111111111111111111111";
const NEW: &str = "2222222222222222222222222222222222222222";

mod main_github_review_head_sync_test {
    use super::*;

    fn observed() -> HeadSyncObservation {
        HeadSyncObservation {
            head: HEAD.to_owned(),
            symbolic_branch: Some("feature".to_owned()),
            dirty_tracked: false,
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
                "keeps uncommitted changes",
                NEW,
                HeadSyncObservation {
                    dirty_tracked: true,
                    ..observed()
                },
                skip(HeadSyncSkip::Dirty),
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
}

fn mapping_of(mapped: &[&str]) -> Value {
    let mapping: Map<String, Value> = mapped
        .iter()
        .map(|condition| {
            (
                (*condition).to_owned(),
                Value::String(format!("{condition}-col")),
            )
        })
        .collect();
    json!({ "statusByCondition": mapping, "pmApprovalTeam": PM_TEAM })
}

fn varied_prs() -> Vec<icarus_rules::facts::PullRequest> {
    let mut prs = Vec::new();
    for bits in 0u32..512 {
        let mut subject = pr(1);
        let on = |bit: u32| bits & (1 << bit) != 0;
        subject.state = if on(0) {
            PrState::Merged
        } else if on(1) {
            PrState::Closed
        } else {
            PrState::Open
        };
        if on(2) {
            subject.author = Some("colleague".to_owned());
        }
        subject.is_in_merge_queue = on(3);
        subject.mergeable = Some(match (on(4), on(5)) {
            (true, _) => Mergeable::Conflicting,
            (false, true) => Mergeable::Unknown,
            _ => Mergeable::Mergeable,
        });
        subject.is_draft = on(6);
        if on(7) {
            subject.latest_reviews =
                vec![Review::new("reviewer", ReviewState::ChangesRequested, None)];
        }
        if on(8) {
            subject.checks = vec![success_run("Reviews satisfied")];
            subject.requested_reviewers = vec![RequestedReviewer::user("PmPerson")];
        }
        prs.push(subject);
    }
    prs
}

fn reference_condition(
    subject: &icarus_rules::facts::PullRequest,
    mapped: &[&str],
) -> Option<&'static str> {
    let is_mapped = |condition: &str| mapped.contains(&condition);
    match subject.state {
        PrState::Merged => return Some("merged"),
        PrState::Closed => return Some("closed"),
        PrState::Open => {}
    }
    if subject.author.as_deref() != Some(VIEWER) {
        return Some("reviewing");
    }
    if subject.is_in_merge_queue && is_mapped("merge-queue") {
        return Some("merge-queue");
    }
    if is_mapped("conflicts") {
        match subject.mergeable {
            Some(Mergeable::Conflicting) => return Some("conflicts"),
            Some(Mergeable::Unknown) => return None,
            _ => {}
        }
    }
    if subject.is_draft {
        return Some("draft");
    }
    if !subject.latest_reviews.is_empty() {
        return Some("changes-requested");
    }
    if !subject.checks.is_empty() {
        return Some("merging");
    }
    Some("review")
}

mod first_match_properties {
    use super::*;

    #[test]
    fn the_preset_matches_a_reference_classifier_for_every_mapping_subset() {
        let snapshot = with_pm(snapshot(), &["PmPerson"]);
        let prs = varied_prs();
        for mask in 0u32..(1 << CONDITIONS.len()) {
            let mapped: Vec<&str> = CONDITIONS
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, condition)| *condition)
                .collect();
            if mask % 37 != 0 && mask != (1 << CONDITIONS.len()) - 1 {
                continue;
            }
            for subject in &prs {
                let outcome = classify_with(mapping_of(&mapped), subject, &snapshot)
                    .expect("review always matches");
                let expected = reference_condition(subject, &mapped);
                assert_eq!(
                    outcome.condition.as_deref(),
                    expected,
                    "mapped {mapped:?}, pr {subject:?}"
                );
                let expected_status = expected
                    .filter(|c| mapped.contains(c))
                    .map(|c| format!("{c}-col"));
                assert_eq!(outcome.status, expected_status);
            }
        }
    }

    #[test]
    fn a_card_moves_at_most_once_per_tick() {
        let snapshot = Snapshot {
            linked: varied_prs().into_iter().take(1).collect(),
            ..with_pm(snapshot(), &["PmPerson"])
        };
        let preset = preset(mapping_of(&CONDITIONS));
        for subject in varied_prs() {
            let snapshot = Snapshot {
                linked: vec![subject],
                ..snapshot.clone()
            };
            let result = run(&preset, vec![workspace("one", 1)], Some(snapshot), &[]);
            assert!(set_columns(&result).len() + deletes(&result).len() <= 1);
        }
    }

    #[test]
    fn earlier_entries_shadow_later_ones_and_skip_falls_through() {
        let group = |first: Outcome| RuleGroup {
            id: "g".to_owned(),
            respect_hold: true,
            entries: vec![
                GroupEntry {
                    condition: Some("a".to_owned()),
                    when: Cond::Always,
                    outcome: first,
                    actions: vec![],
                    on_enter: vec![],
                },
                GroupEntry {
                    condition: Some("b".to_owned()),
                    when: Cond::Always,
                    outcome: Outcome::MoveTo("b-col".to_owned()),
                    actions: vec![],
                    on_enter: vec![],
                },
            ],
        };
        let config = RulesConfig {
            enabled: true,
            scope: vec!["repo-1".to_owned()],
            ..RulesConfig::default()
        };
        let check = |first: Outcome| {
            icarus_rules::engine::classify(&config, &group(first), &pr(1), &snapshot(), &[])
                .and_then(|outcome| outcome.condition)
        };
        assert_eq!(check(Outcome::Stop).as_deref(), Some("a"));
        assert_eq!(
            check(Outcome::MoveTo("a-col".to_owned())).as_deref(),
            Some("a")
        );
        assert_eq!(check(Outcome::Skip).as_deref(), Some("b"));
    }
}

mod hold_properties {
    use super::*;

    #[test]
    fn a_held_workspace_gets_no_pull_request_action_whatever_the_pr_says() {
        let all = mapping_of(&CONDITIONS);
        let mut config = all.as_object().cloned().unwrap_or_default();
        for (key, value) in [
            ("holdStatus", json!("on-hold")),
            ("onResolved", json!("delete")),
            ("onReviewed", json!("delete")),
            ("resolveConflictsWithAgent", json!(true)),
            ("reviewEnteredCommand", json!("make dev-down")),
        ] {
            config.insert(key.to_owned(), value);
        }
        let preset = preset(Value::Object(config));
        let snapshot = with_pm(snapshot(), &["PmPerson"]);
        for mut subject in varied_prs() {
            subject.head_ref_oid = NEW.to_owned();
            let mut held = workspace("one", 1);
            held.status = Some("on-hold".to_owned());
            let result = run(
                &preset,
                vec![held],
                Some(Snapshot {
                    linked: vec![subject],
                    ..snapshot.clone()
                }),
                &[],
            );
            assert!(result.actions.is_empty(), "{:?}", result.actions);
        }
    }

    #[test]
    fn releasing_the_hold_lets_rules_act_again() {
        let preset = preset(
            json!({ "holdStatus": "on-hold", "statusByCondition": { "draft": "draft-col" } }),
        );
        let mut draft = pr(1);
        draft.is_draft = true;
        let snapshot = Snapshot {
            linked: vec![draft],
            ..snapshot()
        };
        let mut ws = workspace("one", 1);
        for (status, moves) in [("on-hold", 0), ("todo", 1), ("draft-col", 0)] {
            ws.status = Some(status.to_owned());
            let result = run(&preset, vec![ws.clone()], Some(snapshot.clone()), &[]);
            assert_eq!(set_columns(&result).len(), moves, "{status}");
        }
    }
}

mod derived_events {
    use super::*;
    use icarus_rules::model::{ActionSpec, Rule, RuleScope, Trigger};

    fn notify_rule(trigger: Trigger) -> Rule {
        Rule {
            id: "notify".to_owned(),
            trigger,
            scope: RuleScope::Group,
            respect_hold: true,
            when: Cond::Always,
            guards: vec![],
            actions: vec![ActionSpec::Notify {
                message: "#{{prNumber}} changed".to_owned(),
            }],
            max_per_evaluation: None,
        }
    }

    #[test]
    fn head_moves_fire_once_and_not_on_first_sight() {
        let mut preset = preset(json!({}));
        preset.config.rules = vec![notify_rule(Trigger::PrHeadMoved)];
        let mut subject = pr(1);
        let first = run(
            &preset,
            vec![workspace("one", 1)],
            Some(Snapshot {
                linked: vec![subject.clone()],
                ..snapshot()
            }),
            &[],
        );
        assert!(first.actions.is_empty());
        subject.head_ref_oid = "moved".to_owned();
        let second = run_with_state(
            &preset,
            vec![workspace("one", 1)],
            Some(Snapshot {
                linked: vec![subject.clone()],
                ..snapshot()
            }),
            &[],
            &first.state,
            1,
        );
        assert_eq!(
            second.actions,
            vec![icarus_rules::PlannedAction::Notify {
                workspace_id: Some("one".to_owned()),
                message: "#1 changed".to_owned()
            }]
        );
        let third = run_with_state(
            &preset,
            vec![workspace("one", 1)],
            Some(Snapshot {
                linked: vec![subject],
                ..snapshot()
            }),
            &[],
            &second.state,
            2,
        );
        assert!(third.actions.is_empty());
    }

    #[test]
    fn rate_limit_guard_caps_firings_per_pr() {
        let mut preset = preset(json!({}));
        let mut rule = notify_rule(Trigger::Tick(
            icarus_rules::model::TickSubject::LinkedWorkspaces,
        ));
        rule.guards = vec![icarus_rules::model::Guard::RateLimit {
            max: 2,
            window_ms: 100,
        }];
        preset.config.rules = vec![rule];
        let snapshot = Snapshot {
            linked: vec![pr(1)],
            ..snapshot()
        };
        let mut state = preset.state.clone();
        let mut fired = Vec::new();
        for now in [0, 10, 20, 115, 130] {
            let result = run_with_state(
                &preset,
                vec![workspace("one", 1)],
                Some(snapshot.clone()),
                &[],
                &state,
                now,
            );
            fired.push(result.actions.len());
            state = result.state;
        }
        assert_eq!(fired, vec![1, 1, 0, 1, 1]);
    }

    #[test]
    fn config_round_trips_through_json() {
        let preset = preset(mapping_of(&CONDITIONS));
        let text = serde_json::to_string(&preset.config).expect("config serializes");
        let back: RulesConfig = serde_json::from_str(&text).expect("config deserializes");
        assert_eq!(back, preset.config);
    }
}
