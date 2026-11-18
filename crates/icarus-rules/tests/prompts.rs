use icarus_rules::facts::{PullRequest, RepoRef};
use icarus_rules::prompts::*;
use icarus_rules::template::*;

fn subject() -> PullRequest {
    PullRequest {
        repo: RepoRef::new("acme", "app"),
        number: 42,
        title: "Tidy the timeline".to_owned(),
        url: "https://github.com/acme/app/pull/42".to_owned(),
        author: Some("colleague".to_owned()),
        head_ref_name: "fix/timeline".to_owned(),
        base_ref_name: "main".to_owned(),
        head_ref_oid: "abc".to_owned(),
        ..PullRequest::default()
    }
}

fn variables(pairs: &[(&str, &str)]) -> Variables {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
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

fn conflict_vars() -> Variables {
    variables(&[
        ("baseRef", "main"),
        ("worktreePath", "/w/e-1"),
        ("prNumber", "3166"),
    ])
}

#[test]
fn blank_conflict_template_uses_the_built_in_prompt() {
    let built_in = conflict_agent_prompt("main", "/w/e-1");
    assert_eq!(render_conflict_agent_prompt("", &conflict_vars()), built_in);
    assert_eq!(
        render_conflict_agent_prompt("   ", &conflict_vars()),
        built_in
    );
}

#[test]
fn editable_conflict_template_starts_as_exactly_the_built_in_prompt() {
    assert_eq!(
        render_conflict_agent_prompt(&default_conflict_prompt_template(), &conflict_vars()),
        conflict_agent_prompt("main", "/w/e-1")
    );
}

#[test]
fn fills_a_custom_conflict_template() {
    assert_eq!(
        render_conflict_agent_prompt(
            "Merge {{baseRef}} into {{worktreePath}} for #{{prNumber}}",
            &conflict_vars()
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

#[test]
fn fills_the_pull_request_settings_into_the_create_prompt() {
    let vars = variables(&[
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
    ]);
    let prompt = render_action_prompt(DEFAULT_CREATE_PULL_REQUEST_PROMPT, &vars);
    assert!(prompt.contains("targeting main"));
    assert!(prompt.contains("Create it as a draft with gh pr create."));
    assert!(prompt.contains("Title as feat(scope): …"));
    assert!(!prompt.contains("{{"));
}

#[test]
fn collapses_blank_runs_trims_and_keeps_padded_or_unknown_names() {
    assert_eq!(
        render_action_prompt(
            "\n a\n{{prInstructions}}\n\n\nb {{ prNumber }} {{x}}\n",
            &variables(&[("prInstructions", "")])
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
