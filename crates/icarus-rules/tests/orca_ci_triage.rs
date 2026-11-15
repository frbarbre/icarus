mod common;

use common::*;
use icarus_rules::ci_triage::*;
use icarus_rules::engine::Event;
use icarus_rules::facts::{
    Check, CheckConclusion as C, CheckRunStatus as S, CheckState, PrLifecycle, Workspace,
};
use icarus_rules::orca_preset::{OrcaPreset, OrcaPresetInput, build};
use icarus_rules::prompts::*;
use icarus_rules::{EngineState, Evaluation};
use serde_json::json;

fn failed(name: &str, url: Option<&str>) -> Check {
    Check::run(name, S::Completed, Some(C::Failure), url)
}

mod shared_ci_triage_test {
    use super::*;

    #[test]
    fn parse_keeps_an_edited_prompt_and_stores_default_or_blank_as_none() {
        let parse = |value| parse_ci_triage_config(&value);
        assert_eq!(
            parse(json!({ "prompt": "  Rerun everything once.  " })),
            Some(CiTriageConfig {
                prompt: Some("Rerun everything once.".to_owned()),
                hints: None
            })
        );
        assert_eq!(
            parse(json!({ "prompt": "" })),
            Some(CiTriageConfig::default())
        );
        assert_eq!(
            parse(json!({ "prompt": DEFAULT_CI_TRIAGE_INSTRUCTIONS })),
            Some(CiTriageConfig::default())
        );
        assert_eq!(parse(json!({})), Some(CiTriageConfig::default()));
        assert_eq!(
            parse(json!({ "hints": "Errors are tracked at errors.example.test." })),
            Some(CiTriageConfig {
                prompt: Some(format!(
                    "{DEFAULT_CI_TRIAGE_INSTRUCTIONS}\n\nAbout this repository:\nErrors are tracked at errors.example.test."
                )),
                hints: None
            })
        );
        assert_eq!(
            parse(json!({ "hints": "" })),
            Some(CiTriageConfig::default())
        );
        assert_eq!(parse(serde_json::Value::Null), None);
        assert_eq!(parse(json!("on")), None);
    }

    #[test]
    fn instructions_are_the_prompt_else_default_with_hints_else_default() {
        let prompt = CiTriageConfig {
            prompt: Some("Rerun everything once.".to_owned()),
            hints: None,
        };
        assert_eq!(ci_triage_instructions(&prompt), "Rerun everything once.");
        let hints = CiTriageConfig {
            prompt: None,
            hints: Some("Use the staging tracker.".to_owned()),
        };
        assert!(
            ci_triage_instructions(&hints)
                .contains("About this repository:\nUse the staging tracker.")
        );
        assert_eq!(
            ci_triage_instructions(&CiTriageConfig::default()),
            DEFAULT_CI_TRIAGE_INSTRUCTIONS
        );
    }

    #[test]
    fn names_completed_failed_checks_in_a_stable_order() {
        let checks = vec![
            Check::run("lint", S::Completed, Some(C::Success), None),
            failed("checks", Some("https://x/runs/7")),
            Check::run("build", S::Completed, Some(C::TimedOut), None),
            Check::run("e2e", S::InProgress, None, None),
            Check::run("deploy", S::Waiting, None, None),
        ];
        assert_eq!(
            failed_checks_to_triage(&checks),
            vec![
                TriagedCheck {
                    name: "build".to_owned(),
                    url: None
                },
                TriagedCheck {
                    name: "checks".to_owned(),
                    url: Some("https://x/runs/7".to_owned())
                },
            ]
        );
    }

    #[test]
    fn default_instructions_cover_every_case() {
        for needle in [
            "gh run rerun <run id> --failed",
            "run_attempt",
            "git merge origin/main",
            "a test the change broke",
            "git push",
            "Never force-push",
            "Fixed: <what>",
        ] {
            assert!(DEFAULT_CI_TRIAGE_INSTRUCTIONS.contains(needle), "{needle}");
        }
    }

    #[test]
    fn prompt_names_the_pr_commit_and_failed_checks() {
        assert_eq!(
            ci_triage_prompt(
                12,
                Some("abc1234"),
                &[TriagedCheck {
                    name: "checks".to_owned(),
                    url: Some("https://github.com/acme/app/actions/runs/7".to_owned())
                }],
                "Rerun everything once."
            ),
            "CI failed on pull request #12 at abc1234. Failed checks:\n- checks: https://github.com/acme/app/actions/runs/7\n\nRerun everything once."
        );
    }
}

mod renderer_ci_triage_trigger_test {
    use super::*;

    fn worktrees() -> Vec<Workspace> {
        ["feature", "other"]
            .into_iter()
            .map(|branch| Workspace {
                id: format!("wt-{branch}"),
                repo_id: "repo-1".to_owned(),
                branch: Some(format!("refs/heads/{branch}")),
                ..Workspace::default()
            })
            .collect()
    }

    fn triage_preset() -> OrcaPreset {
        build(&OrcaPresetInput {
            workspace_status_rules: json!({}),
            ci_triage: vec![(
                "repo-1".to_owned(),
                json!({ "prompt": "Errors are tracked at errors.example.test." }),
            )],
            ..OrcaPresetInput::default()
        })
    }

    fn observed(
        pr_number: u64,
        branch: &str,
        sha: &str,
        checks: Vec<Check>,
        author: Option<bool>,
    ) -> Event {
        Event::ChecksObserved(ChecksObserved {
            repo_id: "repo-1".to_owned(),
            pr_number,
            branch: branch.to_owned(),
            head_sha: Some(sha.to_owned()),
            checks,
            viewer_is_author: author,
            trigger: "watch".to_owned(),
        })
    }

    struct Session {
        preset: OrcaPreset,
        state: EngineState,
        launched: Vec<icarus_rules::engine::AgentLaunch>,
        last: Option<Evaluation>,
    }

    impl Session {
        fn new() -> Self {
            let preset = triage_preset();
            let state = preset.state.clone();
            Self {
                preset,
                state,
                launched: Vec::new(),
                last: None,
            }
        }

        fn observe(&mut self, event: Event) {
            let result = run_with_state(
                &self.preset,
                worktrees(),
                None,
                &[event],
                &self.state,
                1_000,
            );
            self.launched.extend(launches(&result));
            self.state = result.state.clone();
            self.last = Some(result);
        }

        fn decision(&self) -> Option<CiTriageDecision> {
            self.last
                .as_ref()?
                .ci_triage
                .first()
                .map(|record| record.outcome.decision)
        }
    }

    fn failing() -> Vec<Check> {
        vec![failed("checks", Some("https://x/runs/7"))]
    }

    #[test]
    fn starts_one_background_agent_per_failure_in_the_prs_workspace() {
        let mut session = Session::new();
        session.observe(observed(12, "feature", "sha-a", failing(), Some(true)));
        session.observe(observed(12, "feature", "sha-a", failing(), Some(true)));
        assert_eq!(session.launched.len(), 1);
        let launch = &session.launched[0];
        assert!(launch.background);
        assert_eq!(launch.launch_source, "source_control_recovery");
        assert!(
            launch
                .prompt
                .contains("Errors are tracked at errors.example.test.")
        );
        assert!(
            launch
                .prompt
                .starts_with("CI failed on pull request #12 at sha-a.")
        );
        let started = session.last.as_ref().map(|result| result.actions.len());
        assert_eq!(started, Some(0));
    }

    #[test]
    fn leaves_a_pr_you_are_reviewing_or_cannot_tell() {
        let mut session = Session::new();
        session.observe(observed(
            30,
            "feature",
            "sha-review",
            failing(),
            Some(false),
        ));
        assert_eq!(session.decision(), Some(CiTriageDecision::NotAuthor));
        session.observe(observed(31, "feature", "sha-offline", failing(), None));
        assert!(session.launched.is_empty());
    }

    #[test]
    fn memory_is_spent_before_the_author_check() {
        let mut session = Session::new();
        session.observe(observed(32, "feature", "sha", failing(), Some(false)));
        session.observe(observed(32, "feature", "sha", failing(), Some(true)));
        assert_eq!(session.decision(), Some(CiTriageDecision::AlreadyTriaged));
        assert!(session.launched.is_empty());
    }

    #[test]
    fn a_failed_run_reappearing_under_a_new_head_is_left_alone() {
        let mut session = Session::new();
        session.observe(observed(40, "feature", "before-fix", failing(), Some(true)));
        session.observe(observed(40, "feature", "after-fix", failing(), Some(true)));
        assert_eq!(session.launched.len(), 1);
    }

    #[test]
    fn waits_while_the_failed_check_runs_again_but_not_for_unrelated_ones() {
        let mut session = Session::new();
        let mut rerun = failing();
        rerun.push(Check::run(
            "checks",
            S::InProgress,
            None,
            Some("https://x/runs/8"),
        ));
        session.observe(observed(41, "feature", "sha", rerun, Some(true)));
        assert_eq!(session.decision(), Some(CiTriageDecision::Rerunning));

        let mut waiting = failing();
        waiting.push(Check::run(
            "checks",
            S::Waiting,
            None,
            Some("https://x/runs/9"),
        ));
        session.observe(observed(41, "feature", "sha", waiting, Some(true)));
        assert_eq!(session.decision(), Some(CiTriageDecision::Rerunning));

        session.observe(observed(
            42,
            "feature",
            "sha-early",
            vec![
                failed("lint", Some("https://x/runs/9")),
                Check::run("e2e", S::InProgress, None, Some("https://x/runs/10")),
            ],
            Some(true),
        ));
        assert_eq!(session.launched.len(), 1);
    }

    #[test]
    fn a_later_failure_gets_only_the_runs_not_yet_triaged() {
        let mut session = Session::new();
        let lint = failed("lint", Some("https://x/l"));
        let e2e = failed("e2e", Some("https://x/e"));
        session.observe(observed(
            43,
            "feature",
            "sha",
            vec![lint.clone()],
            Some(true),
        ));
        session.observe(observed(43, "feature", "sha", vec![lint, e2e], Some(true)));
        assert_eq!(session.launched.len(), 2);
        assert!(session.launched[1].prompt.contains("https://x/e"));
        assert!(!session.launched[1].prompt.contains("https://x/l"));
    }

    #[test]
    fn an_unlinked_summary_status_starts_nothing_on_its_own() {
        let mut session = Session::new();
        let combined = failed("Combined CI", None);
        session.observe(observed(50, "feature", "sha", failing(), Some(true)));
        let mut both = failing();
        both.push(combined.clone());
        session.observe(observed(50, "feature", "sha", both, Some(true)));
        session.observe(observed(
            51,
            "feature",
            "sha-only",
            vec![combined],
            Some(true),
        ));
        assert_eq!(session.launched.len(), 1);
        assert_eq!(session.decision(), Some(CiTriageDecision::NoLinkedFailure));
    }

    #[test]
    fn remembers_runs_across_a_restart() {
        let mut session = Session::new();
        session.observe(observed(52, "feature", "sha", failing(), Some(true)));
        let persisted: EngineState =
            serde_json::from_str(&serde_json::to_string(&session.state).expect("state serializes"))
                .expect("state deserializes");
        let result = run_with_state(
            &session.preset,
            worktrees(),
            None,
            &[observed(52, "feature", "sha", failing(), Some(true))],
            &persisted,
            2_000,
        );
        assert!(launches(&result).is_empty());
    }

    #[test]
    fn logs_why_it_started() {
        let mut session = Session::new();
        session.observe(observed(53, "feature", "sha-log", failing(), Some(true)));
        let record = session
            .last
            .as_ref()
            .and_then(|result| result.ci_triage.first().cloned())
            .expect("a record");
        assert_eq!(record.trigger, "watch");
        assert_eq!(record.head_sha.as_deref(), Some("sha-log"));
        assert_eq!(record.outcome.decision, CiTriageDecision::Started);
        assert_eq!(record.outcome.workspace_id.as_deref(), Some("wt-feature"));
        assert_eq!(record.outcome.pr_starts, 1);
        assert_eq!(
            record.outcome.runs,
            vec![TriagedCheck {
                name: "checks".to_owned(),
                url: Some("https://x/runs/7".to_owned())
            }]
        );
    }

    #[test]
    fn stops_after_three_agents_on_one_pr() {
        let mut session = Session::new();
        for sha in ["fix-1", "fix-2", "fix-3", "fix-4", "fix-5"] {
            session.observe(observed(
                21,
                "feature",
                sha,
                vec![failed("checks", Some(&format!("https://x/runs/{sha}")))],
                Some(true),
            ));
        }
        assert_eq!(session.launched.len(), 3);
        assert_eq!(session.decision(), Some(CiTriageDecision::CapReached));
    }

    #[test]
    fn the_cap_expires_seven_days_after_the_last_start() {
        let preset = triage_preset();
        let mut state = preset.state.clone();
        for (n, sha) in ["a", "b", "c"].into_iter().enumerate() {
            let event = observed(
                22,
                "feature",
                sha,
                vec![failed("checks", Some(&format!("https://x/{sha}")))],
                Some(true),
            );
            state = run_with_state(&preset, worktrees(), None, &[event], &state, n as u64).state;
        }
        let fourth = observed(
            22,
            "feature",
            "d",
            vec![failed("checks", Some("https://x/d"))],
            Some(true),
        );
        let blocked = run_with_state(
            &preset,
            worktrees(),
            None,
            std::slice::from_ref(&fourth),
            &state,
            REMEMBER_MS,
        );
        assert!(launches(&blocked).is_empty());
        let later = run_with_state(
            &preset,
            worktrees(),
            None,
            &[fourth],
            &state,
            2 + REMEMBER_MS,
        );
        assert_eq!(launches(&later).len(), 1);
    }

    #[test]
    fn stays_out_of_other_repos_passing_checks_and_prs_without_a_workspace() {
        let mut session = Session::new();
        let mut other_repo = observed(12, "feature", "sha-b", failing(), Some(true));
        if let Event::ChecksObserved(observed) = &mut other_repo {
            observed.repo_id = "repo-2".to_owned();
        }
        session.observe(other_repo);
        session.observe(observed(
            12,
            "feature",
            "sha-c",
            vec![Check::run("checks", S::Completed, Some(C::Success), None)],
            Some(true),
        ));
        session.observe(observed(13, "no-workspace", "sha-d", failing(), Some(true)));
        assert_eq!(session.decision(), Some(CiTriageDecision::NoWorkspace));
        assert!(session.launched.is_empty());
    }

    #[test]
    fn runs_whatever_the_status_rules_master_switch_says() {
        let preset = build(&OrcaPresetInput {
            workspace_status_rules: json!({ "enabled": false, "repoIds": [] }),
            ci_triage: vec![("repo-1".to_owned(), json!({}))],
            ..OrcaPresetInput::default()
        });
        let result = run(
            &preset,
            worktrees(),
            None,
            &[observed(60, "feature", "sha", failing(), Some(true))],
        );
        assert_eq!(launches(&result).len(), 1);
        assert!(
            launches(&result)[0]
                .prompt
                .ends_with(DEFAULT_CI_TRIAGE_INSTRUCTIONS)
        );
    }
}

mod renderer_ci_triage_watch_test {
    use super::*;

    fn watched(
        repo: &str,
        enabled: bool,
        branch: &str,
        pr: Option<(u64, CheckState, PrLifecycle)>,
    ) -> WatchedWorkspace {
        WatchedWorkspace {
            repo_id: repo.to_owned(),
            ci_triage_enabled: enabled,
            branch: branch.to_owned(),
            pr: pr.map(|(number, checks, lifecycle)| WatchedPr {
                number,
                lifecycle,
                checks_status: Some(checks),
                head_sha: Some(format!("sha-{number}")),
            }),
        }
    }

    #[test]
    fn picks_open_failed_prs_in_repos_with_triage_on() {
        let targets = ci_triage_fetch_targets(&[
            watched(
                "repo-1",
                true,
                "feature",
                Some((7, CheckState::Failure, PrLifecycle::Open)),
            ),
            watched(
                "repo-1",
                true,
                "green",
                Some((8, CheckState::Success, PrLifecycle::Open)),
            ),
            watched(
                "repo-1",
                true,
                "old",
                Some((9, CheckState::Failure, PrLifecycle::Merged)),
            ),
            watched("repo-1", true, "no-pr", None),
            watched(
                "repo-2",
                false,
                "feature",
                Some((10, CheckState::Failure, PrLifecycle::Open)),
            ),
            watched(
                "repo-1",
                true,
                "draft",
                Some((11, CheckState::Failure, PrLifecycle::Draft)),
            ),
        ]);
        assert_eq!(
            targets
                .iter()
                .map(|target| target.pr_number)
                .collect::<Vec<_>>(),
            vec![7, 11]
        );
        assert_eq!(targets[0].head_sha.as_deref(), Some("sha-7"));
        assert_eq!(targets[0].branch, "feature");
    }
}
