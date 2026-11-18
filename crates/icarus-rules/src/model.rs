use crate::facts::{MergeMethod, MergeQueueEntryState, MergeStateStatus, Mergeable, StatusId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusDef {
    pub id: StatusId,
    pub label: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cond", rename_all = "snake_case")]
pub enum Cond {
    Always,
    Never,
    All { of: Vec<Cond> },
    Any { of: Vec<Cond> },
    Not { inner: Box<Cond> },

    PrMerged,
    PrClosed,
    PrOpen,
    Draft,
    ViewerKnown,
    AuthoredByViewer,
    AuthoredBySomeoneElse,
    InMergeQueue,
    MergeQueueState { state: MergeQueueEntryState },
    Mergeable { is: Mergeable },
    MergeState { is: MergeStateStatus },
    UnansweredChangeRequest,
    CheckSucceeded { name: String },
    ChecksFailing,
    ChecksWaitingApproval,
    ApprovedByTeamAndEveryone { team: String },
    AwaitingOnlyTeam { team: String },
    ViewerOwed,
    ViewerFinished,
    ViewerInReviewLists,
    InReviewInbox,
    TeamReviewRequested,
    Handled { with_reviewed_commit: bool },
    AutoMergeEnabled,
    InStack,
    StackReady,
    StackAutoMergeEnabled,

    Linked,
    BranchHasWorkspace,
    HeadIsLocal,
    BaseFollowsTarget,
    HasPrHead,
    HeadBranchMatches,
    Dirty,
    HasPendingReviewComments,
    LocalHost,
    LocalCheckout,
    InColumn { status: StatusId },
    NoColumn,
    CreatedSince { at_ms: u64 },
}

impl std::ops::Not for Cond {
    type Output = Self;

    fn not(self) -> Self {
        Self::Not {
            inner: Box::new(self),
        }
    }
}

impl Cond {
    pub fn all(of: Vec<Cond>) -> Self {
        Self::All { of }
    }

    pub fn any(of: Vec<Cond>) -> Self {
        Self::Any { of }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "status", rename_all = "snake_case")]
pub enum Outcome {
    MoveTo(StatusId),
    // Why both: Orca's unmapped merge-queue/conflicts fall through, every other unmapped condition stops.
    Skip,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupEntry {
    #[serde(default)]
    pub condition: Option<String>,
    pub when: Cond,
    pub outcome: Outcome,
    #[serde(default)]
    pub actions: Vec<ActionSpec>,
    #[serde(default)]
    pub on_enter: Vec<ActionSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleGroup {
    pub id: String,
    #[serde(default = "yes")]
    pub respect_hold: bool,
    pub entries: Vec<GroupEntry>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TickSubject {
    Workspaces,
    LinkedWorkspaces,
    ReviewRequests,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "on", content = "arg", rename_all = "snake_case")]
pub enum Trigger {
    Tick(TickSubject),
    WorkspaceCreated,
    Entered(StatusId),
    PrHeadMoved,
    ReviewRequested,
    ReviewWithdrawn,
    ViewerReviewed,
    CheckFailed,
    UserAction(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", content = "repos", rename_all = "snake_case")]
pub enum RuleScope {
    Group,
    // Why: CI triage runs per project, outside the status rules' master switch.
    Repos(Vec<String>),
    Everywhere,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "guard", rename_all = "snake_case")]
pub enum Guard {
    RateLimit { max: u32, window_ms: u64 },
    CiTriage { max_per_pr: u32, window_ms: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub trigger: Trigger,
    pub scope: RuleScope,
    #[serde(default = "yes")]
    pub respect_hold: bool,
    pub when: Cond,
    #[serde(default)]
    pub guards: Vec<Guard>,
    pub actions: Vec<ActionSpec>,
    #[serde(default)]
    pub max_per_evaluation: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentView {
    #[default]
    Chat,
    Terminal,
    ClaudeWeb,
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "template", rename_all = "snake_case")]
pub enum PromptSpec {
    Rule(String),
    Conflict(String),
    Action(String),
    CiTriage { instructions: String },
    Provided,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLaunchSpec {
    pub agent: Option<String>,
    pub prompt: PromptSpec,
    #[serde(default)]
    pub rereview_prompt: Option<PromptSpec>,
    pub view: AgentView,
    pub background: bool,
    pub title: String,
    pub launch_source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgetHandled {
    Never,
    UnlessStillRequested,
}

/// The name of a layout under `layouts.<name>` (plan D32).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LayoutSpec(pub String);

impl LayoutSpec {
    pub fn review() -> Self {
        Self("review".to_owned())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrOperation {
    Merge,
    EnableAutoMerge,
    DisableAutoMerge,
    AddToMergeQueue,
    MergeStack,
    DisableStackAutoMerge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ActionSpec {
    SetColumn {
        status: StatusId,
    },
    DeleteWorkspace {
        force: bool,
        forget_handled: ForgetHandled,
    },
    CreateWorkspaceFromPr {
        initial_column: Option<StatusId>,
        max_attempts: Option<u32>,
        agent: Option<AgentLaunchSpec>,
        layout: Option<LayoutSpec>,
    },
    LaunchAgent(AgentLaunchSpec),
    RunCommand {
        command: String,
        local_only: bool,
        timeout_ms: u64,
    },
    SyncToPrHead,
    Notify {
        message: String,
    },
    OpenLayout {
        layout: LayoutSpec,
    },
    EnableStackAutoMerge {
        method: Option<MergeMethod>,
    },
    PullRequest {
        operation: PrOperation,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesConfig {
    pub enabled: bool,
    pub scope: Vec<String>,
    #[serde(default)]
    pub statuses: Vec<StatusDef>,
    #[serde(default)]
    pub hold_status: Option<StatusId>,
    #[serde(default)]
    pub groups: Vec<RuleGroup>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}
