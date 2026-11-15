mod common;

use common::*;
use icarus_rules::PlannedAction;
use icarus_rules::engine::Event;
use icarus_rules::facts::{
    AutoMergeRequest, CheckState, MergeStateStatus as M, Mergeable, PrLifecycle, ReviewDecision,
    Stack, StackAutoMerge, StackAutoMergeMode, StackEntry,
};
use icarus_rules::model::PrOperation;
use icarus_rules::next_action::*;
use serde_json::json;

fn clean() -> ActionInput {
    ActionInput {
        has_uncommitted_changes: false,
        has_upstream: true,
        unpushed_commits: 0,
        has_local_conflicts: false,
        is_default_branch: false,
        pull_request: None,
        pull_request_known: true,
    }
}

fn with_pr(
    state: PrLifecycle,
    conflicting: bool,
    merge: Option<MergeStep>,
    head_is_local: bool,
) -> ActionInput {
    ActionInput {
        pull_request: Some(ActionPr {
            state,
            conflicting,
            merge,
            head_is_local,
        }),
        ..clean()
    }
}

mod shared_workspace_action_test {
    use super::*;

    #[test]
    fn create_pr_over_commit_and_push_without_a_pr() {
        let dirty = ActionInput {
            has_uncommitted_changes: true,
            ..clean()
        };
        assert_eq!(
            resolve_next_action(&dirty),
            Some(NextAction::CreatePullRequest)
        );
        let ahead = ActionInput {
            unpushed_commits: 2,
            ..clean()
        };
        assert_eq!(
            resolve_next_action(&ahead),
            Some(NextAction::CreatePullRequest)
        );
    }

    #[test]
    fn commit_and_push_until_the_pr_lookup_answers() {
        let unknown = ActionInput {
            pull_request_known: false,
            ..clean()
        };
        assert_eq!(
            resolve_next_action(&ActionInput {
                has_uncommitted_changes: true,
                ..unknown.clone()
            }),
            Some(NextAction::CommitAndPush)
        );
        assert_eq!(resolve_next_action(&unknown), None);
    }

    #[test]
    fn commit_and_push_over_the_pr_steps() {
        let base = with_pr(PrLifecycle::Draft, true, None, false);
        assert_eq!(
            resolve_next_action(&ActionInput {
                has_uncommitted_changes: true,
                ..base.clone()
            }),
            Some(NextAction::CommitAndPush)
        );
        assert_eq!(
            resolve_next_action(&ActionInput {
                unpushed_commits: 2,
                ..base
            }),
            Some(NextAction::CommitAndPush)
        );
    }

    #[test]
    fn conflicts_then_ready_for_review_then_create_pr() {
        let table = [
            (
                with_pr(PrLifecycle::Draft, true, None, false),
                Some(NextAction::ResolveConflicts),
            ),
            (
                with_pr(PrLifecycle::Draft, false, None, false),
                Some(NextAction::ReadyForReview),
            ),
            (clean(), Some(NextAction::CreatePullRequest)),
            (
                ActionInput {
                    has_local_conflicts: true,
                    has_uncommitted_changes: true,
                    ..clean()
                },
                Some(NextAction::ResolveConflicts),
            ),
            (with_pr(PrLifecycle::Open, false, None, false), None),
            (with_pr(PrLifecycle::Merged, false, None, false), None),
            (
                ActionInput {
                    is_default_branch: true,
                    ..clean()
                },
                None,
            ),
            (
                ActionInput {
                    has_upstream: false,
                    unpushed_commits: 3,
                    ..clean()
                },
                Some(NextAction::CreatePullRequest),
            ),
        ];
        for (input, expected) in table {
            assert_eq!(resolve_next_action(&input), expected, "{input:?}");
        }
    }

    #[test]
    fn merges_or_toggles_auto_merge_once_the_head_is_local() {
        for step in [
            MergeStep::Merge,
            MergeStep::EnableAutoMerge,
            MergeStep::DisableAutoMerge,
        ] {
            assert_eq!(
                resolve_next_action(&with_pr(PrLifecycle::Open, false, Some(step), true)),
                Some(NextAction::Merge(step))
            );
        }
        assert_eq!(
            resolve_next_action(&with_pr(PrLifecycle::Open, false, None, true)),
            None
        );
    }

    #[test]
    fn never_merges_before_local_work_is_on_the_pr() {
        let open = with_pr(PrLifecycle::Open, false, Some(MergeStep::Merge), true);
        assert_eq!(
            resolve_next_action(&ActionInput {
                has_uncommitted_changes: true,
                ..open.clone()
            }),
            Some(NextAction::CommitAndPush)
        );
        assert_eq!(
            resolve_next_action(&ActionInput {
                unpushed_commits: 1,
                ..open.clone()
            }),
            Some(NextAction::CommitAndPush)
        );
        assert_eq!(
            resolve_next_action(&with_pr(
                PrLifecycle::Open,
                false,
                Some(MergeStep::Merge),
                false
            )),
            None
        );
        assert_eq!(
            resolve_next_action(&with_pr(
                PrLifecycle::Draft,
                false,
                Some(MergeStep::Merge),
                true
            )),
            Some(NextAction::ReadyForReview)
        );
    }
}

fn input() -> MergeStateInput {
    MergeStateInput {
        state: Some(PrLifecycle::Open),
        mergeable: Some(Mergeable::Mergeable),
        merge_state_status: Some(M::Clean),
        checks: Some(CheckState::Success),
        auto_merge_allowed: Some(true),
        ..MergeStateInput::default()
    }
}

mod shared_github_pull_request_auto_merge_availability_test {
    use super::*;

    #[test]
    fn does_not_infer_a_requirement_from_unresolved_status() {
        for status in [None, Some(M::Unknown)] {
            let item = MergeStateInput {
                mergeable: Some(Mergeable::Unknown),
                merge_state_status: status,
                ..input()
            };
            assert!(!can_show_auto_merge_control(&item));
        }
        assert!(!can_show_auto_merge_control(&input()));
    }

    #[test]
    fn offers_auto_merge_for_requirement_blocked_prs() {
        let cases = [
            MergeStateInput {
                mergeable: Some(Mergeable::Unknown),
                merge_state_status: Some(M::Blocked),
                review_decision: Some(ReviewDecision::ReviewRequired),
                ..input()
            },
            MergeStateInput {
                merge_state_status: Some(M::Blocked),
                ..input()
            },
            MergeStateInput {
                merge_state_status: Some(M::Behind),
                ..input()
            },
            MergeStateInput {
                mergeable: Some(Mergeable::Unknown),
                merge_state_status: Some(M::Blocked),
                ..input()
            },
            MergeStateInput {
                merge_queue_required: Some(true),
                ..input()
            },
            MergeStateInput {
                auto_merge_allowed: Some(false),
                merge_queue_required: Some(true),
                ..input()
            },
            MergeStateInput {
                merge_state_status: Some(M::Unstable),
                merge_queue_required: Some(true),
                ..input()
            },
            MergeStateInput {
                auto_merge_enabled: true,
                ..input()
            },
            MergeStateInput {
                auto_merge_enabled: true,
                auto_merge_allowed: Some(false),
                mergeable: Some(Mergeable::Conflicting),
                merge_state_status: Some(M::Dirty),
                ..input()
            },
        ];
        for item in cases {
            assert!(can_show_auto_merge_control(&item), "{item:?}");
        }
    }

    #[test]
    fn suppresses_closed_draft_disallowed_conflicting_and_unstable() {
        let cases = [
            MergeStateInput {
                state: Some(PrLifecycle::Draft),
                merge_state_status: Some(M::Blocked),
                ..input()
            },
            MergeStateInput {
                state: Some(PrLifecycle::Closed),
                auto_merge_enabled: true,
                ..input()
            },
            MergeStateInput {
                auto_merge_allowed: Some(false),
                mergeable: Some(Mergeable::Unknown),
                merge_state_status: Some(M::Blocked),
                ..input()
            },
            MergeStateInput {
                mergeable: Some(Mergeable::Conflicting),
                merge_state_status: Some(M::Dirty),
                ..input()
            },
            MergeStateInput {
                mergeable: Some(Mergeable::Unknown),
                merge_state_status: Some(M::Unstable),
                ..input()
            },
        ];
        for item in cases {
            assert!(!can_show_auto_merge_control(&item), "{item:?}");
        }
    }
}

mod renderer_github_pr_merge_state_test {
    use super::*;

    fn decide(item: MergeStateInput) -> (MergeStateKind, bool, Option<AutoMergeIntent>) {
        let decision = resolve_merge_state(&item);
        (
            decision.kind,
            decision.direct_merge_available,
            decision.auto_merge,
        )
    }

    #[test]
    fn merge_state_table() {
        use AutoMergeIntent as A;
        use MergeStateKind as K;
        let table = [
            (
                MergeStateInput {
                    review_decision: Some(ReviewDecision::ReviewRequired),
                    ..input()
                },
                (K::ReviewRequired, false, Some(A::EnableAutoMerge)),
            ),
            (
                MergeStateInput {
                    review_decision: Some(ReviewDecision::ChangesRequested),
                    ..input()
                },
                (K::ChangesRequested, false, Some(A::EnableAutoMerge)),
            ),
            (
                MergeStateInput {
                    merge_queue_required: Some(true),
                    in_merge_queue: true,
                    review_decision: Some(ReviewDecision::ReviewRequired),
                    ..input()
                },
                (K::InMergeQueue, false, None),
            ),
            (
                MergeStateInput {
                    merge_queue_required: Some(true),
                    ..input()
                },
                (K::MergeQueue, false, Some(A::MergeWhenReady)),
            ),
            (
                MergeStateInput {
                    merge_queue_required: Some(true),
                    mergeable: Some(Mergeable::Unknown),
                    merge_state_status: Some(M::Dirty),
                    ..input()
                },
                (K::Conflicts, false, None),
            ),
            (
                MergeStateInput {
                    checks: Some(CheckState::Pending),
                    ..input()
                },
                (K::MergeableChecksPending, true, None),
            ),
            (
                MergeStateInput {
                    mergeable: Some(Mergeable::Unknown),
                    merge_state_status: Some(M::Unstable),
                    checks: Some(CheckState::Failure),
                    ..input()
                },
                (K::MergeableChecksFailed, true, None),
            ),
            (
                MergeStateInput {
                    mergeable: Some(Mergeable::Unknown),
                    merge_state_status: Some(M::HasHooks),
                    checks: Some(CheckState::Failure),
                    ..input()
                },
                (K::MergeableChecksFailed, true, None),
            ),
            (
                MergeStateInput {
                    state: Some(PrLifecycle::Open),
                    checks: Some(CheckState::Success),
                    ..MergeStateInput::default()
                },
                (K::ChecksPassed, true, None),
            ),
            (
                MergeStateInput {
                    auto_merge_enabled: true,
                    ..input()
                },
                (K::MergeableChecksPassed, true, Some(A::DisableAutoMerge)),
            ),
            (
                MergeStateInput {
                    merge_state_status: Some(M::Behind),
                    ..input()
                },
                (K::Behind, false, Some(A::EnableAutoMerge)),
            ),
            (
                MergeStateInput {
                    checks: Some(CheckState::Failure),
                    ..input()
                },
                (K::MergeableChecksFailed, true, None),
            ),
            (input(), (K::MergeableChecksPassed, true, None)),
            (
                MergeStateInput {
                    mergeable: Some(Mergeable::Unknown),
                    merge_state_status: None,
                    checks: Some(CheckState::Pending),
                    ..input()
                },
                (K::Checking, false, None),
            ),
            (
                MergeStateInput {
                    mergeable: Some(Mergeable::Unknown),
                    merge_state_status: None,
                    checks: Some(CheckState::Success),
                    ..input()
                },
                (K::ChecksPassed, true, None),
            ),
            (
                MergeStateInput {
                    review_decision: Some(ReviewDecision::ReviewRequired),
                    merge_state_status: Some(M::Blocked),
                    ..input()
                },
                (K::ReviewRequired, false, Some(A::EnableAutoMerge)),
            ),
            (
                MergeStateInput {
                    auto_merge_allowed: None,
                    mergeable: Some(Mergeable::Unknown),
                    merge_state_status: Some(M::Blocked),
                    ..input()
                },
                (K::Blocked, false, Some(A::EnableAutoMerge)),
            ),
            (
                MergeStateInput {
                    state: Some(PrLifecycle::Closed),
                    merge_queue_required: Some(true),
                    ..input()
                },
                (K::Closed, false, None),
            ),
            (
                MergeStateInput {
                    state: Some(PrLifecycle::Merged),
                    auto_merge_enabled: true,
                    ..input()
                },
                (K::Merged, false, None),
            ),
            (
                MergeStateInput {
                    state: Some(PrLifecycle::Draft),
                    merge_queue_required: Some(true),
                    ..input()
                },
                (K::Draft, false, None),
            ),
        ];
        for (item, expected) in table {
            assert_eq!(decide(item.clone()), expected, "{item:?}");
        }
    }
}

fn entry(position: u32, number: u64, state: PrLifecycle, status: Option<M>) -> StackEntry {
    StackEntry {
        position,
        number,
        state,
        mergeable: Some(Mergeable::Mergeable),
        merge_state_status: status,
        review_decision: None,
        in_merge_queue: false,
        head_oid: None,
        title: None,
        head_ref_name: None,
        url: None,
        is_draft: false,
        checks_rollup: None,
        author: None,
        author_avatar_url: None,
    }
}

fn stack(position: u32, entries: Vec<StackEntry>) -> Stack {
    Stack {
        number: 4,
        position,
        size: entries.len() as u32,
        base_ref_name: "main".to_owned(),
        entries,
        merge_queue_required: None,
        auto_merge: None,
    }
}

mod renderer_github_pr_stack_merge_target {
    use super::*;

    #[test]
    fn targets_the_top_unmerged_entry_once_every_one_is_ready() {
        let ready = stack(
            1,
            vec![
                entry(1, 10, PrLifecycle::Merged, None),
                entry(2, 11, PrLifecycle::Open, Some(M::Clean)),
                entry(3, 12, PrLifecycle::Open, Some(M::Unstable)),
            ],
        );
        assert_eq!(
            stack_merge_target(&ready),
            Some(StackMergeTarget {
                number: 12,
                position: 3,
                count: 2
            })
        );
    }

    #[test]
    fn hides_for_incomplete_queued_draft_or_unready_entries() {
        let mut incomplete = stack(1, vec![entry(1, 10, PrLifecycle::Open, Some(M::Clean))]);
        incomplete.size = 2;
        assert_eq!(stack_merge_target(&incomplete), None);

        let mut queued = entry(1, 10, PrLifecycle::Open, Some(M::Clean));
        queued.in_merge_queue = true;
        assert_eq!(stack_merge_target(&stack(1, vec![queued])), None);
        assert_eq!(
            stack_merge_target(&stack(
                1,
                vec![entry(1, 10, PrLifecycle::Draft, Some(M::Clean))]
            )),
            None
        );
        assert_eq!(
            stack_merge_target(&stack(
                1,
                vec![entry(1, 10, PrLifecycle::Open, Some(M::Blocked))]
            )),
            None
        );
        assert_eq!(
            stack_merge_target(&stack(1, vec![entry(1, 10, PrLifecycle::Merged, None)])),
            None
        );
    }

    #[test]
    fn blocker_is_the_first_from_the_bottom_up_to_this_pr() {
        let mut review = entry(2, 11, PrLifecycle::Open, Some(M::Blocked));
        review.review_decision = Some(ReviewDecision::ReviewRequired);
        let blocked = stack(
            2,
            vec![
                entry(1, 10, PrLifecycle::Open, Some(M::Behind)),
                review,
                entry(3, 12, PrLifecycle::Draft, None),
            ],
        );
        assert_eq!(
            stack_merge_blocker(&blocked),
            Some(StackBlocker {
                reason: StackBlockerReason::Behind,
                pr_number: 10
            })
        );
    }
}

mod next_action_merge_steps {
    use super::*;

    fn open_pr() -> icarus_rules::facts::PullRequest {
        let mut subject = pr(5);
        subject.mergeable = Some(Mergeable::Mergeable);
        subject.merge_state_status = Some(M::Clean);
        subject.checks_rollup = Some(CheckState::Success);
        subject.auto_merge_allowed = Some(true);
        subject
    }

    #[test]
    fn single_pr_steps() {
        assert_eq!(merge_step(&open_pr()), Some(MergeStep::Merge));
        let mut queue = open_pr();
        queue.merge_queue_required = Some(true);
        assert_eq!(merge_step(&queue), Some(MergeStep::AddToMergeQueue));
        let mut blocked = open_pr();
        blocked.merge_state_status = Some(M::Blocked);
        assert_eq!(merge_step(&blocked), Some(MergeStep::EnableAutoMerge));
        blocked.auto_merge = Some(AutoMergeRequest {
            method: None,
            enabled_by: Some(VIEWER.to_owned()),
        });
        assert_eq!(merge_step(&blocked), Some(MergeStep::DisableAutoMerge));
        let mut queued = queue.clone();
        queued.is_in_merge_queue = true;
        assert_eq!(merge_step(&queued), None);
        let mut draft = open_pr();
        draft.is_draft = true;
        assert_eq!(merge_step(&draft), None);
    }

    #[test]
    fn stacked_pr_steps() {
        let mut ready = open_pr();
        ready.stack = Some(stack(
            1,
            vec![
                entry(1, 5, PrLifecycle::Open, Some(M::Clean)),
                entry(2, 6, PrLifecycle::Open, Some(M::Clean)),
            ],
        ));
        assert_eq!(
            merge_step(&ready),
            Some(MergeStep::MergeStack {
                target: 6,
                count: 2,
                via_merge_queue: false
            })
        );

        let mut waiting = open_pr();
        waiting.stack = Some(stack(
            1,
            vec![
                entry(1, 5, PrLifecycle::Open, Some(M::Clean)),
                entry(2, 6, PrLifecycle::Open, Some(M::Blocked)),
            ],
        ));
        assert_eq!(merge_step(&waiting), Some(MergeStep::EnableStackAutoMerge));

        let mut queue = waiting.clone();
        queue.merge_queue_required = Some(true);
        if let Some(stack) = ready.stack.as_mut() {
            stack.merge_queue_required = Some(true);
        }
        assert_eq!(
            merge_step(&ready),
            Some(MergeStep::MergeStack {
                target: 6,
                count: 2,
                via_merge_queue: true
            })
        );

        if let Some(stack) = waiting.stack.as_mut() {
            stack.auto_merge = Some(StackAutoMerge {
                mode: StackAutoMergeMode::AppManaged,
                enabled_by: None,
            });
        }
        assert_eq!(merge_step(&waiting), Some(MergeStep::DisableStackAutoMerge));

        let mut drafty = open_pr();
        drafty.stack = Some(stack(
            1,
            vec![
                entry(1, 5, PrLifecycle::Open, Some(M::Clean)),
                entry(2, 6, PrLifecycle::Draft, None),
            ],
        ));
        assert_eq!(merge_step(&drafty), None);
    }

    #[test]
    fn next_action_from_facts_requires_the_local_head() {
        let git = LocalGitState {
            has_upstream: true,
            head_oid: Some("abc".to_owned()),
            ..LocalGitState::default()
        };
        assert_eq!(
            next_action(&git, Some(&open_pr()), true),
            Some(NextAction::Merge(MergeStep::Merge))
        );
        let behind = LocalGitState {
            head_oid: Some("old".to_owned()),
            ..git.clone()
        };
        assert_eq!(next_action(&behind, Some(&open_pr()), true), None);
        let mut conflicting = open_pr();
        conflicting.mergeable = Some(Mergeable::Conflicting);
        assert_eq!(
            next_action(&git, Some(&conflicting), true),
            Some(NextAction::ResolveConflicts)
        );
        assert_eq!(
            next_action(&git, None, true),
            Some(NextAction::CreatePullRequest)
        );
    }
}

mod renderer_launch_workspace_action_test {
    use super::*;

    fn trigger(action: &str, vars: &[(&str, &str)]) -> icarus_rules::Evaluation {
        let preset = preset(json!({
            "reviewInbox": { "agent": "codex" },
            "actionPrompts": { "reviewPullRequest": "Mine for #{{prNumber}} {{ignored }}" }
        }));
        let event = Event::UserAction {
            workspace_id: "one".to_owned(),
            action: action.to_owned(),
            variables: vars
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        };
        run(
            &preset,
            vec![workspace("one", 5)],
            Some(snapshot_with(pr(5))),
            &[event],
        )
    }

    fn snapshot_with(subject: icarus_rules::facts::PullRequest) -> icarus_rules::facts::Snapshot {
        icarus_rules::facts::Snapshot {
            linked: vec![subject],
            ..snapshot()
        }
    }

    #[test]
    fn creates_the_pr_with_the_settings_and_instructions() {
        let result = trigger(
            "create-pull-request",
            &[
                ("draftMode", "as a draft"),
                ("templateRule", "Use the template."),
                ("prInstructions", ""),
            ],
        );
        let launched = launches(&result);
        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].agent.as_deref(), Some("codex"));
        assert_eq!(launched[0].title, "Create PR");
        assert!(
            launched[0]
                .prompt
                .contains("Create it as a draft with gh pr create.")
        );
        assert!(launched[0].prompt.contains("in /w/one, targeting main"));
    }

    #[test]
    fn merges_and_toggles_auto_merge_without_an_agent() {
        for (action, operation) in [
            ("merge", PrOperation::Merge),
            ("enable-auto-merge", PrOperation::EnableAutoMerge),
            ("disable-auto-merge", PrOperation::DisableAutoMerge),
            ("add-to-merge-queue", PrOperation::AddToMergeQueue),
        ] {
            let result = trigger(action, &[]);
            assert!(launches(&result).is_empty());
            assert!(matches!(
                result.actions.as_slice(),
                [PlannedAction::PullRequest { operation: op, .. }] if *op == operation
            ));
        }
    }

    #[test]
    fn resolves_conflicts_with_the_provided_prompt_and_reviews_with_the_saved_one() {
        let conflicts = trigger("resolve-conflicts", &[("prompt", "Resolve it.")]);
        assert_eq!(launches(&conflicts)[0].prompt, "Resolve it.");
        assert_eq!(launches(&conflicts)[0].title, "Resolve conflicts");
        let review = trigger("review-pull-request", &[]);
        assert_eq!(launches(&review)[0].prompt, "Mine for #5 {{ignored }}");
        assert_eq!(launches(&review)[0].title, "Review #5");
    }

    #[test]
    fn does_nothing_for_an_unknown_action() {
        assert!(trigger("deploy", &[]).actions.is_empty());
    }
}
