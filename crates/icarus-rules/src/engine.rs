use crate::ci_triage::{self, ChecksObserved, CiTriageMemory, CiTriageOutcome, Starts};
use crate::facts::{
    CheckState, Facts, PrLink, PrState, PullRequest, Snapshot, StatusId, Workspace, handled_key,
    is_handled_key_for_pull_request,
};
use crate::model::{
    ActionSpec, AgentLaunchSpec, AgentView, Cond, ForgetHandled, Guard, LayoutSpec, Outcome,
    PrOperation, PromptSpec, Rule, RuleGroup, RuleScope, RulesConfig, TickSubject, Trigger,
};
use crate::next_action::stack_merge_target;
use crate::predicates::{
    approved_by_team_and_everyone, awaits_only_team, has_passing_named_check,
    has_unanswered_change_request, lowercase_login_set, viewer_in_review_lists, viewer_standing,
};
use crate::prompts::{ci_triage_prompt, render_conflict_agent_prompt};
use crate::template::{
    Variables, render_action_prompt, render_rule_prompt, review_prompt_variables,
    review_workspace_name,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_HANDLED_PULL_REQUESTS: usize = 500;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrMemo {
    pub head_oid: String,
    pub requested: bool,
    pub viewer_review: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineState {
    pub ledger: Vec<String>,
    // Why not persisted in Orca: a create failing for a passing reason must not burn the PR forever.
    pub create_attempts: BTreeMap<String, u32>,
    pub ci_triage: CiTriageMemory,
    pub rate_limits: BTreeMap<String, Starts>,
    pub pr_memo: BTreeMap<String, PrMemo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    Created { handled_key: String },
    CreateFailed { handled_key: String },
    Deleted { forget_handled_key: Option<String> },
}

pub fn sanitize_ledger(entries: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let unique: Vec<String> = entries
        .iter()
        .filter(|entry| !entry.trim().is_empty() && seen.insert(entry.as_str()))
        .cloned()
        .collect();
    let skip = unique.len().saturating_sub(MAX_HANDLED_PULL_REQUESTS);
    unique.into_iter().skip(skip).collect()
}

impl EngineState {
    pub fn record(&mut self, outcome: &ActionOutcome) {
        match outcome {
            ActionOutcome::Created { handled_key } => {
                self.create_attempts.remove(handled_key);
                if !self.ledger.contains(handled_key) {
                    self.ledger.push(handled_key.clone());
                    self.ledger = sanitize_ledger(&self.ledger);
                }
            }
            ActionOutcome::CreateFailed { handled_key } => {
                *self.create_attempts.entry(handled_key.clone()).or_default() += 1;
            }
            ActionOutcome::Deleted {
                forget_handled_key: Some(key),
            } => {
                // Why by pull request: forgetting must also drop the entries keyed on a reviewed commit.
                self.ledger
                    .retain(|entry| !is_handled_key_for_pull_request(entry, key));
            }
            ActionOutcome::Deleted { .. } => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    WorkspaceCreated {
        workspace_id: String,
    },
    ChecksObserved(ChecksObserved),
    UserAction {
        workspace_id: String,
        action: String,
        #[serde(default)]
        variables: Variables,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLaunch {
    pub agent: Option<String>,
    pub prompt: String,
    pub view: AgentView,
    pub background: bool,
    pub title: String,
    pub launch_source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PlannedAction {
    SetColumn {
        workspace_id: String,
        execution_host: String,
        status: StatusId,
        condition: Option<String>,
    },
    DeleteWorkspace {
        workspace_id: String,
        execution_host: String,
        display_name: String,
        force: bool,
        forget_handled_key: Option<String>,
    },
    CreateWorkspaceFromPr {
        pr: Box<PullRequest>,
        handled_key: String,
        since_review_commit: Option<String>,
        name: String,
        display_title: String,
        initial_column: Option<StatusId>,
        agent: Option<AgentLaunch>,
        layout: Option<LayoutSpec>,
    },
    LaunchAgent {
        workspace_id: Option<String>,
        launch: AgentLaunch,
    },
    RunCommand {
        workspace_id: String,
        path: String,
        command: String,
        timeout_ms: u64,
    },
    SyncToPrHead {
        workspace_id: String,
        path: String,
        branch: String,
        head_oid: String,
    },
    Notify {
        workspace_id: Option<String>,
        message: String,
    },
    OpenLayout {
        workspace_id: String,
        layout: LayoutSpec,
        pr_url: Option<String>,
    },
    EnableStackAutoMerge {
        pr: PrLink,
        stack_number: u64,
        method: Option<crate::facts::MergeMethod>,
    },
    PullRequest {
        workspace_id: Option<String>,
        pr: PrLink,
        operation: PrOperation,
    },
}

impl PlannedAction {
    fn phase(&self) -> u8 {
        match self {
            Self::SetColumn { .. } => 0,
            Self::DeleteWorkspace { .. } => 1,
            Self::CreateWorkspaceFromPr { .. } => 2,
            Self::LaunchAgent { .. } => 3,
            Self::RunCommand { .. } => 4,
            Self::SyncToPrHead { .. } => 5,
            _ => 6,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetTrace {
    pub workspace_id: String,
    pub group: String,
    pub condition: Option<String>,
    pub status: Option<StatusId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiTriageRecord {
    pub rule: String,
    pub trigger: String,
    pub repo_id: String,
    pub pr_number: u64,
    pub branch: String,
    pub head_sha: Option<String>,
    pub outcome: CiTriageOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluation {
    pub actions: Vec<PlannedAction>,
    pub trace: Vec<TargetTrace>,
    pub ci_triage: Vec<CiTriageRecord>,
    pub state: EngineState,
}

struct World<'a> {
    config: &'a RulesConfig,
    snapshot: Option<&'a Snapshot>,
    workspaces: &'a [Workspace],
    viewer: Option<&'a str>,
    ledger: &'a [String],
    inbox_keys: BTreeSet<String>,
    linked_keys: BTreeSet<String>,
    branches: BTreeSet<String>,
    teams: BTreeMap<String, BTreeSet<String>>,
    by_key: BTreeMap<String, &'a PullRequest>,
    gated: bool,
}

impl<'a> World<'a> {
    fn new(config: &'a RulesConfig, facts: &'a Facts, ledger: &'a [String]) -> Self {
        let snapshot = facts.snapshot.as_ref();
        let scoped = |workspace: &&Workspace| config.scope.contains(&workspace.repo_id);
        let targets = facts.workspaces.iter().filter(scoped).filter(|workspace| {
            workspace.is_ordinary() && workspace.branch_name().is_some() && workspace.pr.is_some()
        });
        Self {
            config,
            snapshot,
            workspaces: &facts.workspaces,
            viewer: snapshot.and_then(|snapshot| snapshot.viewer_login.as_deref()),
            ledger,
            inbox_keys: snapshot
                .map(|snapshot| {
                    snapshot
                        .review_requested
                        .iter()
                        .map(PullRequest::match_key)
                        .collect()
                })
                .unwrap_or_default(),
            linked_keys: targets
                .filter_map(|workspace| workspace.pr.as_ref().map(PrLink::match_key))
                .collect(),
            branches: facts
                .workspaces
                .iter()
                .filter(scoped)
                .filter_map(|workspace| workspace.branch_name().map(str::to_owned))
                .collect(),
            teams: snapshot
                .map(|snapshot| {
                    snapshot
                        .team_members
                        .iter()
                        .map(|(team, members)| (team.clone(), lowercase_login_set(members)))
                        .collect()
                })
                .unwrap_or_default(),
            by_key: snapshot
                .map(|snapshot| {
                    snapshot
                        .linked
                        .iter()
                        .map(|pr| (pr.match_key(), pr))
                        .collect()
                })
                .unwrap_or_default(),
            gated: config.enabled && !config.scope.is_empty(),
        }
    }

    fn team(&self, team: &str) -> BTreeSet<String> {
        self.teams.get(team).cloned().unwrap_or_default()
    }

    fn is_target(&self, workspace: &Workspace) -> bool {
        self.config.scope.contains(&workspace.repo_id)
            && workspace.is_ordinary()
            && workspace.branch_name().is_some()
            && workspace.pr.is_some()
    }

    fn is_held(&self, workspace: &Workspace) -> bool {
        self.config.hold_status.is_some() && workspace.status == self.config.hold_status
    }

    fn linked_pr(&self, workspace: &Workspace) -> Option<&'a PullRequest> {
        workspace
            .pr
            .as_ref()
            .and_then(|link| self.by_key.get(&link.match_key()).copied())
    }

    fn applies(&self, scope: &RuleScope, repo_id: Option<&str>) -> bool {
        match (scope, repo_id) {
            (RuleScope::Everywhere, _) => true,
            (RuleScope::Group, Some(repo)) => {
                self.gated && self.config.scope.iter().any(|id| id == repo)
            }
            (RuleScope::Group, None) => self.gated,
            (RuleScope::Repos(repos), Some(repo)) => repos.iter().any(|id| id == repo),
            (RuleScope::Repos(_), None) => false,
        }
    }
}

#[derive(Clone, Copy)]
struct Ctx<'w, 'a> {
    world: &'w World<'a>,
    pr: Option<&'a PullRequest>,
    workspace: Option<&'a Workspace>,
}

impl Ctx<'_, '_> {
    fn eval(&self, cond: &Cond) -> bool {
        let pr = self.pr;
        let ws = self.workspace;
        let viewer = self.world.viewer;
        let on_pr = |f: &dyn Fn(&PullRequest) -> bool| pr.is_some_and(f);
        let on_ws = |f: &dyn Fn(&Workspace) -> bool| ws.is_some_and(f);
        let on_both = |f: &dyn Fn(&PullRequest, &Workspace) -> bool| matches!((pr, ws), (Some(p), Some(w)) if f(p, w));
        match cond {
            Cond::Always => true,
            Cond::Never => false,
            Cond::All { of } => of.iter().all(|cond| self.eval(cond)),
            Cond::Any { of } => of.iter().any(|cond| self.eval(cond)),
            Cond::Not { inner } => !self.eval(inner),
            Cond::PrMerged => on_pr(&|pr| pr.state == PrState::Merged),
            Cond::PrClosed => on_pr(&|pr| pr.state == PrState::Closed),
            Cond::PrOpen => on_pr(&|pr| pr.state == PrState::Open),
            Cond::Draft => on_pr(&|pr| pr.is_draft),
            Cond::ViewerKnown => viewer.is_some_and(|login| !login.is_empty()),
            Cond::AuthoredByViewer => on_pr(&|pr| {
                viewer.is_some_and(|login| !login.is_empty() && pr.is_authored_by(login))
            }),
            Cond::AuthoredBySomeoneElse => on_pr(&|pr| {
                viewer.is_some_and(|login| {
                    !login.is_empty() && pr.author.is_some() && !pr.is_authored_by(login)
                })
            }),
            Cond::InMergeQueue => on_pr(&|pr| pr.is_in_merge_queue),
            Cond::MergeQueueState { state } => on_pr(&|pr| {
                pr.merge_queue_entry
                    .as_ref()
                    .is_some_and(|entry| entry.state == *state)
            }),
            Cond::Mergeable { is } => on_pr(&|pr| pr.mergeable == Some(*is)),
            Cond::MergeState { is } => on_pr(&|pr| pr.merge_state_status == Some(*is)),
            Cond::UnansweredChangeRequest => on_pr(&has_unanswered_change_request),
            Cond::CheckSucceeded { name } => on_pr(&|pr| has_passing_named_check(pr, name)),
            Cond::ChecksFailing => on_pr(&|pr| {
                pr.checks
                    .iter()
                    .any(|check| check.state() == CheckState::Failure)
            }),
            Cond::ChecksWaitingApproval => on_pr(&|pr| {
                pr.checks
                    .iter()
                    .any(crate::facts::Check::is_waiting_approval)
            }),
            Cond::ApprovedByTeamAndEveryone { team } => {
                let members = self.world.team(team);
                on_pr(&|pr| approved_by_team_and_everyone(pr, &members))
            }
            Cond::AwaitingOnlyTeam { team } => {
                let members = self.world.team(team);
                on_pr(&|pr| awaits_only_team(pr, &members))
            }
            Cond::ViewerOwed => on_pr(&|pr| viewer_standing(pr, viewer).is_owed),
            Cond::ViewerFinished => on_pr(&|pr| viewer_standing(pr, viewer).is_finished),
            Cond::ViewerInReviewLists => on_pr(&|pr| viewer_in_review_lists(pr, viewer)),
            Cond::InReviewInbox => on_pr(&|pr| self.world.inbox_keys.contains(&pr.match_key())),
            Cond::TeamReviewRequested => on_pr(&PullRequest::has_team_reviewer),
            Cond::Handled {
                with_reviewed_commit,
            } => on_pr(&|pr| {
                if *with_reviewed_commit {
                    let commit = viewer_standing(pr, viewer).commit_oid;
                    let key = handled_key(&pr.repo, pr.number, commit.as_deref());
                    self.world.ledger.contains(&key)
                } else {
                    let key = handled_key(&pr.repo, pr.number, None);
                    self.world
                        .ledger
                        .iter()
                        .any(|entry| is_handled_key_for_pull_request(entry, &key))
                }
            }),
            Cond::AutoMergeEnabled => on_pr(&|pr| pr.auto_merge.is_some()),
            Cond::InStack => on_pr(&|pr| pr.stack.is_some()),
            Cond::StackReady => on_pr(&|pr| {
                pr.stack
                    .as_ref()
                    .is_some_and(|stack| stack_merge_target(stack).is_some())
            }),
            Cond::StackAutoMergeEnabled => on_pr(&|pr| {
                pr.stack
                    .as_ref()
                    .is_some_and(|stack| stack.auto_merge.is_some())
            }),
            Cond::Linked => on_pr(&|pr| self.world.linked_keys.contains(&pr.match_key())),
            Cond::BranchHasWorkspace => {
                on_pr(&|pr| self.world.branches.contains(&pr.head_ref_name))
            }
            Cond::HeadIsLocal => on_both(&|pr, ws| ws.head_oid == pr.head_ref_oid),
            Cond::HasPrHead => on_pr(&|pr| !pr.head_ref_oid.is_empty()),
            Cond::HeadBranchMatches => {
                on_both(&|pr, ws| ws.branch_name() == Some(pr.head_ref_name.as_str()))
            }
            Cond::Dirty => on_ws(&|ws| ws.dirty),
            Cond::HasPendingReviewComments => on_ws(&|ws| ws.has_pending_review_comments),
            Cond::LocalHost => on_ws(&Workspace::is_local_host),
            Cond::LocalCheckout => on_ws(&|ws| ws.is_local_host() && !ws.repo_is_remote),
            Cond::InColumn { status } => on_ws(&|ws| ws.status.as_ref() == Some(status)),
            Cond::NoColumn => on_ws(&|ws| ws.status.as_deref().is_none_or(str::is_empty)),
            Cond::CreatedSince { at_ms } => {
                on_ws(&|ws| ws.created_at.is_some_and(|created| created >= *at_ms))
            }
        }
    }

    fn variables(&self, since: Option<&str>, extra: Option<&Variables>) -> Variables {
        let mut vars = self
            .pr
            .map(|pr| review_prompt_variables(pr, since))
            .unwrap_or_default();
        if let Some(ws) = self.workspace {
            vars.insert("worktreePath".to_owned(), ws.path.clone());
            if let Some(branch) = ws.branch_name().filter(|_| self.pr.is_none()) {
                vars.insert("branch".to_owned(), branch.to_owned());
            }
        }
        if let Some(extra) = extra {
            vars.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        vars
    }
}

fn render_prompt(spec: &PromptSpec, vars: &Variables) -> String {
    match spec {
        PromptSpec::Rule(template) => render_rule_prompt(template, vars),
        PromptSpec::Conflict(template) => render_conflict_agent_prompt(template, vars),
        PromptSpec::Action(template) => render_action_prompt(template, vars),
        PromptSpec::CiTriage { instructions } => instructions.clone(),
        PromptSpec::Provided => vars.get("prompt").cloned().unwrap_or_default(),
    }
}

fn launch(spec: &AgentLaunchSpec, vars: &Variables, rereview: bool) -> AgentLaunch {
    let prompt = match (&spec.rereview_prompt, rereview) {
        (Some(rereview), true) => rereview,
        _ => &spec.prompt,
    };
    AgentLaunch {
        agent: spec.agent.clone(),
        prompt: render_prompt(prompt, vars),
        view: spec.view,
        background: spec.background,
        title: render_rule_prompt(&spec.title, vars),
        launch_source: spec.launch_source.clone(),
    }
}

struct Emitter<'s> {
    out: Vec<PlannedAction>,
    state: &'s mut EngineState,
}

impl Emitter<'_> {
    fn emit(&mut self, spec: &ActionSpec, ctx: Ctx<'_, '_>, extra: Option<&Variables>) {
        let ws = ctx.workspace;
        let pr = ctx.pr;
        let pr_link = || {
            pr.map(|pr| PrLink {
                repo: pr.repo.clone(),
                number: pr.number,
            })
            .or_else(|| ws.and_then(|ws| ws.pr.clone()))
        };
        match spec {
            ActionSpec::SetColumn { status } => {
                if let Some(ws) = ws.filter(|ws| ws.status.as_ref() != Some(status)) {
                    self.out.push(PlannedAction::SetColumn {
                        workspace_id: ws.id.clone(),
                        execution_host: ws.execution_host.clone(),
                        status: status.clone(),
                        condition: None,
                    });
                }
            }
            ActionSpec::DeleteWorkspace {
                force,
                forget_handled,
            } => {
                let Some(ws) = ws else { return };
                // Why conditional: forgetting a PR still listed as owed this tick would have it cloned straight back.
                let forget_handled_key = match (forget_handled, pr) {
                    (ForgetHandled::UnlessStillRequested, Some(pr))
                        if !ctx.world.inbox_keys.contains(&pr.match_key()) =>
                    {
                        Some(handled_key(&pr.repo, pr.number, None))
                    }
                    _ => None,
                };
                self.out.push(PlannedAction::DeleteWorkspace {
                    workspace_id: ws.id.clone(),
                    execution_host: ws.execution_host.clone(),
                    display_name: ws.display_name.clone(),
                    force: *force,
                    forget_handled_key,
                });
            }
            ActionSpec::CreateWorkspaceFromPr {
                initial_column,
                max_attempts,
                agent,
                layout,
            } => {
                let Some(pr) = pr else { return };
                let since = viewer_standing(pr, ctx.world.viewer).commit_oid;
                let key = handled_key(&pr.repo, pr.number, since.as_deref());
                let attempts = self.state.create_attempts.get(&key).copied().unwrap_or(0);
                if max_attempts.is_some_and(|max| attempts >= max) {
                    return;
                }
                let vars = ctx.variables(since.as_deref(), extra);
                self.out.push(PlannedAction::CreateWorkspaceFromPr {
                    pr: Box::new(pr.clone()),
                    handled_key: key,
                    since_review_commit: since.clone(),
                    name: review_workspace_name(pr),
                    display_title: pr.title.clone(),
                    initial_column: initial_column.clone(),
                    agent: agent
                        .as_ref()
                        .map(|spec| launch(spec, &vars, since.is_some())),
                    layout: layout.clone(),
                });
            }
            ActionSpec::LaunchAgent(spec) => {
                let vars = ctx.variables(None, extra);
                self.out.push(PlannedAction::LaunchAgent {
                    workspace_id: ws.map(|ws| ws.id.clone()),
                    launch: launch(spec, &vars, false),
                });
            }
            ActionSpec::RunCommand {
                command,
                local_only,
                timeout_ms,
            } => {
                let command = command.trim();
                let Some(ws) = ws else { return };
                if command.is_empty() || (*local_only && !ws.is_local_host()) {
                    return;
                }
                self.out.push(PlannedAction::RunCommand {
                    workspace_id: ws.id.clone(),
                    path: ws.path.clone(),
                    command: command.to_owned(),
                    timeout_ms: *timeout_ms,
                });
            }
            ActionSpec::SyncToPrHead => {
                if let (Some(ws), Some(pr), Some(branch)) =
                    (ws, pr, ws.and_then(Workspace::branch_name))
                {
                    self.out.push(PlannedAction::SyncToPrHead {
                        workspace_id: ws.id.clone(),
                        path: ws.path.clone(),
                        branch: branch.to_owned(),
                        head_oid: pr.head_ref_oid.clone(),
                    });
                }
            }
            ActionSpec::Notify { message } => {
                let vars = ctx.variables(None, extra);
                self.out.push(PlannedAction::Notify {
                    workspace_id: ws.map(|ws| ws.id.clone()),
                    message: render_rule_prompt(message, &vars),
                });
            }
            ActionSpec::OpenLayout { layout } => {
                if let Some(ws) = ws {
                    self.out.push(PlannedAction::OpenLayout {
                        workspace_id: ws.id.clone(),
                        layout: layout.clone(),
                        pr_url: pr.map(|pr| pr.url.clone()),
                    });
                }
            }
            ActionSpec::EnableStackAutoMerge { method } => {
                if let (Some(link), Some(stack)) = (pr_link(), pr.and_then(|pr| pr.stack.as_ref()))
                {
                    self.out.push(PlannedAction::EnableStackAutoMerge {
                        pr: link,
                        stack_number: stack.number,
                        method: *method,
                    });
                }
            }
            ActionSpec::PullRequest { operation } => {
                if let Some(link) = pr_link() {
                    self.out.push(PlannedAction::PullRequest {
                        workspace_id: ws.map(|ws| ws.id.clone()),
                        pr: link,
                        operation: *operation,
                    });
                }
            }
        }
    }
}

fn rate_key(rule: &Rule, ctx: Ctx<'_, '_>) -> String {
    let subject = ctx
        .pr
        .map(PullRequest::match_key)
        .or_else(|| ctx.workspace.map(|ws| ws.id.clone()))
        .unwrap_or_default();
    serde_json::json!([rule.id, subject]).to_string()
}

fn passes_guards(rule: &Rule, ctx: Ctx<'_, '_>, state: &mut EngineState, now: u64) -> bool {
    for guard in &rule.guards {
        if let Guard::RateLimit { max, window_ms } = guard {
            let key = rate_key(rule, ctx);
            let current = state
                .rate_limits
                .get(&key)
                .filter(|starts| now.saturating_sub(starts.at) < *window_ms)
                .map_or(0, |starts| starts.count);
            if current >= *max {
                return false;
            }
            state.rate_limits.insert(
                key,
                Starts {
                    count: current + 1,
                    at: now,
                },
            );
        }
    }
    true
}

fn run_rule(
    rule: &Rule,
    ctx: Ctx<'_, '_>,
    emitter: &mut Emitter<'_>,
    now: u64,
    extra: Option<&Variables>,
) -> bool {
    if !ctx.eval(&rule.when) || !passes_guards(rule, ctx, emitter.state, now) {
        return false;
    }
    for action in &rule.actions {
        emitter.emit(action, ctx, extra);
    }
    true
}

fn pr_memo(pr: &PullRequest, world: &World<'_>) -> PrMemo {
    let viewer = world.viewer;
    let standing = viewer_standing(pr, viewer);
    let viewer_review = viewer.and_then(|login| {
        let login = login.to_lowercase();
        pr.latest_reviews
            .iter()
            .find(|review| review.login.to_lowercase() == login)
            .map(|review| {
                format!(
                    "{:?}@{}",
                    review.state,
                    review.commit_oid.as_deref().unwrap_or("")
                )
            })
            .or_else(|| {
                pr.viewer_latest_review.as_ref().map(|review| {
                    format!(
                        "{}@{}",
                        review.state,
                        review.commit_oid.as_deref().unwrap_or("")
                    )
                })
            })
    });
    PrMemo {
        head_oid: pr.head_ref_oid.clone(),
        requested: standing.is_owed || world.inbox_keys.contains(&pr.match_key()),
        viewer_review,
    }
}

fn derived_events(previous: &PrMemo, current: &PrMemo) -> Vec<Trigger> {
    let mut events = Vec::new();
    if previous.head_oid != current.head_oid {
        events.push(Trigger::PrHeadMoved);
    }
    if !previous.requested && current.requested {
        events.push(Trigger::ReviewRequested);
    }
    if previous.requested && !current.requested && current.viewer_review.is_none() {
        events.push(Trigger::ReviewWithdrawn);
    }
    if current.viewer_review.is_some() && previous.viewer_review != current.viewer_review {
        events.push(Trigger::ViewerReviewed);
    }
    events
}

pub fn evaluate(
    config: &RulesConfig,
    facts: &Facts,
    events: &[Event],
    state: &EngineState,
    now: u64,
) -> Evaluation {
    let mut new_state = state.clone();
    let ledger = state.ledger.clone();
    let world = World::new(config, facts, &ledger);
    let mut trace = Vec::new();
    let mut ci_records = Vec::new();
    let mut emitter = Emitter {
        out: Vec::new(),
        state: &mut new_state,
    };
    let ctx = |pr, workspace| Ctx {
        world: &world,
        pr,
        workspace,
    };
    let rules_for = |trigger: &Trigger| -> Vec<&Rule> {
        config
            .rules
            .iter()
            .filter(|rule| &rule.trigger == trigger)
            .collect()
    };

    for rule in rules_for(&Trigger::Tick(TickSubject::Workspaces)) {
        for ws in facts.workspaces.iter().filter(|ws| ws.is_ordinary()) {
            if world.applies(&rule.scope, Some(&ws.repo_id))
                && !(rule.respect_hold && world.is_held(ws))
            {
                run_rule(
                    rule,
                    ctx(world.linked_pr(ws), Some(ws)),
                    &mut emitter,
                    now,
                    None,
                );
            }
        }
    }

    if let Some(snapshot) = world.snapshot {
        let mut entered: Vec<(&Workspace, StatusId)> = Vec::new();
        if world.gated {
            for group in &config.groups {
                for ws in facts.workspaces.iter().filter(|ws| world.is_target(ws)) {
                    if group.respect_hold && world.is_held(ws) {
                        continue;
                    }
                    let Some(pr) = world.linked_pr(ws) else {
                        continue;
                    };
                    let c = ctx(Some(pr), Some(ws));
                    let matched = first_match(group, c);
                    let mut record = TargetTrace {
                        workspace_id: ws.id.clone(),
                        group: group.id.clone(),
                        condition: None,
                        status: None,
                    };
                    if let Some(entry) = matched {
                        record.condition = entry.condition.clone();
                        for action in &entry.actions {
                            emitter.emit(action, c, None);
                        }
                        if let Outcome::MoveTo(status) = &entry.outcome {
                            record.status = Some(status.clone());
                            if ws.status.as_ref() != Some(status) {
                                emitter.out.push(PlannedAction::SetColumn {
                                    workspace_id: ws.id.clone(),
                                    execution_host: ws.execution_host.clone(),
                                    status: status.clone(),
                                    condition: entry.condition.clone(),
                                });
                                for action in &entry.on_enter {
                                    emitter.emit(action, c, None);
                                }
                                entered.push((ws, status.clone()));
                            }
                        }
                    }
                    trace.push(record);
                }
            }
        }

        for rule in rules_for(&Trigger::Tick(TickSubject::ReviewRequests)) {
            if !world.applies(&rule.scope, None) {
                continue;
            }
            let mut fired = 0usize;
            for pr in &snapshot.review_requested {
                if rule.max_per_evaluation.is_some_and(|max| fired >= max) {
                    break;
                }
                if run_rule(rule, ctx(Some(pr), None), &mut emitter, now, None) {
                    fired += 1;
                }
            }
        }

        for (ws, status) in &entered {
            for rule in rules_for(&Trigger::Entered(status.clone())) {
                if world.applies(&rule.scope, Some(&ws.repo_id)) {
                    run_rule(
                        rule,
                        ctx(world.linked_pr(ws), Some(ws)),
                        &mut emitter,
                        now,
                        None,
                    );
                }
            }
        }

        for rule in rules_for(&Trigger::Tick(TickSubject::LinkedWorkspaces)) {
            for ws in facts.workspaces.iter().filter(|ws| world.is_target(ws)) {
                if !world.applies(&rule.scope, Some(&ws.repo_id))
                    || (rule.respect_hold && world.is_held(ws))
                {
                    continue;
                }
                if let Some(pr) = world.linked_pr(ws) {
                    run_rule(rule, ctx(Some(pr), Some(ws)), &mut emitter, now, None);
                }
            }
        }

        let mut memo = BTreeMap::new();
        for pr in snapshot.linked.iter().chain(&snapshot.review_requested) {
            let key = pr.match_key();
            let current = pr_memo(pr, &world);
            if let Some(previous) = state.pr_memo.get(&key) {
                for trigger in derived_events(previous, &current) {
                    run_event_rules(&world, &trigger, pr, &mut emitter, now);
                }
            }
            memo.insert(key, current);
        }
        emitter.state.pr_memo = memo;
    }

    for event in events {
        match event {
            Event::WorkspaceCreated { workspace_id } => {
                let Some(ws) = facts.workspaces.iter().find(|ws| &ws.id == workspace_id) else {
                    continue;
                };
                for rule in rules_for(&Trigger::WorkspaceCreated) {
                    if world.applies(&rule.scope, Some(&ws.repo_id)) {
                        run_rule(
                            rule,
                            ctx(world.linked_pr(ws), Some(ws)),
                            &mut emitter,
                            now,
                            None,
                        );
                    }
                }
            }
            Event::UserAction {
                workspace_id,
                action,
                variables,
            } => {
                let Some(ws) = facts.workspaces.iter().find(|ws| &ws.id == workspace_id) else {
                    continue;
                };
                for rule in rules_for(&Trigger::UserAction(action.clone())) {
                    if world.applies(&rule.scope, Some(&ws.repo_id)) {
                        run_rule(
                            rule,
                            ctx(world.linked_pr(ws), Some(ws)),
                            &mut emitter,
                            now,
                            Some(variables),
                        );
                    }
                }
            }
            Event::ChecksObserved(observed) => {
                for rule in rules_for(&Trigger::CheckFailed) {
                    if !world.applies(&rule.scope, Some(&observed.repo_id)) {
                        continue;
                    }
                    if let Some(record) =
                        run_check_failed(rule, observed, &world, &mut emitter, now)
                    {
                        ci_records.push(record);
                    }
                }
            }
        }
    }

    let mut actions = emitter.out;
    actions.sort_by_key(PlannedAction::phase);
    Evaluation {
        actions,
        trace,
        ci_triage: ci_records,
        state: new_state,
    }
}

fn run_event_rules(
    world: &World<'_>,
    trigger: &Trigger,
    pr: &PullRequest,
    emitter: &mut Emitter<'_>,
    now: u64,
) {
    let key = pr.match_key();
    let linked: Vec<&Workspace> = world
        .workspaces
        .iter()
        .filter(|ws| world.is_target(ws))
        .filter(|ws| ws.pr.as_ref().is_some_and(|link| link.match_key() == key))
        .collect();
    for rule in world
        .config
        .rules
        .iter()
        .filter(|rule| &rule.trigger == trigger)
    {
        if linked.is_empty() {
            if world.applies(&rule.scope, None) {
                let ctx = Ctx {
                    world,
                    pr: Some(pr),
                    workspace: None,
                };
                run_rule(rule, ctx, emitter, now, None);
            }
            continue;
        }
        for ws in &linked {
            if world.applies(&rule.scope, Some(&ws.repo_id))
                && !(rule.respect_hold && world.is_held(ws))
            {
                let ctx = Ctx {
                    world,
                    pr: Some(pr),
                    workspace: Some(ws),
                };
                run_rule(rule, ctx, emitter, now, None);
            }
        }
    }
}

fn run_check_failed(
    rule: &Rule,
    observed: &ChecksObserved,
    world: &World<'_>,
    emitter: &mut Emitter<'_>,
    now: u64,
) -> Option<CiTriageRecord> {
    let triage = rule.guards.iter().find_map(|guard| match guard {
        Guard::CiTriage {
            max_per_pr,
            window_ms,
        } => Some((*max_per_pr, *window_ms)),
        Guard::RateLimit { .. } => None,
    });
    let branch = observed
        .branch
        .strip_prefix("refs/heads/")
        .unwrap_or(&observed.branch);
    let workspace = world
        .workspaces
        .iter()
        .find(|ws| ws.repo_id == observed.repo_id && ws.branch_name() == Some(branch));
    let pr = workspace.and_then(|ws| world.linked_pr(ws));
    let ctx = Ctx {
        world,
        pr,
        workspace,
    };
    let Some((max_per_pr, window_ms)) = triage else {
        if !observed
            .checks
            .iter()
            .any(crate::facts::Check::is_triage_failure)
        {
            return None;
        }
        run_rule(rule, ctx, emitter, now, None);
        return None;
    };
    let (outcome, memory) = ci_triage::decide_ci_triage(
        observed,
        world.workspaces,
        &emitter.state.ci_triage,
        now,
        max_per_pr,
        window_ms,
    )?;
    if let Some(memory) = memory {
        emitter.state.ci_triage = memory;
    }
    if outcome.decision == ci_triage::CiTriageDecision::Started && ctx.eval(&rule.when) {
        for action in &rule.actions {
            match action {
                ActionSpec::LaunchAgent(spec) => {
                    let instructions = match &spec.prompt {
                        PromptSpec::CiTriage { instructions } => instructions.clone(),
                        other => render_prompt(other, &ctx.variables(None, None)),
                    };
                    let prompt = ci_triage_prompt(
                        observed.pr_number,
                        observed.head_sha.as_deref(),
                        &outcome.runs,
                        &instructions,
                    );
                    emitter.out.push(PlannedAction::LaunchAgent {
                        workspace_id: outcome.workspace_id.clone(),
                        launch: AgentLaunch {
                            agent: spec.agent.clone(),
                            prompt,
                            view: spec.view,
                            background: spec.background,
                            title: spec.title.clone(),
                            launch_source: spec.launch_source.clone(),
                        },
                    });
                }
                other => emitter.emit(other, ctx, None),
            }
        }
    }
    Some(CiTriageRecord {
        rule: rule.id.clone(),
        trigger: observed.trigger.clone(),
        repo_id: observed.repo_id.clone(),
        pr_number: observed.pr_number,
        branch: observed.branch.clone(),
        head_sha: observed.head_sha.clone(),
        outcome,
    })
}

fn first_match<'g>(group: &'g RuleGroup, ctx: Ctx<'_, '_>) -> Option<&'g crate::model::GroupEntry> {
    group
        .entries
        .iter()
        .filter(|entry| entry.outcome != Outcome::Skip)
        .find(|entry| ctx.eval(&entry.when))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub condition: Option<String>,
    pub status: Option<StatusId>,
}

pub fn classify(
    config: &RulesConfig,
    group: &RuleGroup,
    pr: &PullRequest,
    snapshot: &Snapshot,
    ledger: &[String],
) -> Option<Classification> {
    let facts = Facts {
        workspaces: Vec::new(),
        snapshot: Some(snapshot.clone()),
    };
    let world = World::new(config, &facts, ledger);
    let ctx = Ctx {
        world: &world,
        pr: Some(pr),
        workspace: None,
    };
    first_match(group, ctx).map(|entry| Classification {
        condition: entry.condition.clone(),
        status: match &entry.outcome {
            Outcome::MoveTo(status) => Some(status.clone()),
            _ => None,
        },
    })
}
