mod common;

use common::*;
use icarus_rules::ActionOutcome;
use icarus_rules::facts::{
    Mergeable, PrState, RepoRef, RequestedReviewer, Review, ReviewState, Snapshot, ViewerReview,
    Workspace,
};
use icarus_rules::{EngineState, PlannedAction};
use serde_json::{Value, json};

fn mapped() -> Value {
    json!({ "statusByCondition": {
        "draft": "draft-col",
        "review": "review-col",
        "merged": "done-col",
        "changes-requested": "changes-col"
    } })
}

fn merge(base: Value, extra: Value) -> Value {
    let mut base = base;
    if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    base
}

fn linked(prs: Vec<icarus_rules::facts::PullRequest>) -> Snapshot {
    Snapshot {
        linked: prs,
        ..snapshot()
    }
}

fn inbox(prs: Vec<icarus_rules::facts::PullRequest>) -> Snapshot {
    Snapshot {
        review_requested: prs,
        ..snapshot()
    }
}

fn draft() -> icarus_rules::facts::PullRequest {
    let mut subject = pr(1);
    subject.is_draft = true;
    subject
}

fn theirs(number: u64, branch: &str) -> icarus_rules::facts::PullRequest {
    let mut subject = pr(number);
    subject.author = Some("colleague".to_owned());
    subject.head_ref_name = branch.to_owned();
    subject
}

mod shared_workspace_status_rule_plan_test {
    use super::*;

    #[test]
    fn plans_nothing_while_disabled() {
        let preset = preset(merge(mapped(), json!({ "enabled": false })));
        let result = run(
            &preset,
            vec![workspace("one", 1)],
            Some(linked(vec![draft()])),
            &[],
        );
        assert!(result.actions.is_empty());
    }

    #[test]
    fn plans_nothing_while_no_project_is_in_scope() {
        let preset = preset(merge(mapped(), json!({ "repoIds": [] })));
        let result = run(
            &preset,
            vec![workspace("one", 1)],
            Some(linked(vec![draft()])),
            &[],
        );
        assert!(result.actions.is_empty());
    }

    #[test]
    fn moves_a_workspace_whose_pr_changed_condition() {
        let result = run(
            &preset(mapped()),
            vec![workspace("one", 1)],
            Some(linked(vec![draft()])),
            &[],
        );
        assert_eq!(
            set_columns(&result),
            vec![(
                "one".to_owned(),
                "draft-col".to_owned(),
                Some("draft".to_owned())
            )]
        );
    }

    #[test]
    fn leaves_a_workspace_already_in_the_right_column() {
        let mut ws = workspace("one", 1);
        ws.status = Some("draft-col".to_owned());
        let result = run(
            &preset(mapped()),
            vec![ws],
            Some(linked(vec![draft()])),
            &[],
        );
        assert!(set_columns(&result).is_empty());
    }

    #[test]
    fn a_workspace_with_no_column_counts_as_different() {
        let mut ws = workspace("one", 1);
        ws.status = None;
        let result = run(
            &preset(mapped()),
            vec![ws],
            Some(linked(vec![draft()])),
            &[],
        );
        assert_eq!(set_columns(&result).len(), 1);
    }

    #[test]
    fn ignores_a_workspace_whose_pr_is_missing_from_the_snapshot() {
        let result = run(
            &preset(mapped()),
            vec![workspace("one", 99)],
            Some(linked(vec![pr(1)])),
            &[],
        );
        assert!(set_columns(&result).is_empty());
    }

    #[test]
    fn matches_repositories_case_insensitively() {
        let mut ws = workspace("one", 1);
        if let Some(link) = ws.pr.as_mut() {
            link.repo = RepoRef::new("ACME", "App");
        }
        let result = run(
            &preset(mapped()),
            vec![ws],
            Some(linked(vec![draft()])),
            &[],
        );
        assert_eq!(set_columns(&result).len(), 1);
    }

    #[test]
    fn an_empty_snapshot_plans_no_pull_request_actions() {
        let result = run(&preset(mapped()), vec![workspace("one", 1)], None, &[]);
        assert!(result.actions.is_empty());
    }

    #[test]
    fn skips_archived_main_and_out_of_scope_workspaces() {
        let mut archived = workspace("archived", 1);
        archived.archived = true;
        let mut main = workspace("main", 1);
        main.main = true;
        let mut other = workspace("other", 1);
        other.repo_id = "repo-2".to_owned();
        let result = run(
            &preset(mapped()),
            vec![archived, main, other],
            Some(linked(vec![draft()])),
            &[],
        );
        assert!(set_columns(&result).is_empty());
    }

    mod merged_workspaces {
        use super::*;

        fn merged() -> Snapshot {
            let mut subject = pr(1);
            subject.state = PrState::Merged;
            linked(vec![subject])
        }

        #[test]
        fn only_moves_the_card_when_delete_on_merge_is_off() {
            let result = run(
                &preset(mapped()),
                vec![workspace("one", 1)],
                Some(merged()),
                &[],
            );
            assert!(deletes(&result).is_empty());
            assert_eq!(set_columns(&result)[0].1, "done-col");
        }

        #[test]
        fn removes_the_workspace_and_never_also_moves_it() {
            let preset = preset(merge(mapped(), json!({ "onResolved": "delete" })));
            let result = run(&preset, vec![workspace("one", 1)], Some(merged()), &[]);
            assert_eq!(deletes(&result), vec![("one".to_owned(), false, None)]);
            assert!(set_columns(&result).is_empty());
        }
    }

    mod closed_without_merging {
        use super::*;

        fn closed() -> Snapshot {
            let mut subject = pr(1);
            subject.state = PrState::Closed;
            linked(vec![subject])
        }

        #[test]
        fn removes_on_the_same_switch_as_a_merge() {
            let preset = preset(merge(mapped(), json!({ "onResolved": "delete" })));
            let result = run(&preset, vec![workspace("one", 1)], Some(closed()), &[]);
            assert_eq!(deletes(&result), vec![("one".to_owned(), false, None)]);
        }

        #[test]
        fn keeps_a_workspace_not_at_the_pr_head() {
            let preset = preset(merge(mapped(), json!({ "onResolved": "delete" })));
            let mut ws = workspace("one", 1);
            ws.head_oid = "local-work".to_owned();
            let result = run(&preset, vec![ws], Some(closed()), &[]);
            assert!(deletes(&result).is_empty());
        }

        #[test]
        fn keeps_the_workspace_when_the_switch_is_off() {
            let result = run(
                &preset(mapped()),
                vec![workspace("one", 1)],
                Some(closed()),
                &[],
            );
            assert!(deletes(&result).is_empty());
            assert!(set_columns(&result).is_empty(), "closed is unmapped");
        }
    }

    mod review_inbox {
        use super::*;

        fn inbox_on() -> Value {
            merge(mapped(), json!({ "reviewInbox": { "enabled": true } }))
        }

        #[test]
        fn plans_a_workspace_for_a_new_review_request() {
            let result = run(
                &preset(inbox_on()),
                vec![],
                Some(inbox(vec![theirs(7, "feat/theirs")])),
                &[],
            );
            assert_eq!(creations(&result), vec![("acme/app#7".to_owned(), None)]);
        }

        #[test]
        fn clones_every_outstanding_request_on_the_first_tick() {
            let prs = (1..=8).map(|n| theirs(n, &format!("feat/{n}"))).collect();
            let result = run(&preset(inbox_on()), vec![], Some(inbox(prs)), &[]);
            assert_eq!(creations(&result).len(), 8);
        }

        #[test]
        fn skips_handled_authored_branch_taken_and_linked_prs() {
            let handled = preset(merge(
                inbox_on(),
                json!({ "handledPullRequests": ["acme/app#7"] }),
            ));
            let result = run(
                &handled,
                vec![],
                Some(inbox(vec![theirs(7, "feat/theirs")])),
                &[],
            );
            assert!(creations(&result).is_empty(), "handled");

            let mut mine = pr(7);
            mine.author = Some("ME".to_owned());
            let result = run(&preset(inbox_on()), vec![], Some(inbox(vec![mine])), &[]);
            assert!(creations(&result).is_empty(), "authored by the viewer");

            let mut branch_owner = Workspace {
                id: "old".to_owned(),
                repo_id: "repo-1".to_owned(),
                branch: Some("feat/theirs".to_owned()),
                ..Workspace::default()
            };
            branch_owner.archived = true;
            let result = run(
                &preset(inbox_on()),
                vec![branch_owner],
                Some(inbox(vec![theirs(7, "feat/theirs")])),
                &[],
            );
            assert!(creations(&result).is_empty(), "branch has a workspace");

            let result = run(
                &preset(inbox_on()),
                vec![workspace("one", 7)],
                Some(inbox(vec![theirs(7, "feat/theirs")])),
                &[],
            );
            assert!(creations(&result).is_empty(), "already linked");
        }

        #[test]
        fn caps_creations_per_tick() {
            let prs = (1..=31).map(|n| theirs(n, &format!("feat/{n}"))).collect();
            let result = run(&preset(inbox_on()), vec![], Some(inbox(prs)), &[]);
            assert_eq!(creations(&result).len(), 30);
        }

        #[test]
        fn plans_nothing_when_the_inbox_is_off() {
            let result = run(
                &preset(mapped()),
                vec![],
                Some(inbox(vec![theirs(7, "feat/theirs")])),
                &[],
            );
            assert!(creations(&result).is_empty());
        }

        #[test]
        fn creation_carries_name_column_and_rendered_prompt() {
            let preset = preset(merge(
                inbox_on(),
                json!({ "statusByCondition": { "reviewing": "theirs-col" } }),
            ));
            let mut subject = theirs(7, "feat/theirs");
            subject.title = "Tidy the Timeline!".to_owned();
            let result = run(&preset, vec![], Some(inbox(vec![subject])), &[]);
            let Some(PlannedAction::CreateWorkspaceFromPr {
                name,
                initial_column,
                agent,
                ..
            }) = result.actions.first()
            else {
                panic!("expected a creation, got {:?}", result.actions);
            };
            assert_eq!(name, "review-7-tidy-the-timeline");
            assert_eq!(initial_column.as_deref(), Some("theirs-col"));
            let agent = agent.as_ref().expect("the review agent launches");
            assert_eq!(agent.title, "Review #7");
            assert_eq!(agent.agent.as_deref(), Some("claude"));
            assert!(agent.prompt.starts_with(
                "You are reviewing pull request #7 — \"Tidy the Timeline!\" by colleague"
            ));
        }
    }

    mod hold_column {
        use super::*;

        fn holding(extra: Value) -> icarus_rules::orca_preset::OrcaPreset {
            preset(merge(
                merge(
                    mapped(),
                    json!({ "holdStatus": "on-hold", "onResolved": "delete" }),
                ),
                extra,
            ))
        }

        fn held() -> Workspace {
            let mut ws = workspace("one", 1);
            ws.status = Some("on-hold".to_owned());
            ws
        }

        #[test]
        fn is_never_moved_or_removed() {
            let result = run(
                &holding(json!({})),
                vec![held()],
                Some(linked(vec![draft()])),
                &[],
            );
            assert!(set_columns(&result).is_empty());

            let mut merged = pr(1);
            merged.state = PrState::Merged;
            let result = run(
                &holding(json!({})),
                vec![held()],
                Some(linked(vec![merged])),
                &[],
            );
            assert!(deletes(&result).is_empty());

            let mut reviewed = pr(1);
            reviewed.author = Some("colleague".to_owned());
            reviewed.latest_reviews = vec![Review::new(VIEWER, ReviewState::Approved, Some("sha"))];
            let result = run(
                &holding(json!({ "onReviewed": "delete" })),
                vec![held()],
                Some(linked(vec![reviewed])),
                &[],
            );
            assert!(deletes(&result).is_empty());
        }

        #[test]
        fn still_counts_as_linked_so_the_inbox_never_clones_it() {
            let result = run(
                &holding(json!({ "reviewInbox": { "enabled": true } })),
                vec![held()],
                Some(inbox(vec![theirs(1, "feat/other")])),
                &[],
            );
            assert!(creations(&result).is_empty());
        }

        #[test]
        fn moves_again_once_in_any_other_column() {
            let result = run(
                &holding(json!({})),
                vec![workspace("one", 1)],
                Some(linked(vec![draft()])),
                &[],
            );
            assert_eq!(set_columns(&result)[0].1, "draft-col");
        }

        #[test]
        fn is_left_out_of_the_head_sync() {
            let mut moved = theirs(1, "feat/thing");
            moved.head_ref_oid = "new-head".to_owned();
            let mut released = workspace("two", 1);
            released.status = None;
            let result = run(
                &holding(json!({})),
                vec![held(), released],
                Some(linked(vec![moved])),
                &[],
            );
            assert_eq!(
                syncs(&result),
                vec![("two".to_owned(), "new-head".to_owned())]
            );
        }
    }

    mod closing_after_review {
        use super::*;

        fn reviewed() -> icarus_rules::facts::PullRequest {
            let mut subject = pr(1);
            subject.author = Some("colleague".to_owned());
            subject.latest_reviews = vec![Review::new(
                VIEWER,
                ReviewState::Approved,
                Some("reviewed-sha"),
            )];
            subject
        }

        fn on_reviewed() -> Value {
            json!({ "onReviewed": "delete" })
        }

        fn inbox_handled() -> Value {
            json!({
                "onReviewed": "delete",
                "reviewInbox": { "enabled": true },
                "handledPullRequests": ["acme/app#1"]
            })
        }

        fn removals(
            config: Value,
            snapshot: Snapshot,
            ws: Workspace,
        ) -> Vec<(String, bool, Option<String>)> {
            deletes(&run(&preset(config), vec![ws], Some(snapshot), &[]))
        }

        #[test]
        fn removes_forced_and_forgets_the_ledger_key() {
            assert_eq!(
                removals(on_reviewed(), linked(vec![reviewed()]), workspace("one", 1)),
                vec![("one".to_owned(), true, Some("acme/app#1".to_owned()))]
            );
        }

        #[test]
        fn removes_an_inbox_workspace_once_the_request_is_taken_back() {
            assert_eq!(
                removals(
                    inbox_handled(),
                    linked(vec![theirs(1, "feat/thing")]),
                    workspace("one", 1)
                ),
                vec![("one".to_owned(), true, Some("acme/app#1".to_owned()))]
            );
        }

        #[test]
        fn keeps_while_still_requested_or_a_team_may_owe_it() {
            let still_owed = Snapshot {
                linked: vec![theirs(1, "feat/thing")],
                review_requested: vec![theirs(1, "feat/thing")],
                ..snapshot()
            };
            assert!(removals(inbox_handled(), still_owed, workspace("one", 1)).is_empty());

            let mut via_team = theirs(1, "feat/thing");
            via_team.requested_reviewers = vec![RequestedReviewer::team("frontend")];
            assert!(
                removals(inbox_handled(), linked(vec![via_team]), workspace("one", 1)).is_empty()
            );
        }

        #[test]
        fn keeps_a_workspace_you_opened_yourself() {
            assert!(
                removals(
                    on_reviewed(),
                    linked(vec![theirs(1, "feat/thing")]),
                    workspace("one", 1)
                )
                .is_empty()
            );
        }

        #[test]
        fn keeps_after_a_comment_only_review_was_taken_back() {
            let mut commented = theirs(1, "feat/thing");
            commented.latest_reviews =
                vec![Review::new(VIEWER, ReviewState::Commented, Some("sha"))];
            assert!(
                removals(
                    inbox_handled(),
                    linked(vec![commented]),
                    workspace("one", 1)
                )
                .is_empty()
            );
        }

        #[test]
        fn keeps_while_a_review_is_still_owed() {
            let mut owed = reviewed();
            owed.requested_reviewers = vec![RequestedReviewer::user(VIEWER)];
            assert!(removals(on_reviewed(), linked(vec![owed]), workspace("one", 1)).is_empty());
        }

        #[test]
        fn keeps_when_the_newest_review_only_comments() {
            let mut newer = reviewed();
            newer.latest_reviews = vec![Review::new(
                VIEWER,
                ReviewState::Commented,
                Some("newer-sha"),
            )];
            newer.latest_opinionated_reviews = Some(vec![Review::new(
                VIEWER,
                ReviewState::Approved,
                Some("reviewed-sha"),
            )]);
            assert!(removals(on_reviewed(), linked(vec![newer]), workspace("one", 1)).is_empty());
        }

        #[test]
        fn removes_once_you_request_changes() {
            let mut changes = reviewed();
            changes.latest_reviews = vec![Review::new(
                VIEWER,
                ReviewState::ChangesRequested,
                Some("reviewed-sha"),
            )];
            assert_eq!(
                removals(on_reviewed(), linked(vec![changes]), workspace("one", 1)).len(),
                1
            );
        }

        #[test]
        fn keeps_a_workspace_holding_unsent_review_comments() {
            let mut ws = workspace("one", 1);
            ws.has_pending_review_comments = true;
            assert!(removals(on_reviewed(), linked(vec![reviewed()]), ws).is_empty());
        }

        #[test]
        fn keeps_the_ledger_entry_while_still_listed_as_owed() {
            let listed = Snapshot {
                linked: vec![reviewed()],
                review_requested: vec![reviewed()],
                ..snapshot()
            };
            assert_eq!(
                removals(on_reviewed(), listed, workspace("one", 1)),
                vec![("one".to_owned(), true, None)]
            );
        }

        #[test]
        fn leaves_the_workspace_alone_when_off() {
            assert!(removals(json!({}), linked(vec![reviewed()]), workspace("one", 1)).is_empty());
        }

        fn rerequest_inbox(
            subject: icarus_rules::facts::PullRequest,
            handled: &[&str],
        ) -> Vec<(String, Option<String>)> {
            let config =
                json!({ "reviewInbox": { "enabled": true }, "handledPullRequests": handled });
            creations(&run(
                &preset(config),
                vec![],
                Some(inbox(vec![subject])),
                &[],
            ))
        }

        #[test]
        fn re_opens_a_reviewed_pr_on_the_reviewed_commit() {
            let mut subject = reviewed();
            subject.requested_reviewers = vec![RequestedReviewer::user(VIEWER)];
            assert_eq!(
                rerequest_inbox(subject, &[]),
                vec![(
                    "acme/app#1@reviewed-sha".to_owned(),
                    Some("reviewed-sha".to_owned())
                )]
            );
        }

        #[test]
        fn finds_the_reviewed_commit_once_github_dropped_the_review() {
            let mut subject = reviewed();
            subject.latest_opinionated_reviews = Some(subject.latest_reviews.clone());
            subject.latest_reviews = Vec::new();
            subject.requested_reviewers = vec![RequestedReviewer::user(VIEWER)];
            assert_eq!(
                rerequest_inbox(subject, &["acme/app#1"]),
                vec![(
                    "acme/app#1@reviewed-sha".to_owned(),
                    Some("reviewed-sha".to_owned())
                )]
            );
        }

        #[test]
        fn clones_a_re_request_after_a_comment_only_review() {
            let mut subject = reviewed();
            subject.latest_reviews = Vec::new();
            subject.latest_opinionated_reviews = Some(Vec::new());
            subject.viewer_latest_review = Some(ViewerReview {
                state: "COMMENTED".to_owned(),
                commit_oid: Some("commented-sha".to_owned()),
            });
            subject.requested_reviewers = vec![RequestedReviewer::user(VIEWER)];
            assert_eq!(
                rerequest_inbox(subject, &["acme/app#1"])[0].0,
                "acme/app#1@commented-sha"
            );
        }

        #[test]
        fn does_not_clone_the_same_re_request_twice() {
            let mut subject = reviewed();
            subject.requested_reviewers = vec![RequestedReviewer::user(VIEWER)];
            assert!(rerequest_inbox(subject, &["acme/app#1@reviewed-sha"]).is_empty());
        }

        #[test]
        fn opens_a_first_review_on_the_whole_pr() {
            let mut subject = theirs(1, "feat/thing");
            subject.requested_reviewers = vec![RequestedReviewer::user(VIEWER)];
            assert_eq!(
                rerequest_inbox(subject, &[]),
                vec![("acme/app#1".to_owned(), None)]
            );
        }

        #[test]
        fn re_review_uses_the_re_review_prompt() {
            let mut subject = reviewed();
            subject.requested_reviewers = vec![RequestedReviewer::user(VIEWER)];
            let config = json!({ "reviewInbox": { "enabled": true } });
            let result = run(&preset(config), vec![], Some(inbox(vec![subject])), &[]);
            let Some(PlannedAction::CreateWorkspaceFromPr {
                agent: Some(agent), ..
            }) = result.actions.first()
            else {
                panic!("expected a creation");
            };
            assert!(agent.prompt.starts_with("You reviewed pull request #1"));
            assert!(agent.prompt.contains("pointed at reviewed-sha"));
        }
    }
}

mod renderer_apply_workspace_status_rule_plan_test {
    use super::*;

    #[test]
    fn forces_only_the_removals_the_plan_marks_as_forced() {
        let config = json!({ "onResolved": "delete", "onReviewed": "delete" });
        let mut merged = pr(1);
        merged.state = PrState::Merged;
        let mut reviewed = pr(2);
        reviewed.author = Some("colleague".to_owned());
        reviewed.latest_reviews = vec![Review::new(VIEWER, ReviewState::Approved, None)];
        let result = run(
            &preset(config),
            vec![workspace("mine", 1), workspace("theirs", 2)],
            Some(linked(vec![merged, reviewed])),
            &[],
        );
        let forced: Vec<(String, bool)> = deletes(&result)
            .into_iter()
            .map(|(id, force, _)| (id, force))
            .collect();
        assert_eq!(
            forced,
            vec![("mine".to_owned(), false), ("theirs".to_owned(), true)]
        );
    }

    #[test]
    fn ledger_marks_created_forgets_deleted_and_gives_up_after_three_failures() {
        let mut state = EngineState::default();
        state.record(&ActionOutcome::Created {
            handled_key: "acme/app#7@sha".to_owned(),
        });
        state.record(&ActionOutcome::Created {
            handled_key: "acme/app#8".to_owned(),
        });
        state.record(&ActionOutcome::Deleted {
            forget_handled_key: Some("acme/app#7".to_owned()),
        });
        assert_eq!(state.ledger, vec!["acme/app#8".to_owned()]);

        let config = preset(json!({ "reviewInbox": { "enabled": true } }));
        let request = Some(inbox(vec![theirs(9, "feat/nine")]));
        for _ in 0..3 {
            state.record(&ActionOutcome::CreateFailed {
                handled_key: "acme/app#9".to_owned(),
            });
        }
        let result = run_with_state(&config, vec![], request.clone(), &[], &state, 0);
        assert!(creations(&result).is_empty());
        let fresh = run_with_state(&config, vec![], request, &[], &EngineState::default(), 0);
        assert_eq!(creations(&fresh).len(), 1, "a restart retries");
    }

    #[test]
    fn ledger_keeps_the_newest_500() {
        let mut state = EngineState::default();
        for n in 0..510 {
            state.record(&ActionOutcome::Created {
                handled_key: format!("acme/app#{n}"),
            });
        }
        assert_eq!(state.ledger.len(), 500);
        assert_eq!(state.ledger[0], "acme/app#10");
    }
}

mod renderer_launch_conflict_agents_test {
    use super::*;

    fn conflicting() -> icarus_rules::facts::PullRequest {
        let mut subject = pr(3166);
        subject.mergeable = Some(Mergeable::Conflicting);
        subject
    }

    fn config(extra: Value) -> Value {
        merge(
            json!({
                "resolveConflictsWithAgent": true,
                "statusByCondition": { "conflicts": "conflicts-col", "review": "review-col" },
                "reviewInbox": { "agent": "codex" }
            }),
            extra,
        )
    }

    #[test]
    fn starts_an_agent_that_merges_the_base_and_pushes() {
        let result = run(
            &preset(config(json!({}))),
            vec![workspace("one", 3166)],
            Some(linked(vec![conflicting()])),
            &[],
        );
        let launched = launches(&result);
        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].agent.as_deref(), Some("codex"));
        assert_eq!(launched[0].title, "Resolve conflicts #3166");
        assert_eq!(launched[0].launch_source, "conflict_resolution");
        assert!(launched[0].prompt.contains("git fetch origin main"));
        assert!(
            launched[0]
                .prompt
                .contains("push the branch with a plain git push")
        );
        assert!(launched[0].prompt.contains("\"/w/one\""));
    }

    #[test]
    fn uses_the_prompt_from_settings() {
        let custom = config(
            json!({ "conflictPromptTemplate": "Merge {{baseRef}} into {{worktreePath}} for #{{prNumber}}" }),
        );
        let result = run(
            &preset(custom),
            vec![workspace("one", 3166)],
            Some(linked(vec![conflicting()])),
            &[],
        );
        assert_eq!(
            launches(&result)[0].prompt,
            "Merge main into /w/one for #3166"
        );
    }

    #[test]
    fn does_nothing_while_off_for_other_moves_or_when_already_there() {
        let off = run(
            &preset(config(json!({ "resolveConflictsWithAgent": false }))),
            vec![workspace("one", 3166)],
            Some(linked(vec![conflicting()])),
            &[],
        );
        assert!(launches(&off).is_empty());

        let other = run(
            &preset(config(json!({}))),
            vec![workspace("one", 3166)],
            Some(linked(vec![pr(3166)])),
            &[],
        );
        assert!(launches(&other).is_empty());

        let mut already = workspace("one", 3166);
        already.status = Some("conflicts-col".to_owned());
        let stays = run(
            &preset(config(json!({}))),
            vec![already],
            Some(linked(vec![conflicting()])),
            &[],
        );
        assert!(launches(&stays).is_empty());
    }
}

mod renderer_run_review_entered_commands_test {
    use super::*;

    fn commands(result: &icarus_rules::Evaluation) -> Vec<(String, String)> {
        result
            .actions
            .iter()
            .filter_map(|action| match action {
                PlannedAction::RunCommand {
                    workspace_id,
                    command,
                    timeout_ms,
                    ..
                } => {
                    assert_eq!(*timeout_ms, 300_000);
                    Some((workspace_id.clone(), command.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn config(command: &str) -> Value {
        json!({
            "reviewEnteredCommand": command,
            "statusByCondition": { "review": "review-col", "draft": "draft-col" }
        })
    }

    #[test]
    fn runs_the_command_when_the_pr_enters_review() {
        let result = run(
            &preset(config("  make dev-down  ")),
            vec![workspace("one", 1)],
            Some(linked(vec![pr(1)])),
            &[],
        );
        assert_eq!(
            commands(&result),
            vec![("one".to_owned(), "make dev-down".to_owned())]
        );
    }

    #[test]
    fn does_nothing_without_a_command_for_other_moves_or_on_another_host() {
        let none = run(
            &preset(config("")),
            vec![workspace("one", 1)],
            Some(linked(vec![pr(1)])),
            &[],
        );
        assert!(commands(&none).is_empty());
        let other = run(
            &preset(config("make dev-down")),
            vec![workspace("one", 1)],
            Some(linked(vec![draft()])),
            &[],
        );
        assert!(commands(&other).is_empty());
        let mut remote = workspace("one", 1);
        remote.execution_host = "ssh-box".to_owned();
        let remote = run(
            &preset(config("make dev-down")),
            vec![remote],
            Some(linked(vec![pr(1)])),
            &[],
        );
        assert!(commands(&remote).is_empty());
    }

    #[test]
    fn side_effects_follow_status_updates_in_orca_order() {
        let mut conflicting = pr(2);
        conflicting.mergeable = Some(Mergeable::Conflicting);
        let config = json!({
            "reviewEnteredCommand": "make dev-down",
            "resolveConflictsWithAgent": true,
            "statusByCondition": { "review": "review-col", "conflicts": "conflicts-col" }
        });
        let result = run(
            &preset(config),
            vec![workspace("one", 1), workspace("two", 2)],
            Some(linked(vec![pr(1), conflicting])),
            &[],
        );
        let kinds: Vec<&str> = result
            .actions
            .iter()
            .map(|action| match action {
                PlannedAction::SetColumn { .. } => "set",
                PlannedAction::LaunchAgent { .. } => "agent",
                PlannedAction::RunCommand { .. } => "command",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, vec!["set", "set", "agent", "command"]);
    }
}

mod shared_workspace_new_status_plan_test {
    use super::*;

    fn config() -> Value {
        json!({ "newWorkspaceStatus": "todo", "newWorkspaceStatusSince": 1000 })
    }

    fn fresh(id: &str, created_at: Option<u64>, status: Option<&str>) -> Workspace {
        Workspace {
            id: id.to_owned(),
            repo_id: "repo-1".to_owned(),
            status: status.map(str::to_owned),
            created_at,
            ..Workspace::default()
        }
    }

    #[test]
    fn starts_a_workspace_created_after_the_setting_in_the_chosen_column() {
        let result = run(
            &preset(config()),
            vec![fresh("wt-1", Some(2000), None)],
            None,
            &[],
        );
        assert_eq!(
            set_columns(&result),
            vec![("wt-1".to_owned(), "todo".to_owned(), None)]
        );
    }

    #[test]
    fn leaves_older_undated_and_already_placed_workspaces_alone() {
        let result = run(
            &preset(config()),
            vec![
                fresh("old", Some(500), None),
                fresh("found", None, None),
                fresh("placed", Some(2000), Some("status-6")),
            ],
            None,
            &[],
        );
        assert!(set_columns(&result).is_empty());
    }

    #[test]
    fn does_nothing_until_a_column_is_chosen() {
        let result = run(
            &preset(json!({ "newWorkspaceStatusSince": 1000 })),
            vec![fresh("wt-1", Some(2000), None)],
            None,
            &[],
        );
        assert!(set_columns(&result).is_empty());
    }
}

mod shared_github_review_head_sync_test {
    use super::*;

    fn moved() -> icarus_rules::facts::PullRequest {
        let mut subject = theirs(3179, "feature/e-1");
        subject.head_ref_oid = "new-head".to_owned();
        subject
    }

    fn candidate() -> Workspace {
        let mut ws = workspace("wt", 3179);
        ws.branch = Some("feature/e-1".to_owned());
        ws.head_oid = "old-head".to_owned();
        ws
    }

    fn plan(subject: icarus_rules::facts::PullRequest, ws: Workspace) -> Vec<(String, String)> {
        syncs(&run(
            &preset(json!({})),
            vec![ws],
            Some(linked(vec![subject])),
            &[],
        ))
    }

    #[test]
    fn syncs_a_review_workspace_whose_pr_moved() {
        assert_eq!(
            plan(moved(), candidate()),
            vec![("wt".to_owned(), "new-head".to_owned())]
        );
    }

    #[test]
    fn never_touches_your_own_prs() {
        let mut mine = moved();
        mine.author = Some("ME".to_owned());
        assert!(plan(mine, candidate()).is_empty());
    }

    #[test]
    fn skips_current_closed_remote_and_other_branch() {
        let mut current = moved();
        current.head_ref_oid = "old-head".to_owned();
        assert!(plan(current, candidate()).is_empty());

        let mut merged = moved();
        merged.state = PrState::Merged;
        assert!(plan(merged, candidate()).is_empty());

        let mut remote = candidate();
        remote.repo_is_remote = true;
        assert!(plan(moved(), remote).is_empty());

        let mut renamed = moved();
        renamed.head_ref_name = "renamed".to_owned();
        assert!(plan(renamed, candidate()).is_empty());
    }

    #[test]
    fn needs_a_known_viewer() {
        let snapshot = Snapshot {
            viewer_login: None,
            linked: vec![moved()],
            ..Snapshot::default()
        };
        let result = run(&preset(json!({})), vec![candidate()], Some(snapshot), &[]);
        assert!(syncs(&result).is_empty());
    }
}
