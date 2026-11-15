mod common;

use common::*;
use icarus_rules::facts::{
    MergeStateStatus, Mergeable, PrState, RequestedReviewer, Review, ReviewState,
    StatusContextState,
};
use serde_json::json;

fn pm() -> serde_json::Value {
    json!({ "pmApprovalTeam": PM_TEAM })
}

fn pm_snapshot() -> icarus_rules::facts::Snapshot {
    with_pm(snapshot(), &["PmPerson"])
}

fn review(login: &str, state: ReviewState) -> Review {
    Review::new(login, state, None)
}

mod shared_workspace_status_rules_test {
    use super::*;

    #[test]
    fn queued_pr_is_in_the_merge_queue_ahead_of_every_open_state_condition() {
        let mut queued = pr(1);
        queued.is_in_merge_queue = true;
        queued.is_draft = true;
        queued.mergeable = Some(Mergeable::Conflicting);
        let config = json!({ "statusByCondition": { "merge-queue": "queued" } });
        assert_eq!(
            condition(config.clone(), &queued, &snapshot()).as_deref(),
            Some("merge-queue")
        );
        let mut other_viewer = snapshot();
        other_viewer.viewer_login = Some("someone-else".to_owned());
        assert_eq!(
            condition(config, &queued, &other_viewer).as_deref(),
            Some("reviewing")
        );
    }

    #[test]
    fn queued_pr_falls_through_while_merge_queue_has_no_column() {
        let mut queued = pr(1);
        queued.is_in_merge_queue = true;
        assert_eq!(
            condition(json!({}), &queued, &snapshot()).as_deref(),
            Some("review")
        );
    }

    #[test]
    fn lifecycle_conditions() {
        type Edit = Box<dyn Fn(&mut icarus_rules::facts::PullRequest)>;
        let cases: Vec<(&str, Edit, &str)> = vec![
            (
                "merged before anything else",
                Box::new(|p| {
                    p.state = PrState::Merged;
                    p.is_draft = true;
                    p.checks = vec![success_run("Reviews satisfied")];
                }),
                "merged",
            ),
            (
                "closed unmerged",
                Box::new(|p| p.state = PrState::Closed),
                "closed",
            ),
            (
                "closed even when someone else's",
                Box::new(|p| {
                    p.state = PrState::Closed;
                    p.author = Some("colleague".to_owned());
                }),
                "closed",
            ),
            ("draft", Box::new(|p| p.is_draft = true), "draft"),
            ("ready with no reviewers", Box::new(|_| {}), "review"),
            (
                "someone else's PR",
                Box::new(|p| p.author = Some("colleague".to_owned())),
                "reviewing",
            ),
            (
                "reviewing even once every check and approval has landed",
                Box::new(|p| {
                    p.author = Some("colleague".to_owned());
                    p.checks = vec![success_run("Reviews satisfied")];
                    p.latest_reviews = vec![review("someone", ReviewState::Approved)];
                }),
                "reviewing",
            ),
            (
                "merged PR you were reviewing",
                Box::new(|p| {
                    p.author = Some("colleague".to_owned());
                    p.state = PrState::Merged;
                }),
                "merged",
            ),
        ];
        for (name, edit, expected) in cases {
            let mut subject = pr(1);
            edit(&mut subject);
            assert_eq!(
                condition(pm(), &subject, &pm_snapshot()).as_deref(),
                Some(expected),
                "{name}"
            );
        }
    }

    #[test]
    fn returns_to_draft_after_a_ready_pr_is_put_back() {
        let mut subject = pr(1);
        assert_eq!(
            condition(pm(), &subject, &pm_snapshot()).as_deref(),
            Some("review")
        );
        subject.is_draft = true;
        assert_eq!(
            condition(pm(), &subject, &pm_snapshot()).as_deref(),
            Some("draft")
        );
    }

    #[test]
    fn change_requests() {
        let mut requested = pr(1);
        requested.latest_reviews = vec![review("reviewer", ReviewState::ChangesRequested)];
        assert_eq!(
            condition(pm(), &requested, &pm_snapshot()).as_deref(),
            Some("changes-requested")
        );

        requested.requested_reviewers = vec![RequestedReviewer::user("reviewer")];
        assert_eq!(
            condition(pm(), &requested, &pm_snapshot()).as_deref(),
            Some("review"),
            "cleared once that reviewer is re-requested"
        );

        let mut cased = pr(1);
        cased.latest_reviews = vec![review("Reviewer", ReviewState::ChangesRequested)];
        cased.requested_reviewers = vec![RequestedReviewer::user("reviewer")];
        assert_eq!(
            condition(pm(), &cased, &pm_snapshot()).as_deref(),
            Some("review")
        );
    }

    #[test]
    fn goes_to_pm_approval_when_a_re_request_leaves_only_a_pm_pending() {
        let mut subject = pr(1);
        subject.latest_reviews = vec![
            review("reviewer", ReviewState::Approved),
            review("pmperson", ReviewState::ChangesRequested),
        ];
        subject.requested_reviewers = vec![RequestedReviewer::user("PmPerson")];
        assert_eq!(
            condition(pm(), &subject, &pm_snapshot()).as_deref(),
            Some("pm-approval")
        );
    }

    #[test]
    fn named_check_gate() {
        let cases = [
            (vec![success_run("Reviews satisfied")], "merging"),
            (vec![success_run("build")], "review"),
            (vec![success_run("  reviews SATISFIED ")], "merging"),
            (
                vec![status_check(
                    "Reviews satisfied",
                    StatusContextState::Failure,
                )],
                "review",
            ),
            (
                vec![status_check(
                    "Reviews satisfied",
                    StatusContextState::Pending,
                )],
                "review",
            ),
            (
                vec![icarus_rules::facts::Check::run(
                    "Reviews satisfied",
                    icarus_rules::facts::CheckRunStatus::Completed,
                    Some(icarus_rules::facts::CheckConclusion::Neutral),
                    None,
                )],
                "review",
            ),
        ];
        for (checks, expected) in cases {
            let mut subject = pr(1);
            subject.checks = checks.clone();
            assert_eq!(
                condition(pm(), &subject, &pm_snapshot()).as_deref(),
                Some(expected),
                "{checks:?}"
            );
        }
    }

    #[test]
    fn merging_once_a_pm_and_every_other_reviewer_approved_before_the_gate_runs() {
        let mut subject = pr(1);
        subject.latest_reviews = vec![
            review("bot-reviewer", ReviewState::Commented),
            review("PmPerson", ReviewState::Approved),
            review("reviewer", ReviewState::Approved),
        ];
        subject.latest_opinionated_reviews = Some(vec![
            review("PmPerson", ReviewState::Approved),
            review("reviewer", ReviewState::Approved),
        ]);
        subject.checks = vec![status_check(
            "Reviews satisfied",
            StatusContextState::Pending,
        )];
        assert_eq!(
            condition(pm(), &subject, &pm_snapshot()).as_deref(),
            Some("merging")
        );
    }

    #[test]
    fn stays_in_review_without_a_pm_approval_or_with_someone_pending() {
        let mut without_pm = pr(1);
        without_pm.latest_reviews = vec![review("reviewer", ReviewState::Approved)];
        assert_eq!(
            condition(pm(), &without_pm, &pm_snapshot()).as_deref(),
            Some("review")
        );

        let mut still_pending = pr(1);
        still_pending.latest_reviews = vec![review("PmPerson", ReviewState::Approved)];
        still_pending.requested_reviewers = vec![RequestedReviewer::user("other")];
        assert_eq!(
            condition(pm(), &still_pending, &pm_snapshot()).as_deref(),
            Some("review")
        );

        let mut no_team = pr(1);
        no_team.latest_reviews = vec![review("PmPerson", ReviewState::Approved)];
        assert_eq!(
            condition(pm(), &no_team, &snapshot()).as_deref(),
            Some("review")
        );
        assert_eq!(
            condition(json!({}), &no_team, &pm_snapshot()).as_deref(),
            Some("review")
        );
    }

    #[test]
    fn pm_approval_needs_every_pending_user_to_be_a_pm() {
        let mut only_pm = pr(1);
        only_pm.requested_reviewers = vec![RequestedReviewer::user("pmperson")];
        only_pm.latest_reviews = vec![review("reviewer", ReviewState::Approved)];
        assert_eq!(
            condition(pm(), &only_pm, &pm_snapshot()).as_deref(),
            Some("pm-approval")
        );

        let mut mixed = pr(1);
        mixed.requested_reviewers = vec![
            RequestedReviewer::user("pmperson"),
            RequestedReviewer::user("reviewer"),
        ];
        assert_eq!(
            condition(pm(), &mixed, &pm_snapshot()).as_deref(),
            Some("review")
        );

        let mut unconfigured = pr(1);
        unconfigured.requested_reviewers = vec![RequestedReviewer::user("pmperson")];
        assert_eq!(
            condition(pm(), &unconfigured, &snapshot()).as_deref(),
            Some("review")
        );

        let mut team = pr(1);
        team.requested_reviewers = vec![RequestedReviewer::team("pm-approval")];
        assert_eq!(
            condition(pm(), &team, &pm_snapshot()).as_deref(),
            Some("review")
        );
    }

    #[test]
    fn falls_back_to_your_own_columns_when_the_viewer_is_unknown() {
        let mut subject = pr(1);
        subject.author = Some("colleague".to_owned());
        let mut unknown = pm_snapshot();
        unknown.viewer_login = None;
        assert_eq!(
            condition(pm(), &subject, &unknown).as_deref(),
            Some("review")
        );
    }

    #[test]
    fn unmapped_condition_has_no_status() {
        let outcome = classify_with(json!({}), &pr(1), &snapshot()).expect("review always matches");
        assert_eq!(outcome.condition.as_deref(), Some("review"));
        assert_eq!(outcome.status, None);
    }

    #[test]
    fn mapped_condition_has_its_status() {
        let mut draft = pr(1);
        draft.is_draft = true;
        let outcome = classify_with(
            json!({ "statusByCondition": { "draft": "in-progress" } }),
            &draft,
            &snapshot(),
        )
        .expect("draft matches");
        assert_eq!(outcome.condition.as_deref(), Some("draft"));
        assert_eq!(outcome.status.as_deref(), Some("in-progress"));
    }

    #[test]
    fn closed_pr_stays_put_when_closed_is_unmapped() {
        let mut closed = pr(1);
        closed.state = PrState::Closed;
        let outcome = classify_with(
            json!({ "statusByCondition": { "draft": "todo", "review": "in-review", "merged": "completed" } }),
            &closed,
            &snapshot(),
        )
        .expect("closed matches");
        assert_eq!(outcome.condition.as_deref(), Some("closed"));
        assert_eq!(outcome.status, None);
    }
}

mod shared_workspace_status_rules_conflicts_test {
    use super::*;

    fn mapped() -> serde_json::Value {
        json!({ "statusByCondition": { "conflicts": "conflicts", "draft": "draft", "review": "review" } })
    }

    fn mergeable(state: Mergeable) -> icarus_rules::facts::PullRequest {
        let mut subject = pr(1);
        subject.mergeable = Some(state);
        subject
    }

    #[test]
    fn conflicting_pr_goes_to_conflicts_even_as_a_draft() {
        let mut subject = mergeable(Mergeable::Conflicting);
        subject.is_draft = true;
        assert_eq!(
            condition(mapped(), &subject, &snapshot()).as_deref(),
            Some("conflicts")
        );
    }

    #[test]
    fn someone_elses_conflicting_pr_stays_with_the_reviewer() {
        let mut subject = mergeable(Mergeable::Conflicting);
        subject.author = Some("colleague".to_owned());
        assert_eq!(
            condition(mapped(), &subject, &snapshot()).as_deref(),
            Some("reviewing")
        );
    }

    #[test]
    fn holds_the_card_while_mergeability_is_unknown() {
        let outcome = classify_with(mapped(), &mergeable(Mergeable::Unknown), &snapshot())
            .expect("the unknown hold matches");
        assert_eq!(
            outcome,
            icarus_rules::engine::Classification {
                condition: None,
                status: None
            }
        );
    }

    #[test]
    fn moves_on_once_resolved() {
        assert_eq!(
            condition(mapped(), &mergeable(Mergeable::Mergeable), &snapshot()).as_deref(),
            Some("review")
        );
    }

    #[test]
    fn changes_nothing_until_conflicts_is_mapped() {
        for state in [Mergeable::Conflicting, Mergeable::Unknown] {
            assert_eq!(
                condition(json!({}), &mergeable(state), &snapshot()).as_deref(),
                Some("review")
            );
        }
    }

    #[test]
    fn change_request_stays_in_force_after_a_plain_comment() {
        let mut subject = mergeable(Mergeable::Mergeable);
        subject.latest_reviews = vec![review("reviewer", ReviewState::Commented)];
        subject.latest_opinionated_reviews =
            Some(vec![review("reviewer", ReviewState::ChangesRequested)]);
        assert_eq!(
            condition(json!({}), &subject, &snapshot()).as_deref(),
            Some("changes-requested")
        );
    }

    #[test]
    fn change_request_clears_on_approval_or_re_request() {
        let mut approved = mergeable(Mergeable::Mergeable);
        approved.latest_opinionated_reviews = Some(vec![review("reviewer", ReviewState::Approved)]);
        assert_eq!(
            condition(json!({}), &approved, &snapshot()).as_deref(),
            Some("review")
        );

        let mut asked_again = mergeable(Mergeable::Mergeable);
        asked_again.requested_reviewers = vec![RequestedReviewer::user("reviewer")];
        asked_again.latest_opinionated_reviews =
            Some(vec![review("reviewer", ReviewState::ChangesRequested)]);
        assert_eq!(
            condition(json!({}), &asked_again, &snapshot()).as_deref(),
            Some("review")
        );
    }

    #[test]
    fn dirty_merge_state_alone_does_not_count_as_conflicts() {
        let mut subject = mergeable(Mergeable::Mergeable);
        subject.merge_state_status = Some(MergeStateStatus::Dirty);
        assert_eq!(
            condition(mapped(), &subject, &snapshot()).as_deref(),
            Some("review")
        );
    }
}

mod main_github_review_status_snapshot_merge_gate_test {
    use super::*;

    fn condition_for(state: StatusContextState, pr_state: PrState) -> Option<String> {
        let mut subject = pr(3170);
        subject.state = pr_state;
        subject.checks = vec![
            status_check("Reviews satisfied", state),
            status_check("Combined CI", StatusContextState::Success),
        ];
        condition(json!({}), &subject, &snapshot())
    }

    #[test]
    fn gate_states() {
        let cases = [
            (StatusContextState::Success, PrState::Open, "merging"),
            (StatusContextState::Pending, PrState::Open, "review"),
            (StatusContextState::Failure, PrState::Open, "review"),
            (StatusContextState::Success, PrState::Merged, "merged"),
        ];
        for (state, pr_state, expected) in cases {
            assert_eq!(condition_for(state, pr_state).as_deref(), Some(expected));
        }
    }
}

mod check_state_mapping {
    use icarus_rules::facts::{
        Check, CheckConclusion as C, CheckRunStatus as S, CheckState, StatusContextState as X,
    };

    #[test]
    fn check_runs_and_statuses_map_like_the_snapshot() {
        let cases = [
            (
                Check::run("a", S::Completed, Some(C::Success), None),
                CheckState::Success,
            ),
            (
                Check::run("a", S::InProgress, None, None),
                CheckState::Pending,
            ),
            (
                Check::run("a", S::Completed, Some(C::Skipped), None),
                CheckState::Neutral,
            ),
            (
                Check::run("a", S::Completed, Some(C::Neutral), None),
                CheckState::Neutral,
            ),
            (
                Check::run("a", S::Completed, Some(C::Cancelled), None),
                CheckState::Failure,
            ),
            (
                Check::run("a", S::Completed, None, None),
                CheckState::Pending,
            ),
            (Check::run("a", S::Waiting, None, None), CheckState::Pending),
            (
                Check::run("a", S::Requested, None, None),
                CheckState::Pending,
            ),
            (Check::run("a", S::Pending, None, None), CheckState::Pending),
            (Check::status("a", X::Error, None), CheckState::Failure),
            (Check::status("a", X::Expected, None), CheckState::Pending),
        ];
        for (check, expected) in cases {
            assert_eq!(check.state(), expected, "{check:?}");
        }
        assert!(Check::run("a", S::Waiting, None, None).is_waiting_approval());
        assert!(Check::run("a", S::Waiting, None, None).is_running());
    }
}
