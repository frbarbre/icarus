use crate::ci_triage::{MAX_TRIAGES_PER_PR, REMEMBER_MS};
use crate::engine::{EngineState, sanitize_ledger};
use crate::facts::StatusId;
use crate::model::{
    ActionSpec, AgentLaunchSpec, AgentView, Cond, ForgetHandled, GroupEntry, Guard, LayoutSpec,
    Outcome, PrOperation, PromptSpec, Rule, RuleGroup, RuleScope, RulesConfig, StatusDef,
    TickSubject, Trigger,
};
use crate::prompts::{
    ActionPromptId, CiTriageConfig, DEFAULT_MERGING_CHECK_NAME, DEFAULT_REREVIEW_PROMPT_TEMPLATE,
    DEFAULT_REVIEW_PROMPT_TEMPLATE, ci_triage_instructions, parse_ci_triage_config,
    resolve_action_prompt_template,
};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::ops::Not;

pub const CONDITIONS: [&str; 10] = [
    "merged",
    "closed",
    "reviewing",
    "merge-queue",
    "conflicts",
    "draft",
    "changes-requested",
    "merging",
    "pm-approval",
    "review",
];

pub const TUI_AGENTS: [&str; 45] = [
    "claude",
    "claude-agent-teams",
    "codebuddy",
    "openclaude",
    "codex",
    "autohand",
    "opencode",
    "opencode2",
    "mimo-code",
    "pi",
    "omp",
    "qoder",
    "qoder-cn",
    "gemini",
    "antigravity",
    "aider",
    "goose",
    "amp",
    "kilo",
    "kiro",
    "crush",
    "aug",
    "cline",
    "codebuff",
    "freebuff",
    "command-code",
    "continue",
    "cursor",
    "droid",
    "kimi",
    "mistral-vibe",
    "qwen-code",
    "rovo",
    "hermes",
    "openclaw",
    "copilot",
    "grok",
    "devin",
    "ante",
    "trae",
    "muse",
    "zcode",
    "prime-agent",
    "dsh",
    "jcode",
];

pub const POLL_INTERVAL_MS: u64 = 60_000;
pub const MAX_REVIEW_INBOX_CREATES_PER_TICK: usize = 30;
pub const MAX_CREATE_ATTEMPTS: u32 = 3;
pub const REVIEW_COMMAND_TIMEOUT_MS: u64 = 5 * 60_000;
const SEED_LEDGER_VERSION: u64 = 1;
const MAX_CHECK_NAME_LENGTH: usize = 120;
const MAX_TEAM_LENGTH: usize = 120;
const MAX_COMMAND_LENGTH: usize = 1000;
const MAX_PROMPT_LENGTH: usize = 8000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveAction {
    None,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewInbox {
    pub enabled: bool,
    pub agent: String,
    pub view: AgentView,
    pub prompt_template: String,
    pub rereview_prompt_template: String,
}

impl Default for ReviewInbox {
    fn default() -> Self {
        Self {
            enabled: false,
            agent: "claude".to_owned(),
            view: AgentView::Chat,
            prompt_template: DEFAULT_REVIEW_PROMPT_TEMPLATE.to_owned(),
            rereview_prompt_template: DEFAULT_REREVIEW_PROMPT_TEMPLATE.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrcaRuleConfig {
    pub enabled: bool,
    pub repo_ids: Vec<String>,
    pub status_by_condition: BTreeMap<String, StatusId>,
    pub merging_check_name: String,
    pub new_workspace_status: Option<StatusId>,
    pub new_workspace_status_since: Option<u64>,
    pub hold_status: Option<StatusId>,
    pub pm_approval_team: Option<String>,
    pub on_resolved: ResolveAction,
    pub on_reviewed: ResolveAction,
    pub resolve_conflicts_with_agent: bool,
    pub conflict_prompt_template: String,
    pub review_entered_command: String,
    pub action_prompts: BTreeMap<ActionPromptId, String>,
    pub review_inbox: ReviewInbox,
    pub handled_pull_requests: Vec<String>,
    pub ledger_version: u64,
}

impl Default for OrcaRuleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            repo_ids: Vec::new(),
            status_by_condition: BTreeMap::new(),
            merging_check_name: DEFAULT_MERGING_CHECK_NAME.to_owned(),
            new_workspace_status: None,
            new_workspace_status_since: None,
            hold_status: None,
            pm_approval_team: None,
            on_resolved: ResolveAction::None,
            on_reviewed: ResolveAction::None,
            resolve_conflicts_with_agent: false,
            conflict_prompt_template: String::new(),
            review_entered_command: String::new(),
            action_prompts: BTreeMap::new(),
            review_inbox: ReviewInbox::default(),
            handled_pull_requests: Vec::new(),
            ledger_version: SEED_LEDGER_VERSION,
        }
    }
}

impl OrcaRuleConfig {
    pub fn status_for(&self, condition: &str) -> Option<&StatusId> {
        self.status_by_condition.get(condition)
    }
}

fn truncate_utf16(value: &str, max: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for c in value.chars() {
        units += c.len_utf16();
        if units > max {
            break;
        }
        out.push(c);
    }
    out
}

fn trimmed_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn sanitize_review_inbox(value: Option<&Value>) -> ReviewInbox {
    let defaults = ReviewInbox::default();
    let Some(raw) = value.and_then(Value::as_object) else {
        return defaults;
    };
    let non_blank = |key: &str, fallback: String| {
        raw.get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map_or(fallback, str::to_owned)
    };
    ReviewInbox {
        enabled: raw.get("enabled") == Some(&Value::Bool(true)),
        agent: raw
            .get("agent")
            .and_then(Value::as_str)
            .filter(|agent| TUI_AGENTS.contains(agent))
            .map_or(defaults.agent.clone(), str::to_owned),
        view: match raw.get("view").and_then(Value::as_str) {
            Some("terminal") => AgentView::Terminal,
            Some("claude-web") => AgentView::ClaudeWeb,
            Some("default") => AgentView::Default,
            _ => AgentView::Chat,
        },
        prompt_template: non_blank("promptTemplate", defaults.prompt_template.clone()),
        rereview_prompt_template: non_blank(
            "rereviewPromptTemplate",
            defaults.rereview_prompt_template.clone(),
        ),
    }
}

fn sanitize_strings(value: Option<&Value>) -> Vec<String> {
    let mut seen = Vec::new();
    for entry in value.and_then(Value::as_array).into_iter().flatten() {
        if let Some(text) = entry.as_str().filter(|text| !text.trim().is_empty())
            && !seen.iter().any(|existing: &String| existing == text)
        {
            seen.push(text.to_owned());
        }
    }
    seen
}

pub fn normalize(value: &Value) -> OrcaRuleConfig {
    let empty = Map::new();
    let raw = value.as_object().unwrap_or(&empty);
    let defaults = OrcaRuleConfig::default();
    let new_workspace_status = trimmed_string(raw.get("newWorkspaceStatus"));
    let since = raw
        .get("newWorkspaceStatusSince")
        .and_then(Value::as_f64)
        .filter(|since| since.is_finite() && *since >= 0.0)
        .map(|since| since as u64);
    let mut status_by_condition = BTreeMap::new();
    if let Some(mapping) = raw.get("statusByCondition").and_then(Value::as_object) {
        for condition in CONDITIONS {
            if let Some(status) = trimmed_string(mapping.get(condition)) {
                status_by_condition.insert(condition.to_owned(), status);
            }
        }
    }
    let mut action_prompts = BTreeMap::new();
    if let Some(prompts) = raw.get("actionPrompts").and_then(Value::as_object) {
        for id in ActionPromptId::ALL {
            if let Some(prompt) = prompts
                .get(id.key())
                .and_then(Value::as_str)
                .filter(|prompt| !prompt.trim().is_empty())
            {
                action_prompts.insert(id, truncate_utf16(prompt, MAX_PROMPT_LENGTH));
            }
        }
    }
    let resolve_action = |key: &str| {
        if raw.get(key).and_then(Value::as_str) == Some("delete") {
            ResolveAction::Delete
        } else {
            ResolveAction::None
        }
    };
    let ledger_current =
        raw.get("ledgerVersion").and_then(Value::as_u64) == Some(SEED_LEDGER_VERSION);
    OrcaRuleConfig {
        enabled: raw.get("enabled") == Some(&Value::Bool(true)),
        repo_ids: sanitize_strings(raw.get("repoIds")),
        status_by_condition,
        merging_check_name: trimmed_string(raw.get("mergingCheckName"))
            .map_or(defaults.merging_check_name, |name| {
                truncate_utf16(&name, MAX_CHECK_NAME_LENGTH)
            }),
        new_workspace_status_since: new_workspace_status.as_ref().and(since),
        new_workspace_status,
        hold_status: trimmed_string(raw.get("holdStatus")),
        pm_approval_team: trimmed_string(raw.get("pmApprovalTeam"))
            .map(|team| truncate_utf16(&team, MAX_TEAM_LENGTH)),
        on_resolved: resolve_action("onResolved"),
        on_reviewed: resolve_action("onReviewed"),
        resolve_conflicts_with_agent: raw.get("resolveConflictsWithAgent")
            == Some(&Value::Bool(true)),
        conflict_prompt_template: raw
            .get("conflictPromptTemplate")
            .and_then(Value::as_str)
            .map(|template| truncate_utf16(template, MAX_PROMPT_LENGTH))
            .unwrap_or_default(),
        review_entered_command: raw
            .get("reviewEnteredCommand")
            .and_then(Value::as_str)
            .map(|command| truncate_utf16(command.trim(), MAX_COMMAND_LENGTH))
            .unwrap_or_default(),
        action_prompts,
        review_inbox: sanitize_review_inbox(raw.get("reviewInbox")),
        handled_pull_requests: if ledger_current {
            sanitize_ledger(&sanitize_strings(raw.get("handledPullRequests")))
        } else {
            Vec::new()
        },
        ledger_version: SEED_LEDGER_VERSION,
    }
}

pub fn parse_pm_approval_team(value: Option<&str>) -> Option<(String, String)> {
    let mut parts = value?.split('/');
    let org = parts.next()?.trim();
    let slug = parts.next()?.trim();
    (!org.is_empty() && !slug.is_empty()).then(|| (org.to_owned(), slug.to_owned()))
}

#[derive(Debug, Clone, Default)]
pub struct OrcaPresetInput {
    pub workspace_status_rules: Value,
    pub statuses: Vec<StatusDef>,
    pub ci_triage: Vec<(String, Value)>,
    pub fix_checks_agent: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OrcaPreset {
    pub config: RulesConfig,
    pub state: EngineState,
    pub normalized: OrcaRuleConfig,
}

fn entry(
    condition: Option<&str>,
    when: Cond,
    outcome: Outcome,
    actions: Vec<ActionSpec>,
    on_enter: Vec<ActionSpec>,
) -> GroupEntry {
    GroupEntry {
        condition: condition.map(str::to_owned),
        when,
        outcome,
        actions,
        on_enter,
    }
}

fn mapped_or_stop(config: &OrcaRuleConfig, condition: &str) -> Outcome {
    config
        .status_for(condition)
        .map_or(Outcome::Stop, |status| Outcome::MoveTo(status.clone()))
}

// Why a skip: merge-queue and conflicts take part in the order only once they have a column.
fn mapped_or_skip(config: &OrcaRuleConfig, condition: &str) -> Outcome {
    config
        .status_for(condition)
        .map_or(Outcome::Skip, |status| Outcome::MoveTo(status.clone()))
}

fn delete(force: bool, forget_handled: ForgetHandled) -> ActionSpec {
    ActionSpec::DeleteWorkspace {
        force,
        forget_handled,
    }
}

pub fn status_group(config: &OrcaRuleConfig) -> RuleGroup {
    let mut entries = Vec::new();
    for (name, is_state) in [("merged", Cond::PrMerged), ("closed", Cond::PrClosed)] {
        if config.on_resolved == ResolveAction::Delete {
            // Why the head check: a branch can resolve to an old PR it never was, and deleting would discard its work.
            entries.push(entry(
                Some(name),
                Cond::all(vec![is_state.clone(), Cond::HeadIsLocal]),
                Outcome::Stop,
                vec![delete(false, ForgetHandled::Never)],
                Vec::new(),
            ));
        }
        entries.push(entry(
            Some(name),
            is_state,
            mapped_or_stop(config, name),
            Vec::new(),
            Vec::new(),
        ));
    }
    if config.on_reviewed == ResolveAction::Delete {
        let mut released = vec![Cond::ViewerFinished];
        if config.review_inbox.enabled {
            released.push(Cond::all(vec![
                Cond::ViewerKnown,
                Cond::InReviewInbox.not(),
                Cond::TeamReviewRequested.not(),
                Cond::ViewerOwed.not(),
                Cond::ViewerInReviewLists.not(),
                Cond::Handled {
                    with_reviewed_commit: false,
                },
            ]));
        }
        entries.push(entry(
            Some("reviewing"),
            Cond::all(vec![
                Cond::AuthoredBySomeoneElse,
                Cond::HasPendingReviewComments.not(),
                Cond::any(released),
            ]),
            Outcome::Stop,
            vec![delete(true, ForgetHandled::UnlessStillRequested)],
            Vec::new(),
        ));
    }
    entries.push(entry(
        Some("reviewing"),
        Cond::AuthoredBySomeoneElse,
        mapped_or_stop(config, "reviewing"),
        Vec::new(),
        Vec::new(),
    ));
    entries.push(entry(
        Some("merge-queue"),
        Cond::InMergeQueue,
        mapped_or_skip(config, "merge-queue"),
        Vec::new(),
        Vec::new(),
    ));
    let conflict_agent = config
        .resolve_conflicts_with_agent
        .then(|| {
            ActionSpec::LaunchAgent(AgentLaunchSpec {
                agent: Some(config.review_inbox.agent.clone()),
                prompt: PromptSpec::Conflict(config.conflict_prompt_template.clone()),
                rereview_prompt: None,
                view: AgentView::Default,
                background: true,
                title: "Resolve conflicts #{{prNumber}}".to_owned(),
                launch_source: "conflict_resolution".to_owned(),
            })
        })
        .into_iter()
        .collect();
    entries.push(entry(
        Some("conflicts"),
        Cond::Mergeable {
            is: crate::facts::Mergeable::Conflicting,
        },
        mapped_or_skip(config, "conflicts"),
        Vec::new(),
        conflict_agent,
    ));
    // Why: GitHub reports UNKNOWN while it recomputes after a push; holding avoids a bounce out of Conflicts.
    entries.push(entry(
        None,
        Cond::Mergeable {
            is: crate::facts::Mergeable::Unknown,
        },
        if config.status_for("conflicts").is_some() {
            Outcome::Stop
        } else {
            Outcome::Skip
        },
        Vec::new(),
        Vec::new(),
    ));
    entries.push(entry(
        Some("draft"),
        Cond::Draft,
        mapped_or_stop(config, "draft"),
        Vec::new(),
        Vec::new(),
    ));
    entries.push(entry(
        Some("changes-requested"),
        Cond::UnansweredChangeRequest,
        mapped_or_stop(config, "changes-requested"),
        Vec::new(),
        Vec::new(),
    ));
    let team = config.pm_approval_team.clone();
    let mut merging = vec![Cond::CheckSucceeded {
        name: config.merging_check_name.clone(),
    }];
    if let Some(team) = &team {
        merging.push(Cond::ApprovedByTeamAndEveryone { team: team.clone() });
    }
    entries.push(entry(
        Some("merging"),
        Cond::any(merging),
        mapped_or_stop(config, "merging"),
        Vec::new(),
        Vec::new(),
    ));
    entries.push(entry(
        Some("pm-approval"),
        team.map_or(Cond::Never, |team| Cond::AwaitingOnlyTeam { team }),
        mapped_or_stop(config, "pm-approval"),
        Vec::new(),
        Vec::new(),
    ));
    let review_command = (!config.review_entered_command.is_empty())
        .then(|| ActionSpec::RunCommand {
            command: config.review_entered_command.clone(),
            local_only: true,
            timeout_ms: REVIEW_COMMAND_TIMEOUT_MS,
        })
        .into_iter()
        .collect();
    entries.push(entry(
        Some("review"),
        Cond::Always,
        mapped_or_stop(config, "review"),
        Vec::new(),
        review_command,
    ));
    RuleGroup {
        id: "pr-status".to_owned(),
        respect_hold: true,
        entries,
    }
}

fn user_action_rule(name: &str, action: ActionSpec) -> Rule {
    Rule {
        id: format!("top-bar:{name}"),
        trigger: Trigger::UserAction(name.to_owned()),
        scope: RuleScope::Everywhere,
        respect_hold: false,
        when: Cond::Always,
        guards: Vec::new(),
        actions: vec![action],
        max_per_evaluation: None,
    }
}

fn top_bar_agent(config: &OrcaRuleConfig, prompt: PromptSpec, title: &str) -> ActionSpec {
    ActionSpec::LaunchAgent(AgentLaunchSpec {
        agent: Some(config.review_inbox.agent.clone()),
        prompt,
        rereview_prompt: None,
        view: AgentView::Default,
        background: true,
        title: title.to_owned(),
        launch_source: "unknown".to_owned(),
    })
}

fn top_bar_rules(config: &OrcaRuleConfig) -> Vec<Rule> {
    let template = |id: ActionPromptId| {
        PromptSpec::Action(
            resolve_action_prompt_template(config.action_prompts.get(&id).map(String::as_str), id)
                .to_owned(),
        )
    };
    let mut rules = vec![
        user_action_rule(
            "commit-and-push",
            top_bar_agent(
                config,
                template(ActionPromptId::CommitAndPush),
                "Commit & push",
            ),
        ),
        user_action_rule(
            "create-pull-request",
            top_bar_agent(
                config,
                template(ActionPromptId::CreatePullRequest),
                "Create PR",
            ),
        ),
        user_action_rule(
            "ready-for-review",
            top_bar_agent(
                config,
                template(ActionPromptId::ReadyForReview),
                "Ready for review",
            ),
        ),
        user_action_rule(
            "review-pull-request",
            top_bar_agent(
                config,
                template(ActionPromptId::ReviewPullRequest),
                "Review #{{prNumber}}",
            ),
        ),
        user_action_rule(
            "resolve-conflicts",
            top_bar_agent(config, PromptSpec::Provided, "Resolve conflicts"),
        ),
        user_action_rule(
            "enable-stack-auto-merge",
            ActionSpec::EnableStackAutoMerge { method: None },
        ),
    ];
    for (name, operation) in [
        ("merge", PrOperation::Merge),
        ("enable-auto-merge", PrOperation::EnableAutoMerge),
        ("disable-auto-merge", PrOperation::DisableAutoMerge),
        ("add-to-merge-queue", PrOperation::AddToMergeQueue),
        ("merge-stack", PrOperation::MergeStack),
        (
            "disable-stack-auto-merge",
            PrOperation::DisableStackAutoMerge,
        ),
    ] {
        rules.push(user_action_rule(
            name,
            ActionSpec::PullRequest { operation },
        ));
    }
    rules
}

pub fn ci_triage_rule(repo_id: &str, triage: &CiTriageConfig, agent: Option<String>) -> Rule {
    Rule {
        id: format!("ci-triage:{repo_id}"),
        trigger: Trigger::CheckFailed,
        scope: RuleScope::Repos(vec![repo_id.to_owned()]),
        respect_hold: false,
        when: Cond::Always,
        guards: vec![Guard::CiTriage {
            max_per_pr: MAX_TRIAGES_PER_PR,
            window_ms: REMEMBER_MS,
        }],
        actions: vec![ActionSpec::LaunchAgent(AgentLaunchSpec {
            agent,
            prompt: PromptSpec::CiTriage {
                instructions: ci_triage_instructions(triage),
            },
            rereview_prompt: None,
            view: AgentView::Default,
            background: true,
            title: "Fix checks".to_owned(),
            launch_source: "source_control_recovery".to_owned(),
        })],
        max_per_evaluation: None,
    }
}

pub fn build(input: &OrcaPresetInput) -> OrcaPreset {
    let normalized = normalize(&input.workspace_status_rules);
    let config = &normalized;
    let mut rules = Vec::new();
    if let (Some(status), Some(since)) = (
        &config.new_workspace_status,
        config.new_workspace_status_since,
    ) {
        rules.push(Rule {
            id: "new-workspace-status".to_owned(),
            trigger: Trigger::Tick(TickSubject::Workspaces),
            scope: RuleScope::Group,
            respect_hold: false,
            when: Cond::all(vec![Cond::NoColumn, Cond::CreatedSince { at_ms: since }]),
            guards: Vec::new(),
            actions: vec![ActionSpec::SetColumn {
                status: status.clone(),
            }],
            max_per_evaluation: None,
        });
    }
    if config.review_inbox.enabled {
        rules.push(Rule {
            id: "review-inbox".to_owned(),
            trigger: Trigger::Tick(TickSubject::ReviewRequests),
            scope: RuleScope::Group,
            respect_hold: false,
            when: Cond::all(vec![
                Cond::Handled {
                    with_reviewed_commit: true,
                }
                .not(),
                Cond::Linked.not(),
                Cond::AuthoredByViewer.not(),
                Cond::BranchHasWorkspace.not(),
            ]),
            guards: Vec::new(),
            actions: vec![ActionSpec::CreateWorkspaceFromPr {
                initial_column: config.status_for("reviewing").cloned(),
                max_attempts: Some(MAX_CREATE_ATTEMPTS),
                agent: Some(AgentLaunchSpec {
                    agent: Some(config.review_inbox.agent.clone()),
                    prompt: PromptSpec::Rule(config.review_inbox.prompt_template.clone()),
                    rereview_prompt: Some(PromptSpec::Rule(
                        config.review_inbox.rereview_prompt_template.clone(),
                    )),
                    view: config.review_inbox.view,
                    background: true,
                    title: "Review #{{prNumber}}".to_owned(),
                    launch_source: "unknown".to_owned(),
                }),
                layout: Some(LayoutSpec::review()),
            }],
            max_per_evaluation: Some(MAX_REVIEW_INBOX_CREATES_PER_TICK),
        });
    }
    rules.push(Rule {
        id: "review-head-sync".to_owned(),
        trigger: Trigger::Tick(TickSubject::LinkedWorkspaces),
        scope: RuleScope::Group,
        respect_hold: true,
        when: Cond::all(vec![
            Cond::AuthoredBySomeoneElse,
            Cond::LocalCheckout,
            Cond::PrOpen,
            Cond::HasPrHead,
            Cond::any(vec![Cond::HeadIsLocal.not(), Cond::BaseFollowsTarget.not()]),
            Cond::HeadBranchMatches,
        ]),
        guards: Vec::new(),
        actions: vec![ActionSpec::SyncToPrHead],
        max_per_evaluation: None,
    });
    for (repo_id, value) in &input.ci_triage {
        if let Some(triage) = parse_ci_triage_config(value) {
            rules.push(ci_triage_rule(
                repo_id,
                &triage,
                input.fix_checks_agent.clone(),
            ));
        }
    }
    rules.extend(top_bar_rules(config));
    OrcaPreset {
        config: RulesConfig {
            enabled: config.enabled,
            scope: config.repo_ids.clone(),
            statuses: input.statuses.clone(),
            hold_status: config.hold_status.clone(),
            groups: vec![status_group(config)],
            rules,
        },
        state: EngineState {
            ledger: config.handled_pull_requests.clone(),
            ..EngineState::default()
        },
        normalized,
    }
}
