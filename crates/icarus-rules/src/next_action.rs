use crate::facts::{
    CheckState, MergeStateStatus, Mergeable, PrLifecycle, PullRequest, ReviewDecision, Stack,
    StackEntry,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeStateInput {
    pub state: Option<PrLifecycle>,
    pub mergeable: Option<Mergeable>,
    pub merge_state_status: Option<MergeStateStatus>,
    pub review_decision: Option<ReviewDecision>,
    pub auto_merge_enabled: bool,
    pub auto_merge_allowed: Option<bool>,
    pub merge_queue_required: Option<bool>,
    pub in_merge_queue: bool,
    pub checks: Option<CheckState>,
}

impl From<&PullRequest> for MergeStateInput {
    fn from(pr: &PullRequest) -> Self {
        Self {
            state: Some(pr.lifecycle()),
            mergeable: pr.mergeable,
            merge_state_status: pr.merge_state_status,
            review_decision: pr.review_decision,
            auto_merge_enabled: pr.auto_merge.is_some(),
            auto_merge_allowed: pr.auto_merge_allowed,
            merge_queue_required: pr.merge_queue_required,
            in_merge_queue: pr.is_in_merge_queue,
            checks: pr.checks_rollup,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStateKind {
    Merged,
    Closed,
    Draft,
    InMergeQueue,
    ReviewRequired,
    ChangesRequested,
    Conflicts,
    MergeQueue,
    MergeQueueAutoMergeOn,
    MergeStatusUnavailable,
    Behind,
    Blocked,
    MergeableChecksFailed,
    MergeableChecksPending,
    MergeableChecksPassed,
    Mergeable,
    ChecksPassed,
    Checking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoMergeIntent {
    DisableAutoMerge,
    MergeWhenReady,
    EnableAutoMerge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeStateDecision {
    pub kind: MergeStateKind,
    pub direct_merge_available: bool,
    pub auto_merge: Option<AutoMergeIntent>,
}

fn is_conflicting(input: &MergeStateInput) -> bool {
    input.mergeable == Some(Mergeable::Conflicting)
        || input.merge_state_status == Some(MergeStateStatus::Dirty)
}

pub fn can_show_auto_merge_control(input: &MergeStateInput) -> bool {
    if input.state != Some(PrLifecycle::Open) || input.in_merge_queue {
        return false;
    }
    if input.auto_merge_enabled {
        return true;
    }
    if is_conflicting(input) {
        return false;
    }
    if input.merge_queue_required == Some(true) {
        return true;
    }
    // Why: missing merge metadata is not evidence of an unmet branch requirement.
    input.merge_state_status != Some(MergeStateStatus::Unstable)
        && input.auto_merge_allowed != Some(false)
        && (matches!(
            input.review_decision,
            Some(ReviewDecision::ReviewRequired | ReviewDecision::ChangesRequested)
        ) || matches!(
            input.merge_state_status,
            Some(MergeStateStatus::Blocked | MergeStateStatus::Behind)
        ))
}

fn merge_state_kind(input: &MergeStateInput) -> MergeStateKind {
    match input.state {
        Some(PrLifecycle::Merged) => return MergeStateKind::Merged,
        Some(PrLifecycle::Closed) => return MergeStateKind::Closed,
        Some(PrLifecycle::Draft) => return MergeStateKind::Draft,
        _ => {}
    }
    if input.in_merge_queue {
        return MergeStateKind::InMergeQueue;
    }
    match input.review_decision {
        Some(ReviewDecision::ReviewRequired) => return MergeStateKind::ReviewRequired,
        Some(ReviewDecision::ChangesRequested) => return MergeStateKind::ChangesRequested,
        _ => {}
    }
    if is_conflicting(input) {
        return MergeStateKind::Conflicts;
    }
    if input.merge_queue_required == Some(true) {
        return if input.auto_merge_enabled {
            MergeStateKind::MergeQueueAutoMergeOn
        } else {
            MergeStateKind::MergeQueue
        };
    }
    let checks_passed = input.checks == Some(CheckState::Success);
    if input.mergeable.is_none() && input.merge_state_status.is_none() {
        return if checks_passed {
            MergeStateKind::ChecksPassed
        } else {
            MergeStateKind::MergeStatusUnavailable
        };
    }
    match input.merge_state_status {
        Some(MergeStateStatus::Behind) => return MergeStateKind::Behind,
        Some(MergeStateStatus::Blocked) => return MergeStateKind::Blocked,
        _ => {}
    }
    let mergeable = input.mergeable == Some(Mergeable::Mergeable)
        || input
            .merge_state_status
            .is_some_and(MergeStateStatus::is_ready);
    if mergeable {
        return match input.checks {
            Some(CheckState::Failure) => MergeStateKind::MergeableChecksFailed,
            Some(CheckState::Pending) => MergeStateKind::MergeableChecksPending,
            Some(CheckState::Success) => MergeStateKind::MergeableChecksPassed,
            _ => MergeStateKind::Mergeable,
        };
    }
    if checks_passed {
        MergeStateKind::ChecksPassed
    } else {
        MergeStateKind::Checking
    }
}

pub fn resolve_merge_state(input: &MergeStateInput) -> MergeStateDecision {
    let kind = merge_state_kind(input);
    let auto_merge = can_show_auto_merge_control(input).then(|| {
        if input.auto_merge_enabled {
            AutoMergeIntent::DisableAutoMerge
        } else if input.merge_queue_required == Some(true) {
            AutoMergeIntent::MergeWhenReady
        } else {
            AutoMergeIntent::EnableAutoMerge
        }
    });
    MergeStateDecision {
        kind,
        direct_merge_available: matches!(
            kind,
            MergeStateKind::MergeableChecksFailed
                | MergeStateKind::MergeableChecksPending
                | MergeStateKind::MergeableChecksPassed
                | MergeStateKind::Mergeable
                | MergeStateKind::ChecksPassed
        ),
        auto_merge,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackMergeTarget {
    pub number: u64,
    pub position: u32,
    pub count: usize,
}

// Why every entry: merging the top open PR lands every PR below it.
pub fn stack_merge_target(stack: &Stack) -> Option<StackMergeTarget> {
    let open: Vec<&StackEntry> = stack
        .entries
        .iter()
        .filter(|entry| entry.state != PrLifecycle::Merged)
        .collect();
    let ready = open.iter().all(|entry| {
        entry.state == PrLifecycle::Open
            && !entry.in_merge_queue
            && entry
                .merge_state_status
                .is_some_and(MergeStateStatus::is_ready)
    });
    if stack.entries.len() != stack.size as usize || open.is_empty() || !ready {
        return None;
    }
    let top = open.iter().copied().reduce(|top, candidate| {
        if candidate.position > top.position {
            candidate
        } else {
            top
        }
    })?;
    Some(StackMergeTarget {
        number: top.number,
        position: top.position,
        count: open.len(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackBlockerReason {
    Draft,
    Closed,
    Conflicts,
    ChangesRequested,
    ReviewRequired,
    Behind,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackBlocker {
    pub reason: StackBlockerReason,
    pub pr_number: u64,
}

fn stack_entry_blocker(entry: &StackEntry) -> Option<StackBlockerReason> {
    use StackBlockerReason as R;
    match entry.state {
        PrLifecycle::Draft => return Some(R::Draft),
        PrLifecycle::Closed => return Some(R::Closed),
        _ => {}
    }
    if entry.mergeable == Some(Mergeable::Conflicting)
        || entry.merge_state_status == Some(MergeStateStatus::Dirty)
    {
        return Some(R::Conflicts);
    }
    match entry.review_decision {
        Some(ReviewDecision::ChangesRequested) => return Some(R::ChangesRequested),
        Some(ReviewDecision::ReviewRequired) => return Some(R::ReviewRequired),
        _ => {}
    }
    match entry.merge_state_status {
        Some(MergeStateStatus::Behind) => Some(R::Behind),
        Some(MergeStateStatus::Blocked) => Some(R::Blocked),
        _ => None,
    }
}

pub fn stack_merge_blocker(stack: &Stack) -> Option<StackBlocker> {
    let mut scope: Vec<&StackEntry> = stack
        .entries
        .iter()
        .filter(|entry| entry.position <= stack.position)
        .collect();
    scope.sort_by_key(|entry| entry.position);
    scope.into_iter().find_map(|entry| {
        stack_entry_blocker(entry).map(|reason| StackBlocker {
            reason,
            pr_number: entry.number,
        })
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum MergeStep {
    Merge,
    EnableAutoMerge,
    DisableAutoMerge,
    AddToMergeQueue,
    MergeStack {
        target: u64,
        count: usize,
        via_merge_queue: bool,
    },
    EnableStackAutoMerge,
    DisableStackAutoMerge,
}

// Why stacks get their own steps: a stack merge lands every PR below, so stack readiness decides.
pub fn merge_step(pr: &PullRequest) -> Option<MergeStep> {
    if pr.lifecycle() != PrLifecycle::Open {
        return None;
    }
    if let Some(stack) = &pr.stack {
        return stack_merge_step(pr, stack);
    }
    let decision = resolve_merge_state(&MergeStateInput::from(pr));
    if decision.direct_merge_available {
        return Some(MergeStep::Merge);
    }
    decision.auto_merge.map(|intent| match intent {
        AutoMergeIntent::DisableAutoMerge => MergeStep::DisableAutoMerge,
        AutoMergeIntent::MergeWhenReady => MergeStep::AddToMergeQueue,
        AutoMergeIntent::EnableAutoMerge => MergeStep::EnableAutoMerge,
    })
}

fn stack_merge_step(pr: &PullRequest, stack: &Stack) -> Option<MergeStep> {
    if stack.auto_merge.is_some() {
        return Some(MergeStep::DisableStackAutoMerge);
    }
    if let Some(target) = stack_merge_target(stack) {
        return Some(MergeStep::MergeStack {
            target: target.number,
            count: target.count,
            via_merge_queue: pr.merge_queue_required == Some(true)
                || stack.merge_queue_required == Some(true),
        });
    }
    let unmerged = stack
        .entries
        .iter()
        .filter(|entry| entry.state != PrLifecycle::Merged);
    let can_wait = stack.entries.len() == stack.size as usize
        && unmerged.clone().next().is_some()
        && unmerged.into_iter().all(|entry| {
            entry.state == PrLifecycle::Open
                && !entry.in_merge_queue
                && entry.mergeable != Some(Mergeable::Conflicting)
                && entry.merge_state_status != Some(MergeStateStatus::Dirty)
        });
    can_wait.then_some(MergeStep::EnableStackAutoMerge)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionPr {
    pub state: PrLifecycle,
    pub conflicting: bool,
    pub merge: Option<MergeStep>,
    pub head_is_local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionInput {
    pub has_uncommitted_changes: bool,
    pub has_upstream: bool,
    pub unpushed_commits: u32,
    pub has_local_conflicts: bool,
    pub is_default_branch: bool,
    pub pull_request: Option<ActionPr>,
    // Why: an unanswered lookup is not "no pull request", and must not offer Create PR.
    pub pull_request_known: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NextAction {
    ResolveConflicts,
    CreatePullRequest,
    CommitAndPush,
    ReadyForReview,
    Merge(MergeStep),
}

impl NextAction {
    pub fn name(self) -> &'static str {
        match self {
            Self::ResolveConflicts => "resolve-conflicts",
            Self::CreatePullRequest => "create-pull-request",
            Self::CommitAndPush => "commit-and-push",
            Self::ReadyForReview => "ready-for-review",
            Self::Merge(MergeStep::Merge) => "merge",
            Self::Merge(MergeStep::EnableAutoMerge) => "enable-auto-merge",
            Self::Merge(MergeStep::DisableAutoMerge) => "disable-auto-merge",
            Self::Merge(MergeStep::AddToMergeQueue) => "add-to-merge-queue",
            Self::Merge(MergeStep::MergeStack { .. }) => "merge-stack",
            Self::Merge(MergeStep::EnableStackAutoMerge) => "enable-stack-auto-merge",
            Self::Merge(MergeStep::DisableStackAutoMerge) => "disable-stack-auto-merge",
        }
    }
}

// Why this order: a stopped merge cannot be committed, and Create PR's prompt commits and pushes too.
pub fn resolve_next_action(input: &ActionInput) -> Option<NextAction> {
    if input.has_local_conflicts {
        return Some(NextAction::ResolveConflicts);
    }
    let pr = input.pull_request.as_ref();
    if pr.is_none() && input.pull_request_known && !input.is_default_branch {
        return Some(NextAction::CreatePullRequest);
    }
    if input.has_uncommitted_changes || (input.has_upstream && input.unpushed_commits > 0) {
        return Some(NextAction::CommitAndPush);
    }
    let pr = pr.filter(|pr| matches!(pr.state, PrLifecycle::Open | PrLifecycle::Draft))?;
    if pr.conflicting {
        return Some(NextAction::ResolveConflicts);
    }
    if pr.state == PrLifecycle::Draft {
        return Some(NextAction::ReadyForReview);
    }
    // Why: merging ships what the PR holds, so it waits until that is the local work.
    if pr.head_is_local {
        pr.merge.map(NextAction::Merge)
    } else {
        None
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalGitState {
    pub has_uncommitted_changes: bool,
    pub has_upstream: bool,
    pub unpushed_commits: u32,
    pub has_local_conflicts: bool,
    pub is_default_branch: bool,
    pub head_oid: Option<String>,
}

pub fn next_action(
    git: &LocalGitState,
    pr: Option<&PullRequest>,
    pull_request_known: bool,
) -> Option<NextAction> {
    resolve_next_action(&ActionInput {
        has_uncommitted_changes: git.has_uncommitted_changes,
        has_upstream: git.has_upstream,
        unpushed_commits: if git.has_upstream {
            git.unpushed_commits
        } else {
            0
        },
        has_local_conflicts: git.has_local_conflicts,
        is_default_branch: git.is_default_branch,
        pull_request: pr.map(|pr| ActionPr {
            state: pr.lifecycle(),
            conflicting: pr.mergeable == Some(Mergeable::Conflicting),
            merge: merge_step(pr),
            head_is_local: !pr.head_ref_oid.is_empty()
                && git.head_oid.as_deref() == Some(pr.head_ref_oid.as_str()),
        }),
        pull_request_known,
    })
}
