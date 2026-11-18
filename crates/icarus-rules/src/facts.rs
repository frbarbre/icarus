use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type StatusId = String;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRef {
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    pub fn new(owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            name: name.into(),
        }
    }

    pub fn match_key(&self, number: u64) -> String {
        format!(
            "{}/{}#{}",
            self.owner.to_lowercase(),
            self.name.to_lowercase(),
            number
        )
    }

    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

// Why the reviewed commit: a re-request after a review is new work, so it gets its own key.
pub fn handled_key(repo: &RepoRef, number: u64, reviewed_commit: Option<&str>) -> String {
    let key = format!("{}/{}#{}", repo.owner, repo.name, number);
    match reviewed_commit {
        Some(commit) if !commit.is_empty() => format!("{key}@{commit}"),
        _ => key,
    }
}

pub fn is_handled_key_for_pull_request(entry: &str, pull_request_key: &str) -> bool {
    entry == pull_request_key
        || entry
            .strip_prefix(pull_request_key)
            .is_some_and(|rest| rest.starts_with('@'))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PrState {
    #[default]
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrLifecycle {
    Open,
    Draft,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Mergeable {
    Mergeable,
    Conflicting,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MergeStateStatus {
    Clean,
    HasHooks,
    Unstable,
    Behind,
    Blocked,
    Dirty,
    Draft,
    #[serde(other)]
    Unknown,
}

impl MergeStateStatus {
    pub fn is_ready(self) -> bool {
        matches!(self, Self::Clean | Self::HasHooks | Self::Unstable)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub login: String,
    pub state: ReviewState,
    #[serde(default)]
    pub commit_oid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_avatar_url: Option<String>,
}

impl Review {
    pub fn new(login: impl Into<String>, state: ReviewState, commit_oid: Option<&str>) -> Self {
        Self {
            login: login.into(),
            state,
            commit_oid: commit_oid.map(str::to_owned),
            author_avatar_url: None,
        }
    }
}

// Why kept: a re-requested viewer's comment-only review shows up only here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewerReview {
    pub state: String,
    #[serde(default)]
    pub commit_oid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RequestedReviewer {
    User {
        login: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        avatar_url: Option<String>,
    },
    Team {
        slug: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        avatar_url: Option<String>,
    },
}

impl RequestedReviewer {
    pub fn user(login: impl Into<String>) -> Self {
        Self::User {
            login: login.into(),
            avatar_url: None,
        }
    }

    pub fn team(slug: impl Into<String>) -> Self {
        Self::Team {
            slug: slug.into(),
            avatar_url: None,
        }
    }

    pub fn with_avatar_url(mut self, url: Option<String>) -> Self {
        match &mut self {
            Self::User { avatar_url, .. } | Self::Team { avatar_url, .. } => *avatar_url = url,
        }
        self
    }

    pub fn avatar_url(&self) -> Option<&str> {
        match self {
            Self::User { avatar_url, .. } | Self::Team { avatar_url, .. } => avatar_url.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CheckRunStatus {
    Queued,
    InProgress,
    Completed,
    Waiting,
    Requested,
    Pending,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CheckConclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    Skipped,
    TimedOut,
    ActionRequired,
    Stale,
    StartupFailure,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StatusContextState {
    Success,
    Failure,
    Error,
    Pending,
    Expected,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CheckKind {
    Run {
        status: CheckRunStatus,
        #[serde(default)]
        conclusion: Option<CheckConclusion>,
    },
    Status {
        state: StatusContextState,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Success,
    Failure,
    Pending,
    Neutral,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub kind: CheckKind,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub required: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

impl Check {
    pub fn run(
        name: impl Into<String>,
        status: CheckRunStatus,
        conclusion: Option<CheckConclusion>,
        url: Option<&str>,
    ) -> Self {
        Self {
            name: name.into(),
            kind: CheckKind::Run { status, conclusion },
            url: url.map(str::to_owned),
            required: None,
            started_at: None,
            completed_at: None,
        }
    }

    pub fn with_times(mut self, started_at: Option<String>, completed_at: Option<String>) -> Self {
        self.started_at = started_at;
        self.completed_at = completed_at;
        self
    }

    pub fn status(name: impl Into<String>, state: StatusContextState, url: Option<&str>) -> Self {
        Self {
            name: name.into(),
            kind: CheckKind::Status { state },
            url: url.map(str::to_owned),
            required: None,
            started_at: None,
            completed_at: None,
        }
    }

    pub fn state(&self) -> CheckState {
        match self.kind {
            CheckKind::Run { status, conclusion } => {
                if status != CheckRunStatus::Completed {
                    return CheckState::Pending;
                }
                match conclusion {
                    Some(CheckConclusion::Success) => CheckState::Success,
                    Some(CheckConclusion::Neutral | CheckConclusion::Skipped) => {
                        CheckState::Neutral
                    }
                    Some(_) => CheckState::Failure,
                    None => CheckState::Pending,
                }
            }
            CheckKind::Status { state } => match state {
                StatusContextState::Success => CheckState::Success,
                StatusContextState::Failure | StatusContextState::Error => CheckState::Failure,
                _ => CheckState::Pending,
            },
        }
    }

    pub fn is_running(&self) -> bool {
        match self.kind {
            CheckKind::Run { status, .. } => status != CheckRunStatus::Completed,
            CheckKind::Status { state } => state == StatusContextState::Pending,
        }
    }

    // Why stale and startup failures: Orca's REST mapping reports both as failure, so triage takes them.
    pub fn is_triage_failure(&self) -> bool {
        match self.kind {
            CheckKind::Run {
                status: CheckRunStatus::Completed,
                conclusion: Some(conclusion),
            } => matches!(
                conclusion,
                CheckConclusion::Failure
                    | CheckConclusion::TimedOut
                    | CheckConclusion::Stale
                    | CheckConclusion::StartupFailure
            ),
            CheckKind::Status { state } => {
                matches!(
                    state,
                    StatusContextState::Failure | StatusContextState::Error
                )
            }
            CheckKind::Run { .. } => false,
        }
    }

    pub fn is_waiting_approval(&self) -> bool {
        matches!(
            self.kind,
            CheckKind::Run {
                status: CheckRunStatus::Waiting,
                ..
            }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MergeQueueEntryState {
    Queued,
    AwaitingChecks,
    Mergeable,
    Unmergeable,
    Locked,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeQueueEntry {
    pub state: MergeQueueEntryState,
    #[serde(default)]
    pub position: Option<u32>,
    #[serde(default)]
    pub eta_seconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoMergeRequest {
    #[serde(default)]
    pub method: Option<MergeMethod>,
    #[serde(default)]
    pub enabled_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackEntry {
    pub position: u32,
    pub number: u64,
    pub state: PrLifecycle,
    #[serde(default)]
    pub mergeable: Option<Mergeable>,
    #[serde(default)]
    pub merge_state_status: Option<MergeStateStatus>,
    #[serde(default)]
    pub review_decision: Option<ReviewDecision>,
    #[serde(default)]
    pub in_merge_queue: bool,
    #[serde(default)]
    pub head_oid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_ref_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks_rollup: Option<CheckState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_avatar_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackAutoMergeMode {
    Native,
    AppManaged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackAutoMerge {
    pub mode: StackAutoMergeMode,
    #[serde(default)]
    pub enabled_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stack {
    pub number: u64,
    pub position: u32,
    pub size: u32,
    pub base_ref_name: String,
    #[serde(default)]
    pub entries: Vec<StackEntry>,
    #[serde(default)]
    pub merge_queue_required: Option<bool>,
    #[serde(default)]
    pub auto_merge: Option<StackAutoMerge>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PullRequest {
    pub repo: RepoRef,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_avatar_url: Option<String>,
    pub state: PrState,
    pub is_draft: bool,
    pub head_ref_name: String,
    pub base_ref_name: String,
    pub head_ref_oid: String,
    pub base_ref_oid: Option<String>,
    pub mergeable: Option<Mergeable>,
    pub merge_state_status: Option<MergeStateStatus>,
    pub review_decision: Option<ReviewDecision>,
    pub requested_reviewers: Vec<RequestedReviewer>,
    pub latest_reviews: Vec<Review>,
    pub latest_opinionated_reviews: Option<Vec<Review>>,
    pub viewer_latest_review: Option<ViewerReview>,
    pub checks: Vec<Check>,
    pub checks_rollup: Option<CheckState>,
    pub is_in_merge_queue: bool,
    pub merge_queue_entry: Option<MergeQueueEntry>,
    pub merge_queue_required: Option<bool>,
    pub auto_merge: Option<AutoMergeRequest>,
    pub auto_merge_allowed: Option<bool>,
    pub stack: Option<Stack>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl PullRequest {
    pub fn match_key(&self) -> String {
        self.repo.match_key(self.number)
    }

    pub fn lifecycle(&self) -> PrLifecycle {
        match self.state {
            PrState::Merged => PrLifecycle::Merged,
            PrState::Closed => PrLifecycle::Closed,
            PrState::Open if self.is_draft => PrLifecycle::Draft,
            PrState::Open => PrLifecycle::Open,
        }
    }

    pub fn pending_user_reviewers(&self) -> Vec<String> {
        self.requested_reviewers
            .iter()
            .filter_map(|reviewer| match reviewer {
                RequestedReviewer::User { login, .. } => Some(login.to_lowercase()),
                RequestedReviewer::Team { .. } => None,
            })
            .collect()
    }

    pub fn has_team_reviewer(&self) -> bool {
        self.requested_reviewers
            .iter()
            .any(|reviewer| matches!(reviewer, RequestedReviewer::Team { .. }))
    }

    pub fn opinionated_or_latest(&self) -> &[Review] {
        self.latest_opinionated_reviews
            .as_deref()
            .unwrap_or(&self.latest_reviews)
    }

    pub fn is_authored_by(&self, login: &str) -> bool {
        self.author
            .as_deref()
            .is_some_and(|author| author.to_lowercase() == login.to_lowercase())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrLink {
    pub repo: RepoRef,
    pub number: u64,
}

impl PrLink {
    pub fn match_key(&self) -> String {
        self.repo.match_key(self.number)
    }
}

pub const LOCAL_HOST: &str = "local";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspace {
    pub id: String,
    pub repo_id: String,
    pub execution_host: String,
    pub repo_is_remote: bool,
    pub display_name: String,
    pub path: String,
    pub branch: Option<String>,
    /// What the workspace's Changes read against, e.g. `origin/main`.
    pub base_ref: Option<String>,
    pub status: Option<StatusId>,
    pub head_oid: String,
    pub dirty: bool,
    pub has_upstream: bool,
    pub unpushed_commits: u32,
    pub has_pending_review_comments: bool,
    // Why optional: a workspace found on disk has no creation time and is never auto-placed.
    pub created_at: Option<u64>,
    pub archived: bool,
    pub bare: bool,
    pub main: bool,
    pub pr: Option<PrLink>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            id: String::new(),
            repo_id: String::new(),
            execution_host: LOCAL_HOST.to_owned(),
            repo_is_remote: false,
            display_name: String::new(),
            path: String::new(),
            branch: None,
            base_ref: None,
            status: None,
            head_oid: String::new(),
            dirty: false,
            has_upstream: false,
            unpushed_commits: 0,
            has_pending_review_comments: false,
            created_at: None,
            archived: false,
            bare: false,
            main: false,
            pr: None,
        }
    }
}

/// False when `base_ref` is a branch other than `target`; a base pinned to a commit (a review's
/// "since review") follows the target by itself, so it counts as following.
pub fn base_follows(base_ref: Option<&str>, target: &str) -> bool {
    let Some(base) = base_ref else {
        return false;
    };
    let base = base.strip_prefix("refs/remotes/").unwrap_or(base);
    match base.strip_prefix("origin/") {
        Some(branch) => branch == target,
        None => true,
    }
}

impl Workspace {
    pub fn base_follows(&self, target: &str) -> bool {
        base_follows(self.base_ref.as_deref(), target)
    }

    pub fn branch_name(&self) -> Option<&str> {
        self.branch
            .as_deref()
            .map(|branch| branch.strip_prefix("refs/heads/").unwrap_or(branch))
            .filter(|branch| !branch.is_empty())
    }

    pub fn is_ordinary(&self) -> bool {
        !self.archived && !self.bare && !self.main
    }

    pub fn is_local_host(&self) -> bool {
        self.execution_host == LOCAL_HOST
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub viewer_login: Option<String>,
    pub linked: Vec<PullRequest>,
    pub review_requested: Vec<PullRequest>,
    pub team_members: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Facts {
    pub workspaces: Vec<Workspace>,
    pub snapshot: Option<Snapshot>,
}
