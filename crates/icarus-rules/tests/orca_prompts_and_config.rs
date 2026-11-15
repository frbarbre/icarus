mod common;

use common::*;
use icarus_rules::model::AgentView;
use icarus_rules::orca_preset::{ResolveAction, normalize, parse_pm_approval_team};
use icarus_rules::prompts::*;
use icarus_rules::template::*;
use serde_json::json;

mod shared_workspace_status_rule_prompt_test {
    use super::*;

    fn subject() -> icarus_rules::facts::PullRequest {
        let mut subject = pr(42);
        subject.title = "Tidy the timeline".to_owned();
        subject.author = Some("colleague".to_owned());
        subject.head_ref_name = "fix/timeline".to_owned();
        subject
    }

    #[test]
    fn substitutes_every_supported_variable() {
        assert_eq!(
            render_rule_prompt(
                "{{prNumber}} {{title}} {{author}} {{branch}} {{baseRef}} {{url}} {{repo}}",
                &review_prompt_variables(&subject(), None)
            ),
            "42 Tidy the timeline colleague fix/timeline main https://github.com/acme/app/pull/42 acme/app"
        );
    }

    #[test]
    fn padding_unknowns_and_missing_author() {
        let vars = review_prompt_variables(&subject(), None);
        assert_eq!(render_rule_prompt("{{  prNumber  }}", &vars), "42");
        assert_eq!(render_rule_prompt("{{nope}}", &vars), "{{nope}}");
        assert_eq!(render_rule_prompt("{{{prNumber}}}", &vars), "{42}");
        assert_eq!(render_rule_prompt("{{pr_number}}", &vars), "{{pr_number}}");
        let mut anonymous = subject();
        anonymous.author = None;
        assert_eq!(
            review_prompt_variables(&anonymous, None)["author"],
            "unknown"
        );
    }

    #[test]
    fn default_templates_leave_no_placeholder() {
        let rendered = render_rule_prompt(
            DEFAULT_REVIEW_PROMPT_TEMPLATE,
            &review_prompt_variables(&subject(), None),
        );
        assert!(!rendered.contains("{{"));
        assert!(rendered.contains("#42"));
        assert!(rendered.ends_with(REVIEW_AGENT_NOTE_INSTRUCTION));

        let rendered = render_rule_prompt(
            DEFAULT_REREVIEW_PROMPT_TEMPLATE,
            &review_prompt_variables(&subject(), Some("reviewed-sha")),
        );
        assert!(!rendered.contains("{{"));
        assert!(rendered.contains("reviewed-sha"));
        assert!(rendered.contains("colleague"));
    }

    #[test]
    fn since_commit_is_the_reviewed_commit_else_the_base() {
        let vars = review_prompt_variables(&subject(), Some("reviewed-sha"));
        assert_eq!(render_rule_prompt("{{sinceCommit}}", &vars), "reviewed-sha");
        let vars = review_prompt_variables(&subject(), None);
        assert_eq!(render_rule_prompt("{{sinceCommit}}", &vars), "main");
    }

    #[test]
    fn note_instruction_keeps_its_literal_backslashes() {
        assert!(REVIEW_AGENT_NOTE_INSTRUCTION.contains(r"put \n\n between paragraphs"));
    }
}

mod shared_workspace_status_rule_conflict_prompt_test {
    use super::*;

    fn vars() -> Variables {
        [
            ("baseRef", "main"),
            ("worktreePath", "/w/e-1"),
            ("prNumber", "3166"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
    }

    #[test]
    fn blank_template_uses_the_built_in_prompt() {
        let built_in = conflict_agent_prompt("main", "/w/e-1");
        assert_eq!(render_conflict_agent_prompt("", &vars()), built_in);
        assert_eq!(render_conflict_agent_prompt("   ", &vars()), built_in);
    }

    #[test]
    fn editable_template_starts_as_exactly_the_built_in_prompt() {
        assert_eq!(
            render_conflict_agent_prompt(&default_conflict_prompt_template(), &vars()),
            conflict_agent_prompt("main", "/w/e-1")
        );
    }

    #[test]
    fn fills_a_custom_template() {
        assert_eq!(
            render_conflict_agent_prompt(
                "Merge {{baseRef}} into {{worktreePath}} for #{{prNumber}}",
                &vars()
            ),
            "Merge main into /w/e-1 for #3166"
        );
    }

    #[test]
    fn merges_the_base_and_pushes_without_force() {
        let prompt = conflict_agent_prompt("main", "/w/e-1");
        assert!(prompt.contains("git fetch origin main"));
        assert!(prompt.contains("/w/e-1"));
        assert!(!prompt.contains("Do not push"));
        assert!(prompt.contains("push the branch with a plain git push"));
        assert!(prompt.contains("Never force-push"));
    }

    #[test]
    fn a_non_simple_base_ref_is_quoted() {
        let prompt = conflict_agent_prompt("release candidate", "/w");
        assert!(prompt.contains("quoting the ref exactly for the current shell"));
        assert!(prompt.contains("- PR base branch: \"release candidate\""));
    }
}

mod shared_workspace_action_prompts_test {
    use super::*;

    #[test]
    fn fills_the_pull_request_settings_into_the_create_prompt() {
        let vars: Variables = [
            ("branch", "feature/e-1"),
            ("baseRef", "main"),
            ("worktreePath", "/w"),
            ("draftMode", "as a draft"),
            (
                "templateRule",
                "Fill in the repository's pull request template if it has one.",
            ),
            (
                "prInstructions",
                "\nFollow these instructions for the pull request:\nTitle as feat(scope): …",
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let prompt = render_action_prompt(DEFAULT_CREATE_PULL_REQUEST_PROMPT, &vars);
        assert!(prompt.contains("targeting main"));
        assert!(prompt.contains("Create it as a draft with gh pr create."));
        assert!(prompt.contains("Title as feat(scope): …"));
        assert!(!prompt.contains("{{"));
    }

    #[test]
    fn collapses_blank_runs_trims_and_keeps_padded_or_unknown_names() {
        let vars: Variables = [("prInstructions", "")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        assert_eq!(
            render_action_prompt(
                "\n a\n{{prInstructions}}\n\n\nb {{ prNumber }} {{x}}\n",
                &vars
            ),
            "a\n\nb {{ prNumber }} {{x}}"
        );
    }

    #[test]
    fn uses_a_saved_prompt_and_falls_back_when_blank() {
        assert_eq!(
            resolve_action_prompt_template(Some("Mine"), ActionPromptId::ReviewPullRequest),
            "Mine"
        );
        assert_eq!(
            resolve_action_prompt_template(Some("  "), ActionPromptId::ReviewPullRequest),
            DEFAULT_REVIEW_PULL_REQUEST_PROMPT
        );
    }
}

mod shared_workspace_status_rule_config_test {
    use super::*;

    #[test]
    fn review_view_defaults_to_chat_keeps_known_and_falls_back() {
        assert_eq!(normalize(&json!({})).review_inbox.view, AgentView::Chat);
        for (raw, view) in [
            ("chat", AgentView::Chat),
            ("terminal", AgentView::Terminal),
            ("claude-web", AgentView::ClaudeWeb),
            ("default", AgentView::Default),
            ("sideways", AgentView::Chat),
        ] {
            assert_eq!(
                normalize(&json!({ "reviewInbox": { "view": raw } }))
                    .review_inbox
                    .view,
                view
            );
        }
    }

    #[test]
    fn conflict_template_is_kept_verbatim_and_drops_non_strings() {
        assert_eq!(normalize(&json!({})).conflict_prompt_template, "");
        assert_eq!(
            normalize(&json!({ "conflictPromptTemplate": " Merge {{baseRef}} " }))
                .conflict_prompt_template,
            " Merge {{baseRef}} "
        );
        assert_eq!(
            normalize(&json!({ "conflictPromptTemplate": 7 })).conflict_prompt_template,
            ""
        );
    }

    #[test]
    fn review_command_is_trimmed_and_non_strings_dropped() {
        assert_eq!(
            normalize(&json!({ "reviewEnteredCommand": "  make dev-down  " }))
                .review_entered_command,
            "make dev-down"
        );
        assert_eq!(
            normalize(&json!({ "reviewEnteredCommand": 42 })).review_entered_command,
            ""
        );
    }

    #[test]
    fn ledger_kept_only_for_version_one() {
        assert!(
            normalize(&json!({ "handledPullRequests": ["acme/app#3170"] }))
                .handled_pull_requests
                .is_empty()
        );
        assert_eq!(
            normalize(&json!({ "handledPullRequests": ["acme/app#3170", "acme/app#3170", ""], "ledgerVersion": 1 }))
                .handled_pull_requests,
            vec!["acme/app#3170".to_owned()]
        );
        assert_eq!(normalize(&json!({})).ledger_version, 1);
    }

    #[test]
    fn ignores_the_removed_seeded_flag() {
        let config = normalize(
            &json!({ "reviewInbox": { "enabled": true, "agent": "claude", "promptTemplate": "x", "seeded": true } }),
        );
        assert!(config.review_inbox.enabled);
        assert_eq!(config.review_inbox.prompt_template, "x");
    }

    #[test]
    fn unknown_agent_falls_back_to_claude_and_blank_prompts_to_defaults() {
        let config =
            normalize(&json!({ "reviewInbox": { "agent": "nope", "promptTemplate": "  " } }));
        assert_eq!(config.review_inbox.agent, "claude");
        assert_eq!(
            config.review_inbox.prompt_template,
            DEFAULT_REVIEW_PROMPT_TEMPLATE
        );
        assert_eq!(
            normalize(&json!({ "reviewInbox": { "agent": "codex" } }))
                .review_inbox
                .agent,
            "codex"
        );
    }

    #[test]
    fn new_workspace_column_with_its_time() {
        let config = normalize(
            &json!({ "newWorkspaceStatus": " todo ", "newWorkspaceStatusSince": 1_700_000_000_000_u64 }),
        );
        assert_eq!(config.new_workspace_status.as_deref(), Some("todo"));
        assert_eq!(config.new_workspace_status_since, Some(1_700_000_000_000));
        let cleared = normalize(
            &json!({ "newWorkspaceStatus": "", "newWorkspaceStatusSince": 1_700_000_000_000_u64 }),
        );
        assert_eq!(cleared.new_workspace_status, None);
        assert_eq!(cleared.new_workspace_status_since, None);
    }

    #[test]
    fn hold_column() {
        assert_eq!(
            normalize(&json!({ "holdStatus": " on-hold " }))
                .hold_status
                .as_deref(),
            Some("on-hold")
        );
        assert_eq!(normalize(&json!({ "holdStatus": "" })).hold_status, None);
        assert_eq!(normalize(&json!({})).hold_status, None);
    }

    #[test]
    fn enums_strings_and_mapping_are_lenient() {
        let config = normalize(&json!({
            "enabled": "yes",
            "onResolved": "delete",
            "onReviewed": "archive",
            "mergingCheckName": "   ",
            "pmApprovalTeam": "  acme/pm ",
            "repoIds": ["a", "", "a", 3, "b"],
            "statusByCondition": { "draft": " todo ", "bogus": "x", "review": "" },
            "actionPrompts": { "commitAndPush": " mine ", "readyForReview": " ", "other": "x" }
        }));
        assert!(!config.enabled);
        assert_eq!(config.on_resolved, ResolveAction::Delete);
        assert_eq!(config.on_reviewed, ResolveAction::None);
        assert_eq!(config.merging_check_name, DEFAULT_MERGING_CHECK_NAME);
        assert_eq!(config.pm_approval_team.as_deref(), Some("acme/pm"));
        assert_eq!(config.repo_ids, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(config.status_by_condition.len(), 1);
        assert_eq!(config.status_by_condition["draft"], "todo");
        assert_eq!(config.action_prompts.len(), 1);
        assert_eq!(
            config.action_prompts[&ActionPromptId::CommitAndPush],
            " mine "
        );
    }

    #[test]
    fn pm_team_parses_org_and_slug() {
        assert_eq!(
            parse_pm_approval_team(Some("acme/pm")),
            Some(("acme".to_owned(), "pm".to_owned()))
        );
        assert_eq!(parse_pm_approval_team(Some("acme")), None);
        assert_eq!(parse_pm_approval_team(Some("/pm")), None);
        assert_eq!(parse_pm_approval_team(None), None);
    }

    #[test]
    fn handled_key_matching() {
        use icarus_rules::facts::{handled_key, is_handled_key_for_pull_request};
        let key = handled_key(&repo(), 7, None);
        assert_eq!(key, "acme/app#7");
        assert_eq!(handled_key(&repo(), 7, Some("sha")), "acme/app#7@sha");
        assert!(is_handled_key_for_pull_request("acme/app#7@sha", &key));
        assert!(is_handled_key_for_pull_request("acme/app#7", &key));
        assert!(!is_handled_key_for_pull_request("acme/app#70", &key));
    }
}
