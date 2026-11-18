use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use collections::{HashMap, HashSet};
use editor::Editor;
use gpui::{
    Action, App, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Hsla, Pixels, Render, Subscription, Task, WeakEntity, Window, actions, px, rgb,
};
use gpui_tokio::Tokio;
use icarus_client::Client;
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use ui::{Avatar, CommonAnimationExt as _, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent, PanelSizeState};

use crate::diff_tab::DiffBase;
use crate::links::{PreviewDeployment, ordered_previews};
use crate::pr_comments;
use crate::pr_store::{PrReviewFailed, PrReviewStore, ReviewOp, Viewer};
use crate::review::{
    ReviewDraft, ReviewSection, ReviewThread, ReviewView, ThreadComment as ReviewThreadComment,
};
use crate::sections::{Layout, SectionHeader, SectionKind, drag_preview, sections_menu};
use crate::status_style;

actions!(
    icarus,
    [
        /// Shows or hides the Checks panel: the pull request's checks and comments.
        ToggleGitHubPanel,
        /// Reloads this workspace's pull request from the Icarus server.
        RefreshGitHubPanel,
        /// Reveals Source Control and puts the cursor in the review summary.
        FocusReviewSummary,
    ]
);

const GITHUB_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
/// How often a window that isn't in front re-reads the PR status.
const GITHUB_BACKGROUND_INTERVAL: Duration = Duration::from_secs(60);
/// Opening the panel re-reads review threads older than this; they're never polled otherwise.
const THREADS_STALE: Duration = Duration::from_secs(300);
/// The trailing action slots of every row list, so icons line up whichever ones a row shows.
const SLOT: Pixels = px(20.);

/// The Checks panel's sections (github-review.md §9). Orca's ids, plus Icarus's Previews; Review
/// lives in Source Control, as in Orca.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrSection {
    Attention,
    Previews,
    Checks,
    PendingReview,
    Comments,
    Resolved,
}

impl SectionKind for PrSection {
    fn all() -> &'static [Self] {
        &[
            Self::Attention,
            Self::Previews,
            Self::Checks,
            Self::PendingReview,
            Self::Comments,
            Self::Resolved,
        ]
    }

    fn id(self) -> &'static str {
        match self {
            Self::Attention => "pr-attention",
            Self::Previews => "pr-previews",
            Self::Checks => "pr-checks",
            Self::PendingReview => "pr-pending-review",
            Self::Comments => "pr-comments",
            Self::Resolved => "pr-resolved",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Attention => "Needs attention",
            Self::Previews => "Previews",
            Self::Checks => "Checks",
            Self::PendingReview => "Pending review",
            Self::Comments => "Comments",
            Self::Resolved => "Resolved",
        }
    }
}

const SECTION_HEADER_HEIGHT: Pixels = px(28.);
/// The rendered PR description's key among the comment bodies.
const DESCRIPTION_KEY: &str = "pr-description";
const MIN_SECTION_HEIGHT: Pixels = px(48.);

/// A section's bottom edge being dragged to resize it.
#[derive(Clone)]
pub struct DraggedSectionSize(pub PrSection);

impl Render for DraggedSectionSize {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

#[derive(Clone)]
pub struct DraggedPrSection(pub PrSection);

impl Render for DraggedPrSection {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        drag_preview(self.0.label(), cx)
    }
}

/// The checks summary, in Orca's words: "1 failed · 2 pending · 24 passed · 18 skipped".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckCounts {
    pub failed: usize,
    pub pending: usize,
    pub passed: usize,
    pub skipped: usize,
    pub neutral: usize,
}

impl CheckCounts {
    pub fn of(checks: &[Check]) -> Self {
        let mut counts = Self::default();
        for check in checks {
            let outcome = check_outcome(check);
            match outcome.bucket() {
                CheckBucket::Failing => counts.failed += 1,
                CheckBucket::Running => counts.pending += 1,
                CheckBucket::Passing if outcome == CheckOutcome::Skipped => counts.skipped += 1,
                CheckBucket::Passing => counts.passed += 1,
                CheckBucket::Neutral => counts.neutral += 1,
            }
        }
        counts
    }

    /// Nothing failing or still running.
    pub fn all_green(&self) -> bool {
        self.failed == 0 && self.pending == 0
    }

    /// The parts folded behind the toggle: passed, skipped and neutral.
    pub fn settled(&self) -> usize {
        self.passed + self.skipped + self.neutral
    }

    pub fn parts(&self) -> Vec<(String, Color)> {
        [
            (self.failed, "failed", Color::Error),
            (self.pending, "pending", Color::Warning),
            (self.passed, "passed", Color::Muted),
            (self.skipped, "skipped", Color::Muted),
            (self.neutral, "neutral", Color::Muted),
        ]
        .into_iter()
        .filter(|(count, _, _)| *count > 0)
        .map(|(count, word, color)| (format!("{count} {word}"), color))
        .collect()
    }

    pub fn settled_label(&self) -> String {
        [
            (self.passed, "passed"),
            (self.skipped, "skipped"),
            (self.neutral, "neutral"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, word)| format!("{count} {word}"))
        .collect::<Vec<_>>()
        .join(" · ")
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PrState {
    #[default]
    Open,
    Closed,
    Merged,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrLifecycle {
    #[default]
    Open,
    Draft,
    Closed,
    Merged,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Mergeable {
    Mergeable,
    Conflicting,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
    fn is_ready(self) -> bool {
        matches!(self, Self::Clean | Self::HasHooks | Self::Unstable)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Review {
    pub login: String,
    pub state: ReviewState,
    #[serde(default)]
    pub author_avatar_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RequestedReviewer {
    User {
        login: String,
        #[serde(default)]
        avatar_url: Option<String>,
    },
    Team {
        slug: String,
        #[serde(default)]
        avatar_url: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Success,
    Failure,
    Pending,
    Neutral,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Check {
    pub name: String,
    pub kind: CheckKind,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MergeQueueEntry {
    pub state: MergeQueueEntryState,
    #[serde(default)]
    pub position: Option<u32>,
    #[serde(default)]
    pub eta_seconds: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RepoRef {
    pub owner: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct StackEntry {
    pub position: u32,
    pub number: u64,
    pub state: PrLifecycle,
    pub mergeable: Option<Mergeable>,
    pub merge_state_status: Option<MergeStateStatus>,
    pub review_decision: Option<ReviewDecision>,
    pub in_merge_queue: bool,
    pub title: Option<String>,
    pub head_ref_name: Option<String>,
    pub url: Option<String>,
    pub checks_status: Option<CheckState>,
    pub author: Option<String>,
    pub author_avatar_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct StackView {
    pub number: u64,
    pub position: u32,
    pub size: u32,
    #[serde(alias = "base_ref_name")]
    pub base_ref: String,
    pub out_of_date: bool,
    pub entries: Option<Vec<StackEntry>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PullRequest {
    pub repo: RepoRef,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: Option<String>,
    pub author_avatar_url: Option<String>,
    pub state: PrState,
    pub is_draft: bool,
    pub head_ref_name: String,
    pub base_ref_name: String,
    pub mergeable: Option<Mergeable>,
    pub merge_state_status: Option<MergeStateStatus>,
    pub review_decision: Option<ReviewDecision>,
    pub requested_reviewers: Vec<RequestedReviewer>,
    pub latest_reviews: Vec<Review>,
    pub latest_opinionated_reviews: Option<Vec<Review>>,
    pub checks: Vec<Check>,
    pub checks_rollup: Option<CheckState>,
    pub is_in_merge_queue: bool,
    pub merge_queue_entry: Option<MergeQueueEntry>,
    pub updated_at: Option<String>,
    pub stack: Option<StackView>,
}

impl PullRequest {
    pub fn lifecycle(&self) -> PrLifecycle {
        match self.state {
            PrState::Merged => PrLifecycle::Merged,
            PrState::Closed => PrLifecycle::Closed,
            PrState::Open | PrState::Unknown if self.is_draft => PrLifecycle::Draft,
            PrState::Open | PrState::Unknown => PrLifecycle::Open,
        }
    }

    fn has_conflicts(&self) -> bool {
        self.mergeable == Some(Mergeable::Conflicting)
            || self.merge_state_status == Some(MergeStateStatus::Dirty)
    }

    fn pull_url(&self, number: u64) -> String {
        format!(
            "https://github.com/{}/{}/pull/{number}",
            self.repo.owner, self.repo.name
        )
    }
}

/// The reply of the server's `pr.status` request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PrStatus {
    pub pull_request: Option<PullRequest>,
    pub known: bool,
    pub stack: Option<StackView>,
    pub error: Option<String>,
    pub previews: Vec<PreviewDeployment>,
    pub primary_preview: Option<PreviewDeployment>,
    /// While GitHub's quota is spent: when the server asks again (epoch seconds); the rest is cached.
    pub rate_limited_until: Option<i64>,
}

/// "GitHub rate limit, retrying at 14:05", in local time.
fn rate_limit_note(until: i64) -> String {
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    match OffsetDateTime::from_unix_timestamp(until) {
        Ok(at) => {
            let at = at.to_offset(offset);
            format!(
                "GitHub rate limit, retrying at {:02}:{:02}",
                at.hour(),
                at.minute()
            )
        }
        Err(_) => "GitHub rate limit, retrying later".to_owned(),
    }
}

impl PrStatus {
    fn stack(&self) -> Option<&StackView> {
        self.stack.as_ref().or_else(|| {
            self.pull_request
                .as_ref()
                .and_then(|pull_request| pull_request.stack.as_ref())
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct WorkspaceEntry {
    pub id: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub pull_request: Option<serde_json::Value>,
}

impl WorkspaceEntry {
    fn pull_request_number(&self) -> Option<u64> {
        self.pull_request.as_ref()?["number"].as_u64()
    }

    fn branch_name(&self) -> Option<&str> {
        self.branch
            .as_deref()
            .map(|branch| branch.strip_prefix("refs/heads/").unwrap_or(branch))
            .filter(|branch| !branch.is_empty())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Ready,
    Queued,
    Pending,
    Blocked,
    Merged,
    Neutral,
}

/// Orca's stack badge: the first matching rule wins, and readiness follows GitHub's merge state.
pub fn stack_entry_badge(entry: &StackEntry) -> (&'static str, Tone) {
    let merge_state = entry.merge_state_status;
    match entry.state {
        PrLifecycle::Merged => return ("Merged", Tone::Merged),
        PrLifecycle::Closed => return ("Closed", Tone::Neutral),
        PrLifecycle::Draft => return ("Draft", Tone::Neutral),
        PrLifecycle::Open | PrLifecycle::Unknown => {}
    }
    if entry.in_merge_queue {
        ("Queued", Tone::Queued)
    } else if merge_state.is_some_and(MergeStateStatus::is_ready) {
        ("Ready", Tone::Ready)
    } else if entry.mergeable == Some(Mergeable::Conflicting)
        || merge_state == Some(MergeStateStatus::Dirty)
    {
        ("Conflicts", Tone::Blocked)
    } else if entry.checks_status == Some(CheckState::Failure) {
        ("Checks failed", Tone::Blocked)
    } else if entry.review_decision == Some(ReviewDecision::ChangesRequested) {
        ("Changes requested", Tone::Blocked)
    } else if entry.review_decision == Some(ReviewDecision::ReviewRequired) {
        ("Review needed", Tone::Pending)
    } else if entry.checks_status == Some(CheckState::Pending) {
        ("Checks pending", Tone::Pending)
    } else if merge_state == Some(MergeStateStatus::Behind) {
        ("Behind", Tone::Pending)
    } else if merge_state == Some(MergeStateStatus::Blocked) {
        ("Not ready", Tone::Pending)
    } else if entry.review_decision == Some(ReviewDecision::Approved) {
        ("Approved", Tone::Ready)
    } else {
        ("Open", Tone::Neutral)
    }
}

pub fn pull_request_badge(pull_request: &PullRequest) -> (&'static str, Tone) {
    if pull_request.is_in_merge_queue && pull_request.lifecycle() == PrLifecycle::Open {
        return ("Queued", Tone::Queued);
    }
    match pull_request.lifecycle() {
        PrLifecycle::Open | PrLifecycle::Unknown => ("Open", Tone::Ready),
        PrLifecycle::Draft => ("Draft", Tone::Neutral),
        PrLifecycle::Merged => ("Merged", Tone::Merged),
        PrLifecycle::Closed => ("Closed", Tone::Blocked),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    Queued,
    InProgress,
    Waiting,
    Requested,
    Pending,
    Expected,
    Success,
    Failure,
    Error,
    Neutral,
    Skipped,
    Cancelled,
    TimedOut,
    ActionRequired,
    Stale,
    StartupFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CheckBucket {
    Failing,
    Running,
    Neutral,
    Passing,
}

impl CheckOutcome {
    pub fn bucket(self) -> CheckBucket {
        match self {
            Self::Queued
            | Self::InProgress
            | Self::Waiting
            | Self::Requested
            | Self::Pending
            | Self::Expected => CheckBucket::Running,
            Self::Success | Self::Skipped => CheckBucket::Passing,
            Self::Failure
            | Self::Error
            | Self::Cancelled
            | Self::TimedOut
            | Self::ActionRequired
            | Self::StartupFailure => CheckBucket::Failing,
            Self::Neutral | Self::Stale => CheckBucket::Neutral,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::InProgress => "In progress",
            Self::Waiting => "Waiting for approval",
            Self::Requested => "Requested",
            Self::Pending => "Pending",
            Self::Expected => "Expected",
            Self::Success => "Passed",
            Self::Failure => "Failed",
            Self::Error => "Error",
            Self::Neutral => "Neutral",
            Self::Skipped => "Skipped",
            Self::Cancelled => "Cancelled",
            Self::TimedOut => "Timed out",
            Self::ActionRequired => "Action required",
            Self::Stale => "Stale",
            Self::StartupFailure => "Startup failure",
        }
    }

    fn is_spinning(self) -> bool {
        matches!(
            self,
            Self::InProgress | Self::Waiting | Self::Requested | Self::Pending
        )
    }
}

pub fn check_outcome(check: &Check) -> CheckOutcome {
    match check.kind {
        CheckKind::Run { status, conclusion } => match status {
            CheckRunStatus::Queued => CheckOutcome::Queued,
            CheckRunStatus::Waiting => CheckOutcome::Waiting,
            CheckRunStatus::Requested => CheckOutcome::Requested,
            CheckRunStatus::Pending => CheckOutcome::Pending,
            CheckRunStatus::InProgress | CheckRunStatus::Unknown => CheckOutcome::InProgress,
            CheckRunStatus::Completed => match conclusion {
                Some(CheckConclusion::Success) => CheckOutcome::Success,
                Some(CheckConclusion::Failure) => CheckOutcome::Failure,
                Some(CheckConclusion::Neutral) => CheckOutcome::Neutral,
                Some(CheckConclusion::Cancelled) => CheckOutcome::Cancelled,
                Some(CheckConclusion::Skipped) => CheckOutcome::Skipped,
                Some(CheckConclusion::TimedOut) => CheckOutcome::TimedOut,
                Some(CheckConclusion::ActionRequired) => CheckOutcome::ActionRequired,
                Some(CheckConclusion::Stale) => CheckOutcome::Stale,
                Some(CheckConclusion::StartupFailure) => CheckOutcome::StartupFailure,
                Some(CheckConclusion::Unknown) => CheckOutcome::Neutral,
                None => CheckOutcome::InProgress,
            },
        },
        CheckKind::Status { state } => match state {
            StatusContextState::Success => CheckOutcome::Success,
            StatusContextState::Failure => CheckOutcome::Failure,
            StatusContextState::Error => CheckOutcome::Error,
            StatusContextState::Expected => CheckOutcome::Expected,
            StatusContextState::Pending | StatusContextState::Unknown => CheckOutcome::Pending,
        },
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckSummary {
    pub passing: usize,
    pub failing: usize,
    pub running: usize,
}

pub fn summarize_checks(checks: &[Check]) -> CheckSummary {
    let mut summary = CheckSummary::default();
    for check in checks {
        match check_outcome(check).bucket() {
            CheckBucket::Passing => summary.passing += 1,
            CheckBucket::Failing => summary.failing += 1,
            CheckBucket::Running => summary.running += 1,
            CheckBucket::Neutral => {}
        }
    }
    summary
}

/// Failing first, then running, so what needs attention is at the top; ties keep GitHub's order.
pub fn sorted_checks(checks: &[Check]) -> Vec<&Check> {
    let mut sorted: Vec<&Check> = checks.iter().collect();
    sorted.sort_by_key(|check| check_outcome(check).bucket());
    sorted
}

pub fn parse_timestamp(text: &str) -> Option<i64> {
    OffsetDateTime::parse(text, &Rfc3339)
        .ok()
        .map(OffsetDateTime::unix_timestamp)
}

pub fn format_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        match seconds % 60 {
            0 => format!("{}m", seconds / 60),
            remainder => format!("{}m {remainder}s", seconds / 60),
        }
    } else {
        match (seconds % 3600) / 60 {
            0 => format!("{}h", seconds / 3600),
            minutes => format!("{}h {minutes}m", seconds / 3600),
        }
    }
}

pub fn format_relative(seconds_ago: i64) -> String {
    if seconds_ago < 60 {
        "just now".into()
    } else if seconds_ago < 3600 {
        format!("{} min ago", seconds_ago / 60)
    } else if seconds_ago < 86_400 {
        format!("{}h ago", seconds_ago / 3600)
    } else {
        format!("{}d ago", seconds_ago / 86_400)
    }
}

/// How long a check ran, or for a running check how long it has been running.
pub fn check_duration(check: &Check, now: i64) -> Option<String> {
    let started = parse_timestamp(check.started_at.as_deref()?)?;
    let ended = match check.completed_at.as_deref() {
        Some(completed) => parse_timestamp(completed)?,
        None if check_outcome(check).bucket() == CheckBucket::Running => now,
        None => return None,
    };
    Some(format_duration(ended - started))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionItem {
    pub title: String,
    pub reason: String,
}

pub fn attention_items(pull_request: &PullRequest) -> Vec<AttentionItem> {
    let mut items = Vec::new();
    if matches!(
        pull_request.lifecycle(),
        PrLifecycle::Merged | PrLifecycle::Closed
    ) {
        return items;
    }
    if pull_request.has_conflicts() {
        items.push(AttentionItem {
            title: "Conflicts block this PR".into(),
            reason: format!("Merge conflicts with {}", pull_request.base_ref_name),
        });
    }
    let failing: Vec<&str> = pull_request
        .checks
        .iter()
        .filter(|check| check_outcome(check).bucket() == CheckBucket::Failing)
        .map(|check| check.name.as_str())
        .collect();
    if !failing.is_empty() {
        let mut reason = failing
            .iter()
            .take(3)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        if failing.len() > 3 {
            reason.push_str(&format!(" and {} more", failing.len() - 3));
        }
        items.push(AttentionItem {
            title: match failing.len() {
                1 => "1 check failed".into(),
                count => format!("{count} checks failed"),
            },
            reason,
        });
    }
    if pull_request.review_decision == Some(ReviewDecision::ChangesRequested) {
        let reviewers: Vec<String> = latest_reviews(pull_request)
            .into_iter()
            .filter(|review| review.state == ReviewState::ChangesRequested)
            .map(|review| format!("@{}", review.login))
            .collect();
        items.push(AttentionItem {
            title: "Changes requested".into(),
            reason: if reviewers.is_empty() {
                "A reviewer asked for changes".into()
            } else {
                format!("By {}", reviewers.join(", "))
            },
        });
    }
    if pull_request.merge_state_status == Some(MergeStateStatus::Behind) {
        items.push(AttentionItem {
            title: format!("Behind {}", pull_request.base_ref_name),
            reason: "Update the branch before it can merge".into(),
        });
    }
    items
}

/// Each author's latest submitted review, in the order GitHub lists them.
pub fn latest_reviews(pull_request: &PullRequest) -> Vec<&Review> {
    let reviews = pull_request
        .latest_opinionated_reviews
        .as_deref()
        .unwrap_or(&pull_request.latest_reviews);
    let mut latest: Vec<&Review> = Vec::new();
    for review in reviews {
        if review.state == ReviewState::Pending {
            continue;
        }
        latest.retain(|existing| !existing.login.eq_ignore_ascii_case(&review.login));
        latest.push(review);
    }
    latest
}

/// Stack entries from the top of the stack down; the current PR's own entry takes its details from the PR.
pub fn stack_rows(stack: &StackView, pull_request: &PullRequest) -> Vec<StackEntry> {
    let mut entries = stack.entries.clone().unwrap_or_default();
    for entry in &mut entries {
        if entry.number == pull_request.number {
            entry
                .title
                .get_or_insert_with(|| pull_request.title.clone());
            entry
                .head_ref_name
                .get_or_insert_with(|| pull_request.head_ref_name.clone());
            if entry.checks_status.is_none() {
                entry.checks_status = pull_request.checks_rollup;
            }
            if entry.author.is_none() {
                entry.author = pull_request.author.clone();
            }
            if entry.author_avatar_url.is_none() {
                entry.author_avatar_url = pull_request.author_avatar_url.clone();
            }
        }
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.position));
    entries
}

pub(crate) fn tone_color(tone: Tone, cx: &App) -> Option<Hsla> {
    let (light, dark) = match tone {
        Tone::Ready => (0x059669, 0x34d399),
        Tone::Queued => (0xea580c, 0xfb923c),
        Tone::Pending => (0xb45309, 0xfbbf24),
        Tone::Blocked => (0xe11d48, 0xfb7185),
        Tone::Merged => (0x7c3aed, 0xa78bfa),
        Tone::Neutral => return None,
    };
    let color = if cx.theme().appearance().is_light() {
        light
    } else {
        dark
    };
    Some(rgb(color).into())
}

pub(crate) fn render_outdated_pill(cx: &App) -> AnyElement {
    render_pill("Outdated", Tone::Pending, cx).into_any_element()
}

fn render_pill(label: impl Into<SharedString>, tone: Tone, cx: &App) -> impl IntoElement {
    let colors = cx.theme().colors();
    let (text, background, border) = match tone_color(tone, cx) {
        Some(color) => (
            Color::Custom(color),
            color.opacity(0.15),
            color.opacity(0.3),
        ),
        None => (Color::Muted, colors.element_background, colors.border),
    };
    div()
        .flex_none()
        .px_1p5()
        .rounded_full()
        .border_1()
        .border_color(border)
        .bg(background)
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .color(text)
                .single_line(),
        )
}

pub fn preview_state_badge(state: &str) -> (&'static str, Tone) {
    match state {
        "success" => ("Ready", Tone::Ready),
        "in_progress" => ("Building", Tone::Pending),
        "queued" => ("Queued", Tone::Pending),
        "pending" => ("Pending", Tone::Pending),
        "failure" => ("Failed", Tone::Blocked),
        "error" => ("Error", Tone::Blocked),
        "inactive" => ("Inactive", Tone::Neutral),
        _ => ("Unknown", Tone::Neutral),
    }
}

/// The letter shown in place of an avatar that has no image: `@octocat` → `O`, `acme/payments` → `P`.
pub fn initials(name: &str) -> String {
    name.rsplit('/')
        .next()
        .and_then(|name| name.chars().find(|character| character.is_alphanumeric()))
        .map(|character| character.to_uppercase().collect())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvatarKind {
    Person,
    Team,
    Bot,
}

/// A GitHub avatar, or a letter or icon in its place when there is no image URL.
pub fn render_avatar(
    url: Option<&str>,
    name: &str,
    kind: AvatarKind,
    size: Pixels,
    cx: &App,
) -> AnyElement {
    if let Some(url) = url.filter(|url| !url.is_empty()) {
        return Avatar::new(url.to_string()).size(size).into_any_element();
    }
    let initials = initials(name);
    let content = if kind == AvatarKind::Team {
        Icon::new(IconName::UserGroup)
            .size(IconSize::XSmall)
            .color(Color::Muted)
            .into_any_element()
    } else if kind == AvatarKind::Bot {
        Icon::new(IconName::Bot)
            .size(IconSize::XSmall)
            .color(Color::Muted)
            .into_any_element()
    } else if initials.is_empty() {
        Icon::new(IconName::Person)
            .size(IconSize::XSmall)
            .color(Color::Muted)
            .into_any_element()
    } else {
        Label::new(initials)
            .size(LabelSize::XSmall)
            .color(Color::Muted)
            .weight(gpui::FontWeight::SEMIBOLD)
            .into_any_element()
    };
    h_flex()
        .flex_none()
        .size(size)
        .rounded_full()
        .justify_center()
        .bg(cx.theme().colors().element_selected)
        .border_1()
        .border_color(cx.theme().colors().border)
        .child(content)
        .into_any_element()
}

fn check_icon(outcome: CheckOutcome, animate: bool) -> AnyElement {
    let icon = match outcome {
        CheckOutcome::Success => Icon::new(IconName::Check).color(Color::Success),
        CheckOutcome::Failure | CheckOutcome::Error | CheckOutcome::StartupFailure => {
            Icon::new(IconName::XCircle).color(Color::Error)
        }
        CheckOutcome::TimedOut => Icon::new(IconName::Clock).color(Color::Error),
        CheckOutcome::ActionRequired => Icon::new(IconName::Warning).color(Color::Warning),
        CheckOutcome::Cancelled => {
            Icon::from_path(status_style::icon_path("", Some("ban"))).color(Color::Muted)
        }
        CheckOutcome::Skipped => Icon::new(IconName::Dash).color(Color::Muted),
        CheckOutcome::Neutral | CheckOutcome::Stale => {
            Icon::from_path(status_style::icon_path("", Some("circle-dashed"))).color(Color::Muted)
        }
        CheckOutcome::Expected => {
            Icon::from_path(status_style::icon_path("", Some("circle-dashed")))
                .color(Color::Warning)
        }
        CheckOutcome::Queued => {
            Icon::from_path(status_style::icon_path("", Some("circle"))).color(Color::Muted)
        }
        CheckOutcome::InProgress
        | CheckOutcome::Waiting
        | CheckOutcome::Requested
        | CheckOutcome::Pending => Icon::new(IconName::LoadCircle).color(Color::Warning),
    }
    .size(IconSize::Small);
    if animate && outcome.is_spinning() {
        icon.with_rotate_animation(2).into_any_element()
    } else {
        icon.into_any_element()
    }
}

fn unix_now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Snapshot {
    workspaces: Vec<WorkspaceEntry>,
    current: Option<WorkspaceEntry>,
    status: Option<PrStatus>,
    /// The PR's review threads, from the same server cache the diff gutters read.
    threads: Vec<ReviewThread>,
    /// Issue comments and review summaries, read with the threads.
    conversation: Vec<pr_comments::ConversationComment>,
    viewer: Option<Viewer>,
    /// The workspace and PR `updated_at` the threads were read for; None when this snapshot skipped them.
    threads_for: Option<(String, Option<String>)>,
}

/// When a snapshot reads the review threads. Like Orca they aren't polled: a comment or review
/// changes the PR's `updated_at`, so they're read when that moves, on a manual refresh, or rarely.
#[derive(Debug, Clone, PartialEq)]
enum ThreadsFetch {
    Refresh,
    /// Read them if the PR isn't the one `known` describes, or if `due`.
    IfChanged {
        known: Option<(String, Option<String>)>,
        due: bool,
    },
}

type FetchedThreads = (
    Vec<ReviewThread>,
    Vec<pr_comments::ConversationComment>,
    Option<Viewer>,
);

/// `None` when the read failed: the shared store keeps what it has rather than going empty.
async fn fetch_threads(client: &Client, workspace: &str, refresh: bool) -> Option<FetchedThreads> {
    let reply = client
        .request(
            "review.threads",
            serde_json::json!({ "workspace": workspace, "refresh": refresh, "conversation": true }),
        )
        .await;
    match reply {
        Ok(reply) => Some((
            serde_json::from_value(reply["threads"].clone()).unwrap_or_default(),
            serde_json::from_value(reply["conversation"].clone()).unwrap_or_default(),
            serde_json::from_value(reply["viewer"].clone()).ok(),
        )),
        Err(error) => {
            log::warn!("loading PR threads failed: {error:#}");
            None
        }
    }
}

async fn fetch_snapshot(
    client: Arc<Client>,
    paths: Vec<PathBuf>,
    threads_fetch: ThreadsFetch,
) -> Result<Snapshot> {
    let workspaces: Vec<WorkspaceEntry> = serde_json::from_value(
        client
            .request("workspace.list", serde_json::json!({}))
            .await?,
    )?;
    let current = workspaces
        .iter()
        .find(|workspace| paths.contains(&workspace.path))
        .cloned();
    let status: Option<PrStatus> = match &current {
        Some(current) => Some(serde_json::from_value(
            client
                .request("pr.status", serde_json::json!({ "workspace": current.id }))
                .await?,
        )?),
        None => None,
    };
    let pull_request = status
        .as_ref()
        .and_then(|status| status.pull_request.as_ref());
    let ((threads, conversation, viewer), threads_for) = match (&current, pull_request) {
        (Some(current), Some(pull_request)) => {
            let key = (current.id.clone(), pull_request.updated_at.clone());
            let wanted = match &threads_fetch {
                ThreadsFetch::Refresh => true,
                ThreadsFetch::IfChanged { known, due } => *due || known.as_ref() != Some(&key),
            };
            let refresh = threads_fetch == ThreadsFetch::Refresh;
            let fetched = if wanted {
                fetch_threads(&client, &current.id, refresh).await
            } else {
                None
            };
            match fetched {
                Some(fetched) => (fetched, Some(key)),
                None => (Default::default(), None),
            }
        }
        (Some(current), None) => (Default::default(), Some((current.id.clone(), None))),
        (None, _) => (Default::default(), None),
    };
    Ok(Snapshot {
        workspaces,
        current,
        status,
        threads,
        conversation,
        viewer,
        threads_for,
    })
}

/// A fixture for previews and visual tests: `now` (RFC 3339), `workspaces` as `workspace.list`
/// returns them, `current` (a workspace id or null) and `pr_status` as `pr.status` returns it.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PreviewFixture {
    now: Option<String>,
    workspaces: Vec<WorkspaceEntry>,
    current: Option<String>,
    pr_status: Option<PrStatus>,
    /// `review.get`'s answer, to show the Review section.
    review: Option<ReviewView>,
    /// Uncommitted files, which disable re-request.
    uncommitted: usize,
    /// `review.threads`' threads.
    threads: Vec<ReviewThread>,
    /// `review.threads`' conversation: issue comments and review summaries.
    conversation: Vec<pr_comments::ConversationComment>,
    /// The `sourceControl.sections.pr` layout.
    layout: Option<serde_json::Value>,
    /// Sections drawn collapsed or expanded regardless of their default, by id.
    collapsed: HashMap<String, bool>,
    /// Show the passed and skipped checks.
    checks_expanded: bool,
    /// The Comments tab: `all`, `humans`, `bots` or `unresolved`.
    audience: Option<String>,
    /// `pr.body`'s description.
    description: Option<String>,
    /// `false` unfolds the description, which starts folded.
    description_collapsed: Option<bool>,
}

pub struct GitHubPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    position: DockPosition,
    client: Option<Arc<Client>>,
    offline: Option<String>,
    snapshot: Option<Snapshot>,
    refreshing: bool,
    stack_expanded: bool,
    fixed_now: Option<i64>,
    animate: bool,
    review: Option<Entity<ReviewSection>>,
    /// The PR update time the review section last loaded at; a change reloads it.
    review_loaded_at: Option<String>,
    preview_review: Option<(ReviewView, usize)>,
    /// The PR sections' order and hidden set (`sourceControl.sections.pr`).
    layout: Layout,
    /// Collapse toggles per workspace and section; untouched sections use their default.
    collapsed: HashMap<(String, PrSection), bool>,
    /// Workspaces whose passed and skipped checks are unfolded.
    checks_expanded: HashSet<String>,
    /// The thread a reply is being written in, and its editor.
    reply: Option<(String, Entity<Editor>)>,
    /// The conversation comment or review summary being rewritten, by node id.
    editing: Option<(String, Entity<Editor>)>,
    /// Threads with a resolve, unresolve or reply on its way.
    busy_threads: HashSet<String>,
    action_error: Option<String>,
    /// The Comments section's audience tab per workspace.
    audiences: HashMap<String, pr_comments::Audience>,
    /// Rendered comment bodies by comment id, with the text they were made from.
    markdown: HashMap<String, (String, Entity<markdown::Markdown>)>,
    markdown_style: Option<markdown::MarkdownStyle>,
    /// Section body heights the user dragged to, per workspace; absent means natural height.
    heights: HashMap<(String, PrSection), Pixels>,
    /// The PR description: the workspace and PR update time it was read at, and the text.
    description: Option<(String, Option<String>, String)>,
    description_task: Option<Task<()>>,
    /// Workspaces whose description is unfolded; it starts folded, as in Orca.
    description_expanded: HashSet<String>,
    /// Showing in its dock; threads are only read while it is.
    active: bool,
    threads_read_at: Option<Instant>,
    /// The review state the diff tabs share; the snapshot's threads and conversation mirror it.
    store: Entity<PrReviewStore>,
    _store_subscriptions: [Subscription; 2],
    _poll: Task<()>,
}

fn markdown_style(panel: &GitHubPanel) -> markdown::MarkdownStyle {
    panel.markdown_style.clone().unwrap_or_default()
}

/// Small body text with headings barely above it, like Orca's 12 px PR panel.
pub(crate) fn compact_markdown_style(
    mut style: markdown::MarkdownStyle,
    cx: &App,
) -> markdown::MarkdownStyle {
    let heading = |size: f32| {
        Some(gpui::TextStyleRefinement {
            font_size: Some(gpui::rems(size).into()),
            font_weight: Some(gpui::FontWeight::SEMIBOLD),
            ..Default::default()
        })
    };
    style.base_text_style.font_size = ui::TextSize::Small.rems(cx).into();
    style.base_text_style.line_height = gpui::relative(1.45);
    style.heading_level_styles = Some(markdown::HeadingLevelStyles {
        h1: heading(0.875),
        h2: heading(0.8125),
        h3: heading(0.75),
        h4: heading(0.75),
        h5: heading(0.75),
        h6: heading(0.75),
    });
    style
}

impl GitHubPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            cx.update(|window, cx| {
                let handle = window.window_handle();
                cx.new(|cx| Self::new(workspace, Some(handle), cx))
            })
        })
    }

    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: Option<gpui::AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self::empty(workspace, cx);
        panel.animate = true;
        if !crate::enabled(cx) {
            return panel;
        }
        panel._poll = cx.spawn(async move |this, cx| {
            let mut last_poll: Option<Instant> = None;
            loop {
                let Ok(workspace) = this.read_with(cx, |panel, _| panel.workspace.clone()) else {
                    return;
                };
                let in_front = window.is_none_or(|window| {
                    cx.update_window(window, |_, window, _| window.is_window_active())
                        .unwrap_or(false)
                });
                if !in_front
                    && last_poll.is_some_and(|at| at.elapsed() < GITHUB_BACKGROUND_INTERVAL)
                {
                    cx.background_executor()
                        .timer(GITHUB_REFRESH_INTERVAL)
                        .await;
                    continue;
                }
                last_poll = Some(Instant::now());
                let client = cx.update(|cx| crate::routing::client_for_workspace(&workspace, cx));
                let Some(client) = client else {
                    let offline = anyhow::anyhow!("not connected to the Icarus server");
                    if this
                        .update(cx, |panel, cx| panel.set_offline(offline, cx))
                        .is_err()
                    {
                        return;
                    }
                    cx.background_executor()
                        .timer(GITHUB_REFRESH_INTERVAL)
                        .await;
                    continue;
                };
                let Ok((paths, threads_fetch)) = this.update(cx, |panel, cx| {
                    panel.load_layout(cx);
                    (panel.project_paths(cx), panel.threads_fetch(false))
                }) else {
                    return;
                };
                let snapshot =
                    Tokio::spawn_result(cx, fetch_snapshot(client.clone(), paths, threads_fetch))
                        .await;
                let updated = this.update(cx, |panel, cx| match snapshot {
                    Ok(snapshot) => {
                        panel.client = Some(client);
                        panel.apply_snapshot(snapshot, cx);
                    }
                    Err(error) => panel.set_offline(error, cx),
                });
                if updated.is_err() {
                    return;
                }
                cx.background_executor()
                    .timer(GITHUB_REFRESH_INTERVAL)
                    .await;
            }
        });
        panel
    }

    fn empty(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let store = PrReviewStore::global(cx);
        let store_subscriptions = [
            cx.observe(&store, |panel, _, cx| panel.sync_from_store(cx)),
            cx.subscribe(&store, |panel, _, failed: &PrReviewFailed, cx| {
                if panel.store_key().as_deref() == Some(failed.workspace.as_str()) {
                    panel.action_error = Some(failed.message.clone());
                    cx.notify();
                }
            }),
        ];
        Self {
            store,
            _store_subscriptions: store_subscriptions,
            focus_handle: cx.focus_handle(),
            workspace,
            position: DockPosition::Right,
            client: None,
            offline: None,
            snapshot: None,
            refreshing: false,
            stack_expanded: false,
            fixed_now: None,
            animate: false,
            review: None,
            review_loaded_at: None,
            preview_review: None,
            layout: Layout::default(),
            collapsed: HashMap::default(),
            checks_expanded: HashSet::default(),
            reply: None,
            editing: None,
            busy_threads: HashSet::default(),
            action_error: None,
            audiences: HashMap::default(),
            markdown: HashMap::default(),
            markdown_style: None,
            heights: HashMap::default(),
            description: None,
            description_task: None,
            description_expanded: HashSet::default(),
            active: false,
            threads_read_at: None,
            _poll: Task::ready(()),
        }
    }

    /// Keeps one Markdown view per shown comment, rebuilt when its body changes.
    fn sync_markdown(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.markdown_style = Some(pr_comments::comment_markdown_style(window, cx));
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let description = self
            .current_description()
            .map(|body| (DESCRIPTION_KEY.to_owned(), body.to_owned()));
        let mut seen = HashSet::default();
        let mut missing = Vec::new();
        let bodies = snapshot
            .threads
            .iter()
            .flat_map(|thread| thread.comments.iter())
            .map(|comment| (comment.id.clone(), comment.body.clone()))
            .chain(
                snapshot
                    .conversation
                    .iter()
                    .map(|comment| (comment.id.clone(), comment.body.clone())),
            )
            .chain(description);
        for (id, body) in bodies {
            let current = self
                .markdown
                .get(&id)
                .is_some_and(|(known, _)| *known == body);
            if !current {
                missing.push((id.clone(), body));
            }
            seen.insert(id);
        }
        // A pending comment's view carries over to its confirmed id, so its text never blanks.
        let by_body: HashMap<String, Entity<markdown::Markdown>> = self
            .markdown
            .values()
            .map(|(body, markdown)| (body.clone(), markdown.clone()))
            .collect();
        self.markdown.retain(|id, _| seen.contains(id));
        for (id, body) in missing {
            let markdown = match by_body.get(&body) {
                Some(markdown) => markdown.clone(),
                None => {
                    let (text, _) = crate::media::split_media(&body);
                    cx.new(|cx| crate::review::comment_markdown(text, cx))
                }
            };
            self.markdown.insert(id, (body, markdown));
        }
    }

    pub fn react(
        &mut self,
        subject: String,
        reaction: String,
        reacted: bool,
        cx: &mut Context<Self>,
    ) {
        let op = ReviewOp::React {
            subject,
            reaction,
            react: !reacted,
        };
        self.change(op, cx);
    }

    /// Adds a conversation comment or review summary to the workspace composer's attachments,
    /// built from what the panel shows, so it lands at once.
    pub fn send_conversation_to_agent(&mut self, id: String, cx: &mut Context<Self>) {
        if let Some(key) = self.store_key()
            && crate::pr_store::send_conversation_to_agent(&key, &id, cx).is_none()
        {
            log::warn!("no comment {id} to send to the agent");
        }
    }

    /// Opens the editor over a conversation comment or review summary the viewer may rewrite.
    pub fn start_edit(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some((body, _)) = self.published(&id) else {
            return;
        };
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 12, window, cx);
            editor.set_text(body, window, cx);
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.editing = Some((id, editor));
        cx.notify();
    }

    pub fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.editing = None;
        cx.notify();
    }

    pub fn save_edit(&mut self, cx: &mut Context<Self>) {
        let Some((id, editor)) = self.editing.take() else {
            return;
        };
        let body = editor.read(cx).text(cx).trim().to_owned();
        let Some((known, published)) = self.published(&id) else {
            return;
        };
        if body.is_empty() || body == known {
            cx.notify();
            return;
        }
        self.change(
            ReviewOp::EditComment {
                id,
                body,
                published,
            },
            cx,
        );
    }

    pub fn delete_comment(&mut self, id: String, cx: &mut Context<Self>) {
        let Some((_, published)) = self.published(&id) else {
            return;
        };
        self.change(ReviewOp::DeleteComment { id, published }, cx);
    }

    /// A published comment's body and the reference the edit and delete RPCs address it by.
    fn published(&self, id: &str) -> Option<(String, serde_json::Value)> {
        let snapshot = self.snapshot.as_ref()?;
        if let Some(comment) = snapshot.conversation.iter().find(|c| c.id == id) {
            return Some((comment.body.clone(), comment.published_ref()));
        }
        snapshot
            .threads
            .iter()
            .flat_map(|thread| &thread.comments)
            .find(|comment| comment.id == id)
            .map(|comment| {
                let published = serde_json::json!({ "kind": "review_comment", "node_id": id });
                (comment.body.clone(), published)
            })
    }

    /// Adds the thread to the workspace composer's attachments, as the diff thread's footer does.
    pub fn send_thread_to_agent(&mut self, thread: String, cx: &mut Context<Self>) {
        if let Some(key) = self.store_key()
            && crate::pr_store::send_thread_to_agent(&key, &thread, cx).is_none()
        {
            log::warn!("no thread {thread} to send to the agent");
        }
    }

    /// Reads the PR section layout from the user's settings, where Orca's import puts it.
    fn load_layout(&mut self, cx: &mut Context<Self>) {
        let Some(task) = crate::review::request(
            crate::app_client(cx),
            "config.get",
            serde_json::json!({ "key": "sourceControl" }),
            cx,
        ) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let Some(value) = task.await.log_err() else {
                return;
            };
            this.update(cx, |panel, cx| {
                let layout = Layout::from_settings(&value["sections"]["pr"]);
                if panel.layout != layout {
                    panel.layout = layout;
                    cx.notify();
                }
            })
            .log_err();
        })
        .detach();
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Shows a new section layout at once and saves it to `sourceControl.sections.pr`.
    pub fn set_layout(&mut self, layout: Layout, cx: &mut Context<Self>) {
        if self.layout == layout {
            return;
        }
        self.layout = layout.clone();
        cx.notify();
        if let Some(task) = crate::review::request(
            crate::app_client(cx),
            "config.set",
            serde_json::json!({ "key": "sourceControl.sections.pr", "value": layout.to_value() }),
            cx,
        ) {
            task.detach_and_log_err(cx);
        }
    }

    pub fn toggle_section_hidden(&mut self, section: PrSection, cx: &mut Context<Self>) {
        let layout = self.layout.toggled_hidden(section);
        self.set_layout(layout, cx);
    }

    pub fn move_section_by_one(&mut self, section: PrSection, up: bool, cx: &mut Context<Self>) {
        if let Some(layout) = self.layout.moved_by_one(section, up) {
            self.set_layout(layout, cx);
        }
    }

    pub fn drop_section(&mut self, dragged: PrSection, target: PrSection, cx: &mut Context<Self>) {
        let layout = self.layout.dropped(dragged, target);
        self.set_layout(layout, cx);
    }

    fn current_workspace_id(&self) -> Option<&str> {
        self.snapshot
            .as_ref()?
            .current
            .as_ref()
            .map(|current| current.id.as_str())
    }

    fn current_pull_request(&self) -> Option<&PullRequest> {
        self.snapshot
            .as_ref()?
            .status
            .as_ref()?
            .pull_request
            .as_ref()
    }

    /// Resolved starts collapsed, and Checks does while everything is green.
    fn default_collapsed(&self, section: PrSection) -> bool {
        match section {
            PrSection::Resolved => true,
            PrSection::Checks => self.current_pull_request().is_some_and(|pull_request| {
                !pull_request.checks.is_empty() && CheckCounts::of(&pull_request.checks).all_green()
            }),
            _ => false,
        }
    }

    pub fn is_collapsed(&self, section: PrSection) -> bool {
        let workspace = self.current_workspace_id().unwrap_or_default().to_owned();
        self.collapsed
            .get(&(workspace, section))
            .copied()
            .unwrap_or_else(|| self.default_collapsed(section))
    }

    pub fn toggle_collapsed(&mut self, section: PrSection, cx: &mut Context<Self>) {
        let collapsed = !self.is_collapsed(section);
        let workspace = self.current_workspace_id().unwrap_or_default().to_owned();
        self.collapsed.insert((workspace, section), collapsed);
        cx.notify();
    }

    fn checks_expanded(&self) -> bool {
        self.current_workspace_id()
            .is_some_and(|workspace| self.checks_expanded.contains(workspace))
    }

    pub fn toggle_checks_expanded(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.current_workspace_id().map(str::to_owned) else {
            return;
        };
        if !self.checks_expanded.remove(&workspace) {
            self.checks_expanded.insert(workspace);
        }
        cx.notify();
    }

    pub fn review_section(&self) -> Option<&Entity<ReviewSection>> {
        self.review.as_ref()
    }

    pub fn pull_request(&self) -> Option<&PullRequest> {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.status.as_ref())
            .and_then(|status| status.pull_request.as_ref())
    }

    /// The Checks tab's dot, as Orca's: the rollup of the open PR's checks.
    pub fn checks_state(&self) -> Option<CheckState> {
        let pull_request = self.pull_request()?;
        if pull_request.checks.is_empty() {
            return None;
        }
        pull_request
            .checks_rollup
            .filter(|state| !matches!(state, CheckState::Neutral | CheckState::Unknown))
    }

    /// Keeps the Review section on the current workspace's PR, reloading it when the PR changes.
    /// The Source Control panel shows the section, so it calls this as it renders.
    pub fn sync_review_section(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !crate::enabled(cx) && self.preview_review.is_none() {
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let (Some(current), Some(pull_request)) = (
            snapshot.current.clone(),
            snapshot
                .status
                .as_ref()
                .and_then(|status| status.pull_request.clone()),
        ) else {
            self.review = None;
            return;
        };
        let stale = self
            .review
            .as_ref()
            .is_none_or(|section| section.read(cx).workspace() != current.id);
        if stale {
            let client = self.client.clone();
            let preview = self.preview_review.clone();
            self.review = Some(cx.new(|cx| match preview {
                Some((view, uncommitted)) => {
                    ReviewSection::preview(view, &pull_request, uncommitted, window, cx)
                }
                None => ReviewSection::new(current.id.clone(), client, window, cx),
            }));
            self.review_loaded_at = pull_request.updated_at.clone();
        }
        if let Some(section) = &self.review {
            let reload = !stale && self.review_loaded_at != pull_request.updated_at;
            self.review_loaded_at = pull_request.updated_at.clone();
            section.update(cx, |section, cx| {
                section.set_pull_request(&pull_request);
                if reload {
                    section.reload(window, cx);
                }
            });
        }
    }

    /// A panel that never contacts a server and shows the fixture described on `PreviewFixture`.
    pub fn preview(
        workspace: WeakEntity<Workspace>,
        fixture: serde_json::Value,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self::empty(workspace, cx);
        match serde_json::from_value::<PreviewFixture>(fixture) {
            Ok(fixture) => {
                panel.fixed_now = fixture.now.as_deref().and_then(parse_timestamp);
                let current = fixture.current.as_deref().and_then(|id| {
                    fixture
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.id == id)
                        .cloned()
                });
                let workspace = current
                    .as_ref()
                    .map(|current| current.id.clone())
                    .unwrap_or_default();
                for (id, collapsed) in fixture.collapsed {
                    if let Some(section) = PrSection::parse(&id) {
                        panel
                            .collapsed
                            .insert((workspace.clone(), section), collapsed);
                    }
                }
                if let Some(audience) = pr_comments::Audience::ALL.into_iter().find(|audience| {
                    fixture
                        .audience
                        .as_deref()
                        .is_some_and(|name| audience.label().eq_ignore_ascii_case(name))
                }) {
                    panel.audiences.insert(workspace.clone(), audience);
                }
                if fixture.checks_expanded {
                    panel.checks_expanded.insert(workspace.clone());
                }
                if fixture.description_collapsed == Some(false) {
                    panel.description_expanded.insert(workspace.clone());
                }
                if let Some(body) = fixture.description {
                    let updated_at = fixture
                        .pr_status
                        .as_ref()
                        .and_then(|status| status.pull_request.as_ref())
                        .and_then(|pull_request| pull_request.updated_at.clone());
                    panel.description = Some((workspace, updated_at, body));
                }
                if let Some(layout) = &fixture.layout {
                    panel.layout = Layout::from_settings(layout);
                }
                let shared = (fixture.threads.clone(), fixture.conversation.clone());
                panel.snapshot = Some(Snapshot {
                    workspaces: fixture.workspaces,
                    current,
                    status: fixture.pr_status,
                    threads: fixture.threads,
                    conversation: fixture.conversation,
                    viewer: None,
                    threads_for: None,
                });
                if let Some(key) = panel.store_key() {
                    let (threads, conversation) = shared;
                    panel.store.update(cx, |store, cx| {
                        store.set_loaded(&key, Some(threads), Some(conversation), None, None, cx)
                    });
                }
                panel.preview_review = fixture.review.map(|review| (review, fixture.uncommitted));
            }
            Err(error) => panel.offline = Some(format!("invalid preview fixture: {error}")),
        }
        panel
    }

    pub fn set_stack_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        self.stack_expanded = expanded;
        cx.notify();
    }

    fn now(&self) -> i64 {
        self.fixed_now.unwrap_or_else(unix_now)
    }

    fn project_paths(&self, cx: &App) -> Vec<PathBuf> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Vec::new();
        };
        workspace
            .read(cx)
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect()
    }

    fn set_offline(&mut self, error: anyhow::Error, cx: &mut Context<Self>) {
        self.client = None;
        self.refreshing = false;
        self.offline = Some(format!("{error:#}"));
        cx.notify();
    }

    fn apply_snapshot(&mut self, mut snapshot: Snapshot, cx: &mut Context<Self>) {
        self.offline = None;
        self.refreshing = false;
        let read = snapshot.threads_for.is_some();
        if read {
            self.threads_read_at = Some(Instant::now());
        } else if let Some(previous) = self.snapshot.take()
            && previous.current.as_ref().map(|current| &current.id)
                == snapshot.current.as_ref().map(|current| &current.id)
        {
            snapshot.threads = previous.threads;
            snapshot.conversation = previous.conversation;
            snapshot.threads_for = previous.threads_for;
        }
        let fetched = read.then(|| {
            (
                snapshot.threads.clone(),
                snapshot.conversation.clone(),
                snapshot.viewer.clone(),
            )
        });
        self.snapshot = Some(snapshot);
        if let (Some((threads, conversation, viewer)), Some(key)) = (fetched, self.store_key()) {
            // The store answers through `sync_from_store`, laying pending changes over the read.
            self.store.update(cx, |store, cx| {
                store.set_loaded(&key, Some(threads), Some(conversation), None, viewer, cx)
            });
        }
        self.sync_from_store(cx);
        self.load_description(cx);
        cx.notify();
    }

    /// The current workspace's root, which keys its pull request in the shared store.
    fn store_key(&self) -> Option<String> {
        let current = self.snapshot.as_ref()?.current.as_ref()?;
        Some(current.path.to_string_lossy().into_owned())
    }

    /// Mirrors the shared store's threads and conversation for the current workspace.
    fn sync_from_store(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.store_key() else {
            return;
        };
        let Some(data) = self.store.read(cx).data(&key) else {
            return;
        };
        let (threads, conversation) = (data.threads.clone(), data.conversation.clone());
        let Some(snapshot) = self.snapshot.as_mut() else {
            return;
        };
        if snapshot.threads != threads || snapshot.conversation != conversation {
            snapshot.threads = threads;
            snapshot.conversation = conversation;
            cx.notify();
        }
    }

    /// Applies a review change through the shared store: both views show it at once.
    fn change(&mut self, op: ReviewOp, cx: &mut Context<Self>) {
        let Some(key) = self.store_key() else {
            return;
        };
        self.action_error = None;
        let client = self.client.clone();
        self.store.update(cx, |store, cx| {
            if let Some(task) = store.mutate(&key, client, op, cx) {
                task.detach();
            }
        });
    }

    /// Reads the PR description again whenever the PR changes, so polling costs nothing otherwise.
    fn load_description(&mut self, cx: &mut Context<Self>) {
        let (Some(workspace), Some(updated_at)) = (
            self.current_workspace_id().map(str::to_owned),
            self.current_pull_request()
                .map(|pull_request| pull_request.updated_at.clone()),
        ) else {
            return;
        };
        let current = self
            .description
            .as_ref()
            .is_some_and(|(id, at, _)| *id == workspace && *at == updated_at);
        if current || self.description_task.is_some() {
            return;
        }
        let Some(call) = crate::review::request(
            self.client.clone(),
            "pr.body",
            serde_json::json!({ "workspace": workspace }),
            cx,
        ) else {
            return;
        };
        self.description_task = Some(cx.spawn(async move |this, cx| {
            let reply = call.await;
            this.update(cx, |panel, cx| {
                panel.description_task = None;
                match reply {
                    Ok(reply) => {
                        let body = reply["body"].as_str().unwrap_or_default().to_owned();
                        panel.description = Some((workspace, updated_at, body));
                        cx.notify();
                    }
                    Err(error) => log::warn!("loading the PR description failed: {error:#}"),
                }
            })
            .log_err();
        }));
    }

    /// The description of the PR the panel shows, once read.
    fn current_description(&self) -> Option<&str> {
        let workspace = self.current_workspace_id()?;
        self.description
            .as_ref()
            .filter(|(id, _, _)| id == workspace)
            .map(|(_, _, body)| body.as_str())
    }

    pub fn toggle_description(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.current_workspace_id().map(str::to_owned) else {
            return;
        };
        if !self.description_expanded.remove(&workspace) {
            self.description_expanded.insert(workspace);
        }
        cx.notify();
    }

    fn render_description(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let body = self.current_description()?.to_owned();
        let collapsed = !self
            .current_workspace_id()
            .is_some_and(|workspace| self.description_expanded.contains(workspace));
        let colors = cx.theme().colors();
        let (text, media) = crate::media::split_media(&body);
        let mut frame = v_flex()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .overflow_hidden()
            .child(
                h_flex()
                    .id("icarus-github-description-toggle")
                    .px_2()
                    .py_1()
                    .gap_1p5()
                    .cursor_pointer()
                    .hover(|style| style.bg(colors.ghost_element_hover))
                    .on_click(
                        cx.listener(|panel, _: &ClickEvent, _, cx| panel.toggle_description(cx)),
                    )
                    .child(
                        Icon::new(IconName::FileDoc)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        div().flex_1().child(
                            Label::new("Description")
                                .size(LabelSize::Small)
                                .weight(gpui::FontWeight::SEMIBOLD),
                        ),
                    )
                    .when(!media.is_empty(), |row| {
                        row.child(
                            Label::new(match media.len() {
                                1 => "1 attachment".to_owned(),
                                count => format!("{count} attachments"),
                            })
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        )
                    })
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronUp
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    ),
            );
        if !collapsed {
            let content = if text.is_empty() && media.is_empty() {
                Label::new("No description provided.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element()
            } else {
                match self.markdown.get(DESCRIPTION_KEY) {
                    Some((_, markdown)) => div()
                        .text_ui_sm(cx)
                        .child(markdown::MarkdownElement::new(
                            markdown.clone(),
                            compact_markdown_style(markdown_style(self), cx),
                        ))
                        .into_any_element(),
                    None => Label::new(text).size(LabelSize::Small).into_any_element(),
                }
            };
            frame = frame.child(
                v_flex()
                    .px_2()
                    .py_1p5()
                    .gap_1()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(content)
                    .children(crate::media::render_media_strip(
                        "icarus-github-description",
                        &media,
                        &media,
                        cx,
                    )),
            );
        }
        Some(frame.into_any_element())
    }

    /// Polls read threads only when the PR's `updated_at` moved; `opening` the panel also reads
    /// them when they're older than [`THREADS_STALE`].
    fn threads_fetch(&self, opening: bool) -> ThreadsFetch {
        ThreadsFetch::IfChanged {
            known: self
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.threads_for.clone()),
            due: opening
                && self
                    .threads_read_at
                    .is_none_or(|at| at.elapsed() >= THREADS_STALE),
        }
    }

    /// On opening the panel: the threads come back if the PR moved or they're stale.
    fn read_threads_if_stale(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let paths = self.project_paths(cx);
        let threads_fetch = self.threads_fetch(true);
        cx.spawn(async move |this, cx| {
            let snapshot =
                Tokio::spawn_result(cx, fetch_snapshot(client, paths, threads_fetch)).await;
            this.update(cx, |panel, cx| match snapshot {
                Ok(snapshot) => panel.apply_snapshot(snapshot, cx),
                Err(error) => panel.set_offline(error, cx),
            })
        })
        .detach_and_log_err(cx);
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.refreshing = true;
        cx.notify();
        let paths = self.project_paths(cx);
        cx.spawn(async move |this, cx| {
            let snapshot =
                Tokio::spawn_result(cx, fetch_snapshot(client, paths, ThreadsFetch::Refresh)).await;
            this.update(cx, |panel, cx| match snapshot {
                Ok(snapshot) => panel.apply_snapshot(snapshot, cx),
                Err(error) => panel.set_offline(error, cx),
            })
        })
        .detach_and_log_err(cx);
    }

    fn open_stack_entry(
        &mut self,
        number: u64,
        head_ref_name: Option<String>,
        url: String,
        force_url: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let target = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.pull_request_number() == Some(number))
            .or_else(|| {
                let head_ref_name = head_ref_name.as_deref()?;
                snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.branch_name() == Some(head_ref_name))
            });
        match target {
            Some(target) if !force_url => {
                let is_current = snapshot
                    .current
                    .as_ref()
                    .is_some_and(|current| current.id == target.id);
                if is_current {
                    return;
                }
                crate::show_workspace(target.path.clone(), cx);
            }
            _ => cx.open_url(&url),
        }
    }

    /// Opens a thread's or draft's file in its diff tab, at its line.
    fn open_location(
        &mut self,
        path: &str,
        line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Some(current) = &snapshot.current else {
            return;
        };
        let base = self
            .current_pull_request()
            .map(|pull_request| pull_request.base_ref_name.clone())
            .filter(|base| !base.is_empty())
            .map(|base| DiffBase::MergeBase(format!("origin/{base}")))
            .unwrap_or(DiffBase::Uncommitted);
        let file = current.path.join(path);
        pr_comments::open_in_workspace(&self.workspace, file, line, base, window, cx);
    }

    pub fn set_thread_resolved(&mut self, thread: String, resolved: bool, cx: &mut Context<Self>) {
        self.change(ReviewOp::Resolve { thread, resolved }, cx);
    }

    pub fn remove_draft(&mut self, id: String, cx: &mut Context<Self>) {
        self.change(ReviewOp::RemoveDraft { id }, cx);
    }

    /// Opens the reply box under a thread, focused.
    pub fn start_reply(
        &mut self,
        thread: String,
        author: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let placeholder = match author {
            Some(author) => format!("Reply to @{author}…"),
            None => "Reply…".to_owned(),
        };
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 8, window, cx);
            editor.set_placeholder_text(&placeholder, window, cx);
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.reply = Some((thread, editor));
        cx.notify();
    }

    pub fn cancel_reply(&mut self, cx: &mut Context<Self>) {
        self.reply = None;
        cx.notify();
    }

    pub fn send_reply(&mut self, cx: &mut Context<Self>) {
        let Some((thread, editor)) = &self.reply else {
            return;
        };
        let body = editor.read(cx).text(cx).trim().to_owned();
        if body.is_empty() {
            return;
        }
        let thread = thread.clone();
        self.reply = None;
        let answered = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .conversation
                .iter()
                .find(|comment| comment.id == thread)
                .map(|comment| comment.author.clone())
        });
        let (author, avatar_url) = self.viewer(cx);
        let created_at = OffsetDateTime::from_unix_timestamp(self.now())
            .ok()
            .and_then(|now| now.format(&Rfc3339).ok());
        let id = self.store.update(cx, |store, _| store.pending_id());
        let op = match answered {
            // GitHub has no replies on the conversation, so like Orca an answer is a new comment
            // that mentions the author.
            Some(answered) => {
                let shown = match answered.as_deref().map(|a| a.trim_end_matches("[bot]")) {
                    Some(handle) if !handle.is_empty() => format!("@{handle} {body}"),
                    _ => body.clone(),
                };
                ReviewOp::Conversation {
                    comment: pr_comments::ConversationComment {
                        id,
                        kind: "comment".into(),
                        author,
                        author_avatar_url: avatar_url,
                        body: shown,
                        created_at,
                        ..Default::default()
                    },
                    body,
                    reply_to: answered,
                }
            }
            None => ReviewOp::Reply {
                thread,
                comment: ReviewThreadComment {
                    id,
                    author,
                    author_avatar_url: avatar_url,
                    body,
                    created_at,
                    ..ReviewThreadComment::default()
                },
            },
        };
        self.change(op, cx);
    }

    /// The signed-in user, who writes every comment sent from here.
    fn viewer(&self, cx: &App) -> (Option<String>, Option<String>) {
        let viewer = self.store_key().and_then(|key| {
            self.store
                .read(cx)
                .data(&key)
                .and_then(|data| data.viewer())
        });
        match viewer {
            Some(viewer) => (Some(viewer.login), viewer.avatar_url),
            None => (None, None),
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pull_request = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.status.as_ref())
            .and_then(|status| status.pull_request.as_ref());
        // Same height as the centre's tab bar, so the bottom borders line up across the window.
        let mut toolbar = h_flex()
            .flex_none()
            .h(ui::Tab::container_height(cx))
            .px_2()
            .gap_1p5()
            .border_b_1()
            .border_color(cx.theme().colors().border);
        match pull_request {
            Some(pull_request) => {
                let (label, tone) = pull_request_badge(pull_request);
                toolbar = toolbar
                    .child(
                        Label::new(format!("#{}", pull_request.number))
                            .size(LabelSize::Small)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(render_pill(label, tone, cx));
            }
            None => {
                toolbar = toolbar.child(
                    Label::new("Pull request")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
        }
        toolbar
            .child(div().flex_1())
            .child(
                IconButton::new("icarus-github-refresh", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .disabled(self.refreshing || self.client.is_none())
                    .tooltip(Tooltip::text("Refresh"))
                    .on_click(cx.listener(|panel, _, _window, cx| panel.refresh(cx))),
            )
            .when_some(
                pull_request.map(|pull_request| pull_request.url.clone()),
                |toolbar, url| {
                    toolbar.child(
                        IconButton::new("icarus-github-open", IconName::ArrowUpRight)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Open on GitHub"))
                            .on_click(move |_, _window, cx| cx.open_url(&url)),
                    )
                },
            )
    }

    fn render_message(
        &self,
        title: impl Into<SharedString>,
        detail: Option<String>,
    ) -> impl IntoElement {
        v_flex()
            .p_2()
            .gap_1()
            .child(Label::new(title).size(LabelSize::Small))
            .when_some(detail, |message, detail| {
                message.child(
                    Label::new(detail)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
    }

    /// A section: the shared header, then `body` unless it is collapsed.
    fn render_section(
        &self,
        section: PrSection,
        count: Option<String>,
        trailing: Vec<AnyElement>,
        body: impl FnOnce(&Self, &mut Context<Self>) -> AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed = self.is_collapsed(section);
        let mut header = SectionHeader::new(
            SharedString::from(format!("icarus-github-{}-header", section.id())),
            section.label(),
            collapsed,
        );
        if let Some(count) = count {
            header = header.count(count);
        }
        for element in trailing {
            header = header.trailing(element);
        }
        let toggle = cx.entity().downgrade();
        let target = toggle.clone();
        let menu = toggle.clone();
        let layout = self.layout.clone();
        let header = header.render(
            DraggedPrSection(section),
            move |_, cx| {
                toggle
                    .update(cx, |panel, cx| panel.toggle_collapsed(section, cx))
                    .log_err();
            },
            move |dragged: &DraggedPrSection, _, cx| {
                target
                    .update(cx, |panel, cx| panel.drop_section(dragged.0, section, cx))
                    .log_err();
            },
            move |window, cx| {
                let on_move = {
                    let menu = menu.clone();
                    move |section, up, cx: &mut App| {
                        menu.update(cx, |panel, cx| panel.move_section_by_one(section, up, cx))
                            .log_err();
                    }
                };
                let on_toggle = {
                    let menu = menu.clone();
                    move |section, cx: &mut App| {
                        menu.update(cx, |panel, cx| panel.toggle_section_hidden(section, cx))
                            .log_err();
                    }
                };
                sections_menu(
                    Some(section),
                    layout.clone(),
                    on_move,
                    on_toggle,
                    window,
                    cx,
                )
            },
        );
        let height = self.section_height(section);
        v_flex()
            .id(SharedString::from(format!(
                "icarus-github-{}",
                section.id()
            )))
            .debug_selector(|| format!("pr-section:{}", section.id()))
            .w_full()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .on_drag_move(cx.listener(
                move |panel, event: &gpui::DragMoveEvent<DraggedSectionSize>, _, cx| {
                    if event.drag(cx).0 != section {
                        return;
                    }
                    let body = event.event.position.y - event.bounds.top() - SECTION_HEADER_HEIGHT;
                    panel.set_section_height(section, Some(body.max(MIN_SECTION_HEIGHT)), cx);
                },
            ))
            .child(header)
            .when(!collapsed, |frame| {
                let body = div().w_full().pb_1().child(body(self, cx));
                frame
                    .child(match height {
                        Some(height) => div()
                            .id(SharedString::from(format!(
                                "icarus-github-{}-body",
                                section.id()
                            )))
                            .h(height)
                            .overflow_y_scroll()
                            .child(body)
                            .into_any_element(),
                        None => body.into_any_element(),
                    })
                    .child(self.render_resize_handle(section, cx))
            })
            .into_any_element()
    }

    /// The strip under an open section: drag it to size the section, double-click for natural height.
    fn render_resize_handle(&self, section: PrSection, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(SharedString::from(format!(
                "icarus-github-{}-resize",
                section.id()
            )))
            .w_full()
            .h(px(4.))
            .mt(px(-2.))
            .cursor_row_resize()
            .hover(|style| style.bg(cx.theme().colors().border_focused))
            .on_drag(DraggedSectionSize(section), |dragged, _, _, cx| {
                cx.new(|_| dragged.clone())
            })
            .on_click(cx.listener(move |panel, event: &ClickEvent, _, cx| {
                if event.click_count() == 2 {
                    panel.set_section_height(section, None, cx);
                }
            }))
    }

    fn section_height(&self, section: PrSection) -> Option<Pixels> {
        let workspace = self.current_workspace_id()?.to_owned();
        self.heights.get(&(workspace, section)).copied()
    }

    /// Sets a section's body height for this workspace; `None` gives back its natural height.
    pub fn set_section_height(
        &mut self,
        section: PrSection,
        height: Option<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.current_workspace_id().map(str::to_owned) else {
            return;
        };
        match height {
            Some(height) => self.heights.insert((workspace, section), height),
            None => self.heights.remove(&(workspace, section)),
        };
        cx.notify();
    }

    fn render_empty_line(text: &'static str) -> AnyElement {
        div()
            .px_2()
            .py_0p5()
            .child(Label::new(text).size(LabelSize::XSmall).color(Color::Muted))
            .into_any_element()
    }

    /// The summary parts, e.g. "1 failed · 24 passed", for a header's right side.
    fn render_summary(parts: Vec<(String, Color)>) -> AnyElement {
        let mut row = h_flex().gap_1();
        for (index, (text, color)) in parts.into_iter().enumerate() {
            if index > 0 {
                row = row.child(Label::new("·").size(LabelSize::XSmall).color(Color::Muted));
            }
            row = row.child(Label::new(text).size(LabelSize::XSmall).color(color));
        }
        row.into_any_element()
    }

    fn render_stack(
        &self,
        stack: &StackView,
        pull_request: &PullRequest,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let expanded = self.stack_expanded;
        let header = h_flex()
            .id("icarus-github-stack-toggle")
            .px_2()
            .py_1()
            .gap_1p5()
            .cursor_pointer()
            .hover(|style| style.bg(colors.ghost_element_hover))
            .on_click(cx.listener(|panel, _, _window, cx| {
                panel.stack_expanded = !panel.stack_expanded;
                cx.notify();
            }))
            .child(
                Icon::new(IconName::PullRequest)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new(format!("Stack #{}", stack.number))
                    .size(LabelSize::Small)
                    .weight(gpui::FontWeight::SEMIBOLD),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(format!(
                        "{} of {} · {}",
                        stack.position, stack.size, stack.base_ref
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
                ),
            )
            .child(
                Icon::new(if expanded {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::Small)
                .color(Color::Muted),
            );
        let mut frame = v_flex()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .overflow_hidden()
            .child(header);
        if expanded {
            let mut rows = v_flex()
                .border_t_1()
                .border_color(colors.border_variant)
                .py_0p5();
            if stack.entries.is_none() {
                rows = rows.child(
                    div().px_2().py_1().child(
                        Label::new("Stack details are temporarily unavailable.")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                );
            }
            for entry in stack_rows(stack, pull_request) {
                let (badge, tone) = stack_entry_badge(&entry);
                let is_current = entry.number == pull_request.number;
                let url = entry
                    .url
                    .clone()
                    .unwrap_or_else(|| pull_request.pull_url(entry.number));
                let number = entry.number;
                let head_ref_name = entry.head_ref_name.clone();
                rows = rows.child(
                    h_flex()
                        .id(SharedString::from(format!("icarus-stack-entry-{number}")))
                        .px_2()
                        .py_0p5()
                        .gap_1p5()
                        .cursor_pointer()
                        .when(is_current, |row| row.bg(colors.element_selected))
                        .hover(|style| style.bg(colors.ghost_element_hover))
                        .on_click(cx.listener(move |panel, event: &ClickEvent, _, cx| {
                            let modifiers = event.modifiers();
                            let force_url =
                                modifiers.platform || modifiers.control || modifiers.shift;
                            panel.open_stack_entry(
                                number,
                                head_ref_name.clone(),
                                url.clone(),
                                force_url,
                                cx,
                            );
                        }))
                        .child(
                            div().w(px(36.)).flex_none().child(
                                Label::new(format!("#{number}"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                        )
                        .when(
                            entry.author.is_some() || entry.author_avatar_url.is_some(),
                            |row| {
                                row.child(render_avatar(
                                    entry.author_avatar_url.as_deref(),
                                    entry.author.as_deref().unwrap_or_default(),
                                    AvatarKind::Person,
                                    px(14.),
                                    cx,
                                ))
                            },
                        )
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(entry.title.clone().unwrap_or_default())
                                    .size(LabelSize::XSmall)
                                    .truncate(),
                            ),
                        )
                        .child(render_pill(badge, tone, cx)),
                );
            }
            rows = rows.child(
                h_flex()
                    .px_2()
                    .py_0p5()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::GitBranch)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(stack.base_ref.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            );
            frame = frame.child(rows);
        }
        frame.when(stack.out_of_date, |frame| {
            frame.child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1p5()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .color(Color::Warning),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(format!(
                                "This stack is out of date with {}",
                                stack.base_ref
                            ))
                            .size(LabelSize::XSmall)
                            .truncate(),
                        ),
                    ),
            )
        })
    }

    fn render_merge_queue(
        &self,
        pull_request: &PullRequest,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        if !pull_request.is_in_merge_queue {
            return None;
        }
        let entry = pull_request.merge_queue_entry.as_ref();
        let mut text = String::from("In merge queue");
        if let Some(position) = entry.and_then(|entry| entry.position) {
            text.push_str(&format!(" · position {position}"));
        }
        if let Some(eta) = entry.and_then(|entry| entry.eta_seconds) {
            text.push_str(&format!(" · ~{}", format_duration(eta as i64)));
        }
        let detail = entry.and_then(|entry| match entry.state {
            MergeQueueEntryState::AwaitingChecks => Some("Queue checks running"),
            MergeQueueEntryState::Mergeable => Some("Merging next"),
            MergeQueueEntryState::Unmergeable => Some("Will be removed from the queue"),
            MergeQueueEntryState::Locked => Some("Locked while merging"),
            MergeQueueEntryState::Queued | MergeQueueEntryState::Unknown => None,
        });
        let color = tone_color(Tone::Queued, cx)
            .map(Color::Custom)
            .unwrap_or(Color::Warning);
        Some(
            h_flex()
                .gap_1p5()
                .child(
                    Icon::new(IconName::QueueMessage)
                        .size(IconSize::Small)
                        .color(color),
                )
                .child(Label::new(text).size(LabelSize::Small).color(color))
                .when_some(detail, |row, detail| {
                    row.child(
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                }),
        )
    }

    fn render_attention(items: Vec<AttentionItem>) -> AnyElement {
        let mut section = v_flex().gap_1().px_2();
        for item in items {
            section = section.child(
                h_flex()
                    .items_start()
                    .gap_1p5()
                    .child(
                        div().pt_0p5().child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::Small)
                                .color(Color::Error),
                        ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(item.title).size(LabelSize::Small))
                            .child(
                                Label::new(item.reason)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    ),
            );
        }
        section.into_any_element()
    }

    fn render_previews(previews: Vec<PreviewDeployment>, cx: &App) -> AnyElement {
        let colors = cx.theme().colors();
        let mut section = v_flex().gap_0p5().px_1();
        for (index, preview) in previews.into_iter().enumerate() {
            let (badge, tone) = preview_state_badge(&preview.state);
            let detail = match (&preview.environment, preview.short_sha()) {
                (Some(environment), Some(sha)) => Some(format!("{environment} · {sha}")),
                (Some(environment), None) => Some(environment.clone()),
                (None, Some(sha)) => Some(sha.to_string()),
                (None, None) => None,
            };
            section = section.child(
                v_flex()
                    .id(SharedString::from(format!("icarus-preview-{index}")))
                    .px_1()
                    .py_0p5()
                    .rounded_md()
                    .when(preview.url.is_some(), |row| {
                        row.cursor_pointer()
                            .hover(|style| style.bg(colors.ghost_element_hover))
                    })
                    .when_some(preview.url.clone(), |row, url| {
                        row.on_click(move |_, _window, cx| cx.open_url(&url))
                    })
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Icon::from_path(crate::links::GLOBE_ICON)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(preview.app.clone()).size(LabelSize::Small))
                            .child(div().flex_1().min_w_0().when_some(detail, |cell, detail| {
                                cell.child(
                                    Label::new(detail)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                            }))
                            .when(!preview.is_head, |row| {
                                row.child(
                                    Label::new("behind head")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Warning)
                                        .single_line(),
                                )
                            })
                            .child(render_pill(badge, tone, cx)),
                    )
                    .child(
                        div().pl_5().child(
                            Label::new(preview.url.clone().unwrap_or_else(|| "No URL yet".into()))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    ),
            );
        }
        section.into_any_element()
    }

    /// Failing and running checks, then the passed and skipped ones folded behind a toggle row.
    fn render_checks(&self, pull_request: &PullRequest, cx: &mut Context<Self>) -> AnyElement {
        if pull_request.checks.is_empty() {
            return Self::render_empty_line("No checks reported.");
        }
        let counts = CheckCounts::of(&pull_request.checks);
        let expanded = self.checks_expanded();
        let mut section = v_flex();
        let mut folded_at = None;
        for (index, check) in sorted_checks(&pull_request.checks).into_iter().enumerate() {
            let outcome = check_outcome(check);
            let settled = matches!(
                outcome.bucket(),
                CheckBucket::Passing | CheckBucket::Neutral
            );
            if settled && folded_at.is_none() {
                folded_at = Some(index);
                section = section.child(self.render_checks_toggle(&counts, expanded, cx));
                if !expanded {
                    break;
                }
            }
            section = section.child(self.render_check(index, check));
        }
        section.into_any_element()
    }

    fn render_checks_toggle(
        &self,
        counts: &CheckCounts,
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .id("icarus-github-checks-fold")
            .h(px(24.))
            .px_2()
            .gap_1p5()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| panel.toggle_checks_expanded(cx)))
            .child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(
                Label::new(if expanded {
                    format!("Hide {}", counts.settled_label())
                } else {
                    counts.settled_label()
                })
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
    }

    fn render_check(&self, index: usize, check: &Check) -> impl IntoElement {
        let outcome = check_outcome(check);
        let conclusion_color = match outcome.bucket() {
            CheckBucket::Failing => Color::Error,
            CheckBucket::Running => Color::Warning,
            CheckBucket::Passing | CheckBucket::Neutral => Color::Muted,
        };
        let now = self.now();
        h_flex()
            .h(px(24.))
            .px_2()
            .gap_1p5()
            .child(check_icon(outcome, self.animate))
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(check.name.clone())
                        .size(LabelSize::Small)
                        .truncate(),
                ),
            )
            .when_some(check_duration(check, now), |row, duration| {
                row.child(
                    Label::new(duration)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .single_line(),
                )
            })
            .child(
                Label::new(outcome.label())
                    .size(LabelSize::XSmall)
                    .color(conclusion_color)
                    .single_line(),
            )
            .child(
                div()
                    .w(SLOT)
                    .flex_none()
                    .when_some(check.url.clone(), |slot, url| {
                        slot.child(
                            IconButton::new(
                                SharedString::from(format!("icarus-check-link-{index}")),
                                IconName::ArrowUpRight,
                            )
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Open check details"))
                            .on_click(move |_, _window, cx| cx.open_url(&url)),
                        )
                    }),
            )
    }

    fn render_pull_request(
        &self,
        status: &PrStatus,
        pull_request: &PullRequest,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let updated = pull_request
            .updated_at
            .as_deref()
            .and_then(parse_timestamp)
            .map(|updated| format!("Updated {}", format_relative(self.now() - updated)));
        v_flex()
            .gap_2()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(Label::new(pull_request.title.clone()).size(LabelSize::Small))
                    .child(
                        h_flex()
                            .gap_1()
                            .when_some(pull_request.author.clone(), |row, author| {
                                row.child(render_avatar(
                                    pull_request.author_avatar_url.as_deref(),
                                    &author,
                                    AvatarKind::Person,
                                    px(16.),
                                    cx,
                                ))
                                .child(
                                    Label::new(format!("@{author} ·"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .single_line(),
                                )
                            })
                            .child(
                                Label::new(format!(
                                    "{} → {}",
                                    pull_request.head_ref_name, pull_request.base_ref_name
                                ))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                            )
                            .when_some(updated, |row, updated| {
                                row.child(
                                    Label::new(format!("· {updated}"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .single_line(),
                                )
                            }),
                    ),
            )
            .children(self.render_description(cx))
            .when_some(status.stack(), |column, stack| {
                column.child(self.render_stack(stack, pull_request, cx))
            })
            .children(self.render_merge_queue(pull_request, cx))
    }

    /// The PR's sections in the saved order, each only when it has something to show.
    fn render_sections(
        &self,
        status: &PrStatus,
        pull_request: &PullRequest,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let threads = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.threads.clone())
            .unwrap_or_default();
        let drafts: Vec<ReviewDraft> = self
            .review
            .as_ref()
            .and_then(|section| section.read(cx).view().map(|view| view.drafts.clone()))
            .unwrap_or_default();
        let mut sections = Vec::new();
        for section in self.layout.visible::<PrSection>() {
            let element = match section {
                PrSection::Attention => {
                    let items = attention_items(pull_request);
                    if items.is_empty() {
                        continue;
                    }
                    let count = items.len().to_string();
                    self.render_section(
                        section,
                        Some(count),
                        Vec::new(),
                        move |_, _| Self::render_attention(items),
                        cx,
                    )
                }
                PrSection::Previews => {
                    let previews =
                        ordered_previews(&status.previews, status.primary_preview.as_ref());
                    if previews.is_empty() {
                        continue;
                    }
                    let count = previews.len().to_string();
                    self.render_section(
                        section,
                        Some(count),
                        Vec::new(),
                        move |_, cx| Self::render_previews(previews, cx),
                        cx,
                    )
                }
                PrSection::Checks => {
                    let counts = CheckCounts::of(&pull_request.checks);
                    self.render_section(
                        section,
                        Some(pull_request.checks.len().to_string()),
                        vec![Self::render_summary(counts.parts())],
                        |panel, cx| panel.render_checks(pull_request, cx),
                        cx,
                    )
                }
                PrSection::PendingReview => {
                    if drafts.is_empty() {
                        continue;
                    }
                    let drafts = drafts.clone();
                    self.render_section(
                        section,
                        Some(drafts.len().to_string()),
                        Vec::new(),
                        move |panel, cx| panel.render_drafts(drafts, cx),
                        cx,
                    )
                }
                PrSection::Comments => {
                    let open: Vec<ReviewThread> = pr_comments::threads_by_activity(&threads, false)
                        .into_iter()
                        .cloned()
                        .collect();
                    let conversation = self
                        .snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.conversation.clone())
                        .unwrap_or_default();
                    let count = pr_comments::audience_count(
                        &open,
                        &conversation,
                        pr_comments::Audience::All,
                    )
                    .to_string();
                    self.render_section(
                        section,
                        Some(count),
                        Vec::new(),
                        move |panel, cx| panel.render_comments(open, conversation, cx),
                        cx,
                    )
                }
                PrSection::Resolved => {
                    let resolved = pr_comments::threads_by_activity(&threads, true);
                    if resolved.is_empty() {
                        continue;
                    }
                    let count = resolved.len().to_string();
                    let resolved: Vec<ReviewThread> = resolved.into_iter().cloned().collect();
                    self.render_section(
                        section,
                        Some(count),
                        Vec::new(),
                        move |panel, cx| panel.render_resolved(resolved, cx),
                        cx,
                    )
                }
            };
            sections.push(element);
        }
        sections
    }

    /// The audience tabs, then a card per open thread, issue comment and review summary.
    fn render_comments(
        &self,
        threads: Vec<ReviewThread>,
        conversation: Vec<pr_comments::ConversationComment>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let audience = self.audience();
        let list = v_flex().gap_1p5().child(self.render_audience_tabs(
            &threads,
            &conversation,
            audience,
            cx,
        ));
        let items = pr_comments::comment_items(&threads, &conversation, audience);
        if items.is_empty() {
            return list
                .child(Self::render_empty_line(audience.empty_text()))
                .into_any_element();
        }
        list.children(items.into_iter().map(|item| match item {
            pr_comments::CommentItem::Thread(thread) => self.render_thread_card(thread, cx),
            pr_comments::CommentItem::Conversation(comment) => {
                self.render_conversation_card(comment, cx)
            }
        }))
        .into_any_element()
    }

    fn render_resolved(&self, threads: Vec<ReviewThread>, cx: &mut Context<Self>) -> AnyElement {
        if threads.is_empty() {
            return Self::render_empty_line("No resolved threads.").into_any_element();
        }
        v_flex()
            .gap_1p5()
            .children(
                threads
                    .iter()
                    .map(|thread| self.render_thread_card(thread, cx)),
            )
            .into_any_element()
    }

    fn audience(&self) -> pr_comments::Audience {
        self.current_workspace_id()
            .and_then(|workspace| self.audiences.get(workspace).copied())
            .unwrap_or_default()
    }

    pub fn set_audience(&mut self, audience: pr_comments::Audience, cx: &mut Context<Self>) {
        let Some(workspace) = self.current_workspace_id().map(str::to_owned) else {
            return;
        };
        self.audiences.insert(workspace, audience);
        cx.notify();
    }

    fn render_audience_tabs(
        &self,
        threads: &[ReviewThread],
        conversation: &[pr_comments::ConversationComment],
        selected: pr_comments::Audience,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let mut tabs = h_flex()
            .mx_2()
            .p_0p5()
            .gap_0p5()
            .rounded_md()
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.editor_background);
        for audience in pr_comments::Audience::ALL {
            let count = pr_comments::audience_count(threads, conversation, audience);
            let active = audience == selected;
            tabs = tabs.child(
                h_flex()
                    .id(SharedString::from(format!(
                        "icarus-github-audience-{}",
                        audience.label()
                    )))
                    .flex_1()
                    .h(px(22.))
                    .justify_center()
                    .gap_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(active, |tab| tab.bg(colors.element_selected))
                    .hover(|style| style.bg(colors.element_hover))
                    .tooltip(Tooltip::text(audience.label()))
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                        panel.set_audience(audience, cx)
                    }))
                    .child(
                        Icon::new(audience.icon())
                            .size(IconSize::XSmall)
                            .color(if active { Color::Default } else { Color::Muted }),
                    )
                    .child(
                        Label::new(count.to_string())
                            .size(LabelSize::XSmall)
                            .color(if active { Color::Default } else { Color::Muted }),
                    ),
            );
        }
        tabs
    }

    /// Orca's comment card: each comment with avatar, author, time and body, the first with the
    /// `file:line` link; then the reply box and the thread's actions.
    fn render_thread_card(&self, thread: &ReviewThread, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let busy = self.busy_threads.contains(&thread.id);
        let replying = self
            .reply
            .as_ref()
            .filter(|(id, _)| *id == thread.id)
            .map(|(_, editor)| editor.clone());
        let mut card = pr_comments::comment_card(cx)
            .mx_2()
            .border_color(colors.border)
            .when(thread.is_resolved, |card| card.opacity(0.6));
        for (index, comment) in thread.comments.iter().enumerate() {
            card = card.child(self.render_comment(thread, index, comment, cx));
        }
        if let Some(editor) = replying {
            card = card.child(self.render_reply_box(editor, busy, cx));
        }
        card.into_any_element()
    }

    /// A click handler that runs `action` on this panel.
    fn handler(
        &self,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> pr_comments::ClickHandler {
        let weak = cx.entity().downgrade();
        Rc::new(move |window, cx| {
            weak.update(cx, |panel, cx| action(panel, window, cx))
                .log_err();
        })
    }

    fn react_handler(&self, cx: &mut Context<Self>) -> pr_comments::ReactHandler {
        let weak = cx.entity().downgrade();
        Rc::new(move |subject, content, give, cx| {
            weak.update(cx, |panel, cx| panel.react(subject, content, !give, cx))
                .log_err();
        })
    }

    /// Orca's More menu: send to the agent, open on GitHub, and edit or delete your own comment.
    fn comment_menu(
        &self,
        comment: &crate::review::ThreadComment,
        send: pr_comments::ClickHandler,
        can_edit: bool,
        can_delete: bool,
        cx: &mut Context<Self>,
    ) -> Vec<pr_comments::CommentAction> {
        let mut menu = vec![pr_comments::CommentAction {
            label: "Send to Agent",
            icon: IconName::Send,
            destructive: false,
            handler: send,
        }];
        if let Some(url) = comment.url.clone() {
            menu.push(pr_comments::CommentAction {
                label: "Open on GitHub",
                icon: IconName::ArrowUpRight,
                destructive: false,
                handler: Rc::new(move |_, cx| cx.open_url(&url)),
            });
        }
        let viewer = self.viewer(cx).0;
        let own = pr_comments::is_own(comment.author.as_deref(), viewer.as_deref());
        if own && can_edit {
            let id = comment.id.clone();
            menu.push(pr_comments::CommentAction {
                label: "Edit",
                icon: IconName::Pencil,
                destructive: false,
                handler: self.handler(
                    move |panel, window, cx| panel.start_edit(id.clone(), window, cx),
                    cx,
                ),
            });
        }
        if own && can_delete {
            let id = comment.id.clone();
            menu.push(pr_comments::CommentAction {
                label: "Delete",
                icon: IconName::Trash,
                destructive: true,
                handler: self.handler(move |panel, _, cx| panel.delete_comment(id.clone(), cx), cx),
            });
        }
        menu
    }

    fn render_comment(
        &self,
        thread: &ReviewThread,
        index: usize,
        comment: &crate::review::ThreadComment,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let first = index == 0;
        let path = thread.path.clone();
        let line = pr_comments::thread_line(thread);
        let location = first.then(|| {
            div()
                .id(SharedString::from(format!(
                    "icarus-thread-link-{}",
                    thread.id
                )))
                .min_w_0()
                .cursor_pointer()
                .tooltip(Tooltip::text("Open in the diff"))
                .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                    panel.open_location(&path, line, window, cx)
                }))
                .child(
                    Label::new(pr_comments::thread_location(thread))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .buffer_font(cx)
                        .truncate(),
                )
                .into_any_element()
        });
        let gallery: Vec<crate::media::MediaRef> = thread
            .comments
            .iter()
            .flat_map(|comment| crate::media::split_media(&comment.body).1)
            .collect();
        let pending = comment.id.starts_with(crate::pr_store::PENDING_PREFIX);
        let mut buttons: Vec<(&'static str, pr_comments::ClickHandler)> = Vec::new();
        let mut menu = Vec::new();
        let mut send_button = None;
        if !pending {
            if first {
                let can_toggle = if thread.is_resolved {
                    thread.viewer_can_unresolve
                } else {
                    thread.viewer_can_resolve
                };
                let resolved = thread.is_resolved;
                if can_toggle {
                    let id = thread.id.clone();
                    buttons.push((
                        if resolved { "Unresolve" } else { "Resolve" },
                        self.handler(
                            move |panel, _, cx| {
                                panel.set_thread_resolved(id.clone(), !resolved, cx)
                            },
                            cx,
                        ),
                    ));
                }
                if thread.viewer_can_reply && !resolved {
                    let (id, author) = (thread.id.clone(), comment.author.clone());
                    buttons.push((
                        "Reply",
                        self.handler(
                            move |panel, window, cx| {
                                panel.start_reply(id.clone(), author.clone(), window, cx)
                            },
                            cx,
                        ),
                    ));
                }
            }
            let thread_id = thread.id.clone();
            let send = self.handler(
                move |panel, _, cx| panel.send_thread_to_agent(thread_id.clone(), cx),
                cx,
            );
            send_button = (index + 1 == thread.comments.len()).then(|| send.clone());
            menu = self.comment_menu(
                comment,
                send,
                comment.viewer_can_update,
                comment.viewer_can_delete,
                cx,
            );
        }
        let body = match self.editing.as_ref().filter(|(id, _)| *id == comment.id) {
            Some((_, editor)) => self.render_edit_box(editor.clone(), cx).into_any_element(),
            None => self.render_comment_body(comment, &gallery, cx),
        };
        pr_comments::render_comment(
            pr_comments::CommentParts {
                id: format!("icarus-comment-{}-{index}", thread.id),
                comment: comment.clone(),
                first,
                now: self.now(),
                review_state: None,
                location,
                outdated: first && thread.placement.outdated,
                resolved: thread.is_resolved,
                buttons,
                menu,
                send: send_button,
                body,
                react: (!pending).then(|| self.react_handler(cx)),
            },
            cx,
        )
    }

    /// An issue comment or review summary: one comment, no thread to reply in or resolve.
    fn render_conversation_card(
        &self,
        conversation: &pr_comments::ConversationComment,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let comment = conversation.as_thread_comment();
        let (_, gallery) = crate::media::split_media(&comment.body);
        let editing = self
            .editing
            .as_ref()
            .filter(|(id, _)| *id == conversation.id)
            .map(|(_, editor)| editor.clone());
        let replying = self
            .reply
            .as_ref()
            .filter(|(id, _)| *id == conversation.id)
            .map(|(_, editor)| editor.clone());
        let body = match editing {
            Some(editor) => self.render_edit_box(editor, cx).into_any_element(),
            None => self.render_comment_body(&comment, &gallery, cx),
        };
        let pending = conversation.is_pending();
        let mut buttons: Vec<(&'static str, pr_comments::ClickHandler)> = Vec::new();
        let mut menu = Vec::new();
        let mut send_button = None;
        if !pending {
            let (id, author) = (conversation.id.clone(), conversation.author.clone());
            buttons.push((
                "Reply",
                self.handler(
                    move |panel, window, cx| {
                        panel.start_reply(id.clone(), author.clone(), window, cx)
                    },
                    cx,
                ),
            ));
            let id = conversation.id.clone();
            let send = self.handler(
                move |panel, _, cx| panel.send_conversation_to_agent(id.clone(), cx),
                cx,
            );
            send_button = Some(send.clone());
            menu = self.comment_menu(
                &comment,
                send,
                conversation.can_edit(),
                conversation.can_delete(),
                cx,
            );
        }
        pr_comments::comment_card(cx)
            .mx_2()
            .child(pr_comments::render_comment(
                pr_comments::CommentParts {
                    id: format!("icarus-conversation-{}", conversation.id),
                    comment,
                    first: true,
                    now: self.now(),
                    review_state: conversation.review_state(),
                    location: None,
                    outdated: false,
                    resolved: false,
                    buttons,
                    menu,
                    send: send_button,
                    body,
                    react: (!pending).then(|| self.react_handler(cx)),
                },
                cx,
            ))
            .children(replying.map(|editor| self.render_reply_box(editor, false, cx)))
            .into_any_element()
    }

    fn render_edit_box(&self, editor: Entity<Editor>, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        v_flex()
            .m_1p5()
            .p_1p5()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.editor_background)
            .key_context("IcarusReply")
            .on_action(
                cx.listener(|panel, _: &crate::review::CancelReviewComment, _, cx| {
                    panel.cancel_edit(cx)
                }),
            )
            .on_action(
                cx.listener(|panel, _: &crate::review::SubmitReviewComment, _, cx| {
                    panel.save_edit(cx)
                }),
            )
            .child(editor)
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("icarus-edit-cancel", "Cancel")
                            .label_size(LabelSize::Small)
                            .on_click(
                                cx.listener(|panel, _: &ClickEvent, _, cx| panel.cancel_edit(cx)),
                            ),
                    )
                    .child(
                        Button::new("icarus-edit-save", "Save")
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .on_click(
                                cx.listener(|panel, _: &ClickEvent, _, cx| panel.save_edit(cx)),
                            ),
                    ),
            )
    }

    /// The comment's Markdown, then its images and videos; the viewer steps through `gallery`.
    fn render_comment_body(
        &self,
        comment: &crate::review::ThreadComment,
        gallery: &[crate::media::MediaRef],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (text, media) = crate::media::split_media(&comment.body);
        let text = match self.markdown.get(&comment.id) {
            Some((_, markdown)) => div()
                .child(markdown::MarkdownElement::new(
                    markdown.clone(),
                    markdown_style(self),
                ))
                .into_any_element(),
            None => Label::new(text).size(LabelSize::Small).into_any_element(),
        };
        v_flex()
            .gap_1()
            .child(text)
            .children(crate::media::render_media_strip(
                &format!("icarus-github-{}", comment.id),
                &media,
                &gallery,
                cx,
            ))
            .into_any_element()
    }

    fn render_reply_box(
        &self,
        editor: Entity<Editor>,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        v_flex()
            .mx_2()
            .mb_1p5()
            .p_1p5()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.editor_background)
            .key_context("IcarusReply")
            .on_action(
                cx.listener(|panel, _: &crate::review::CancelReviewComment, _, cx| {
                    panel.cancel_reply(cx)
                }),
            )
            .on_action(
                cx.listener(|panel, _: &crate::review::SubmitReviewComment, _, cx| {
                    panel.send_reply(cx)
                }),
            )
            .child(editor)
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("icarus-reply-cancel", "Cancel")
                            .label_size(LabelSize::Small)
                            .on_click(
                                cx.listener(|panel, _: &ClickEvent, _, cx| panel.cancel_reply(cx)),
                            ),
                    )
                    .child(
                        Button::new("icarus-reply-send", if busy { "Sending…" } else { "Reply" })
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .disabled(busy)
                            .on_click(
                                cx.listener(|panel, _: &ClickEvent, _, cx| panel.send_reply(cx)),
                            ),
                    ),
            )
    }

    fn render_drafts(&self, drafts: Vec<ReviewDraft>, cx: &mut Context<Self>) -> AnyElement {
        let mut list = v_flex();
        for (index, draft) in drafts.into_iter().enumerate() {
            let path = draft.path.clone();
            let line = draft.line;
            let id = draft.id.clone();
            list = list.child(
                h_flex()
                    .id(("icarus-github-draft", index))
                    .group("icarus-github-draft")
                    .min_h(px(24.))
                    .px_2()
                    .gap_1p5()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                    .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                        panel.open_location(&path, line, window, cx)
                    }))
                    .child(
                        Icon::new(IconName::Pencil)
                            .size(IconSize::XSmall)
                            .color(Color::Warning),
                    )
                    .child(
                        Label::new(pr_comments::draft_location(&draft))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line(),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(pr_comments::excerpt(&draft.body))
                                .size(LabelSize::Small)
                                .truncate(),
                        ),
                    )
                    .child(
                        div().w(SLOT).flex_none().child(
                            IconButton::new(("icarus-github-draft-remove", index), IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Delete pending comment"))
                                .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                    cx.stop_propagation();
                                    panel.remove_draft(id.clone(), cx);
                                })),
                        ),
                    ),
            );
        }
        list.into_any_element()
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(reason) = &self.offline {
            return v_flex()
                .p_2()
                .gap_1()
                .child(Label::new("The Icarus server isn't running.").size(LabelSize::Small))
                .child(
                    Label::new("Start it with `icarus server start`.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(reason.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element();
        }
        let Some(snapshot) = &self.snapshot else {
            return self
                .render_message("Connecting to Icarus…", None)
                .into_any_element();
        };
        let Some(current) = &snapshot.current else {
            return self
                .render_message(
                    "This window has no Icarus workspace.",
                    Some(
                        "Open a workspace from the Workspaces panel to see its pull request."
                            .into(),
                    ),
                )
                .into_any_element();
        };
        let Some(status) = &snapshot.status else {
            return self
                .render_message("Loading pull request…", None)
                .into_any_element();
        };
        let branch = current.branch_name().unwrap_or("this branch").to_string();
        let rate_limit = status.rate_limited_until.map(rate_limit_note);
        match &status.pull_request {
            Some(pull_request) => v_flex()
                .w_full()
                .when_some(rate_limit, |column, note| {
                    column.child(
                        div()
                            .px_2()
                            .pt_1()
                            .child(Label::new(note).size(LabelSize::XSmall).color(Color::Muted)),
                    )
                })
                .child(
                    div()
                        .p_2()
                        .child(self.render_pull_request(status, pull_request, cx)),
                )
                .when_some(self.action_error.clone(), |column, error| {
                    column.child(
                        div().px_2().pb_1().child(
                            Label::new(error)
                                .size(LabelSize::XSmall)
                                .color(Color::Error),
                        ),
                    )
                })
                .children(self.render_sections(status, pull_request, cx))
                .into_any_element(),
            None if !status.known => match rate_limit {
                Some(note) => self.render_message(note, None).into_any_element(),
                None => self
                    .render_message("Couldn't load the pull request.", status.error.clone())
                    .into_any_element(),
            },
            None => self
                .render_message(
                    format!("No pull request for {branch}"),
                    Some(status.error.clone().unwrap_or_else(|| {
                        "Push the branch and open a PR to see its checks here.".into()
                    })),
                )
                .into_any_element(),
        }
    }
}

impl Render for GitHubPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_review_section(window, cx);
        self.sync_markdown(window, cx);
        v_flex()
            .key_context("IcarusGitHubPanel")
            .debug_selector(|| "pr-panel".into())
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|panel, _: &RefreshGitHubPanel, _window, cx| {
                panel.refresh(cx);
            }))
            .child(self.render_toolbar(cx))
            .child(
                v_flex()
                    .id("icarus-github")
                    .size_full()
                    .overflow_y_scroll()
                    .child(self.render_body(cx)),
            )
    }
}

impl Focusable for GitHubPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for GitHubPanel {}

impl Panel for GitHubPanel {
    fn persistent_name() -> &'static str {
        "IcarusGitHubPanel"
    }

    fn panel_key() -> &'static str {
        "IcarusGitHubPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(340.)
    }

    fn initial_size_state(&self, _window: &Window, _cx: &App) -> PanelSizeState {
        PanelSizeState {
            size: None,
            flex: None,
        }
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListChecks)
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        let became_active = active && !self.active;
        self.active = active;
        if became_active {
            // Deferred: the workspace is mid-update while it activates a panel, and this reads it.
            cx.defer_in(window, |this, _, cx| this.read_threads_if_stale(cx));
        }
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Checks")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        ToggleGitHubPanel.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

/// Focuses a panel, opening its dock; when it already has focus, hides it, as Orca's sidebar keys do.
pub(crate) fn toggle_panel<T: Panel>(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let focused = workspace
        .panel::<T>(cx)
        .is_some_and(|panel| panel.focus_handle(cx).contains_focused(window, cx));
    if focused {
        workspace.close_panel::<T>(window, cx);
    } else {
        workspace.focus_panel::<T>(window, cx);
    }
}

pub(crate) fn register(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleGitHubPanel, window, cx| {
            toggle_panel::<GitHubPanel>(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &FocusReviewSummary, window, cx| {
            crate::changes_tab::reveal_source_control(workspace, window, cx);
            let Some(panel) = workspace.panel::<GitHubPanel>(cx) else {
                return;
            };
            let section = panel.update(cx, |panel, cx| {
                panel.sync_review_section(window, cx);
                panel.review_section().cloned()
            });
            if let Some(section) = section {
                window.focus(&section.read(cx).summary_focus_handle(cx), cx);
            }
        });
    })
    .detach();
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn entry(json: serde_json::Value) -> StackEntry {
        serde_json::from_value(json).expect("valid stack entry")
    }

    fn check(json: serde_json::Value) -> Check {
        serde_json::from_value(json).expect("valid check")
    }

    #[test]
    fn badge_follows_the_first_matching_rule() {
        let cases = [
            (
                serde_json::json!({ "state": "merged", "in_merge_queue": true }),
                "Merged",
                Tone::Merged,
            ),
            (
                serde_json::json!({ "state": "closed" }),
                "Closed",
                Tone::Neutral,
            ),
            (
                serde_json::json!({ "state": "draft", "mergeable": "CONFLICTING" }),
                "Draft",
                Tone::Neutral,
            ),
            (
                serde_json::json!({ "state": "open", "in_merge_queue": true, "mergeable": "CONFLICTING" }),
                "Queued",
                Tone::Queued,
            ),
            (
                serde_json::json!({ "state": "open", "merge_state_status": "UNSTABLE", "checks_status": "failure" }),
                "Ready",
                Tone::Ready,
            ),
            (
                serde_json::json!({ "state": "open", "merge_state_status": "DIRTY" }),
                "Conflicts",
                Tone::Blocked,
            ),
            (
                serde_json::json!({ "state": "open", "mergeable": "CONFLICTING", "merge_state_status": "BLOCKED" }),
                "Conflicts",
                Tone::Blocked,
            ),
            (
                serde_json::json!({ "state": "open", "merge_state_status": "BLOCKED", "checks_status": "failure" }),
                "Checks failed",
                Tone::Blocked,
            ),
            (
                serde_json::json!({ "state": "open", "review_decision": "CHANGES_REQUESTED", "checks_status": "pending" }),
                "Changes requested",
                Tone::Blocked,
            ),
            (
                serde_json::json!({ "state": "open", "review_decision": "REVIEW_REQUIRED", "checks_status": "pending" }),
                "Review needed",
                Tone::Pending,
            ),
            (
                serde_json::json!({ "state": "open", "merge_state_status": "BLOCKED", "checks_status": "pending" }),
                "Checks pending",
                Tone::Pending,
            ),
            (
                serde_json::json!({ "state": "open", "merge_state_status": "BEHIND" }),
                "Behind",
                Tone::Pending,
            ),
            (
                serde_json::json!({ "state": "open", "merge_state_status": "BLOCKED", "review_decision": "APPROVED" }),
                "Not ready",
                Tone::Pending,
            ),
            (
                serde_json::json!({ "state": "open", "review_decision": "APPROVED" }),
                "Approved",
                Tone::Ready,
            ),
            (
                serde_json::json!({ "state": "open", "merge_state_status": "UNKNOWN" }),
                "Open",
                Tone::Neutral,
            ),
        ];
        for (json, label, tone) in cases {
            assert_eq!(
                stack_entry_badge(&entry(json.clone())),
                (label, tone),
                "{json}"
            );
        }
    }

    #[test]
    fn checks_are_classified_by_status_then_conclusion() {
        let run = |status: &str, conclusion: Option<&str>| {
            check_outcome(&check(serde_json::json!({
                "name": "ci",
                "kind": { "type": "run", "status": status, "conclusion": conclusion },
            })))
        };
        assert_eq!(run("QUEUED", None), CheckOutcome::Queued);
        assert_eq!(run("IN_PROGRESS", None), CheckOutcome::InProgress);
        for status in ["WAITING", "REQUESTED", "PENDING", "SOMETHING_NEW"] {
            assert_eq!(run(status, None).bucket(), CheckBucket::Running, "{status}");
        }
        assert_eq!(run("COMPLETED", Some("SUCCESS")), CheckOutcome::Success);
        assert_eq!(
            run("COMPLETED", Some("SKIPPED")).bucket(),
            CheckBucket::Passing
        );
        assert_eq!(
            run("COMPLETED", Some("NEUTRAL")).bucket(),
            CheckBucket::Neutral
        );
        for conclusion in [
            "FAILURE",
            "CANCELLED",
            "TIMED_OUT",
            "ACTION_REQUIRED",
            "STARTUP_FAILURE",
        ] {
            assert_eq!(
                run("COMPLETED", Some(conclusion)).bucket(),
                CheckBucket::Failing,
                "{conclusion}"
            );
        }
        let status = |state: &str| {
            check_outcome(&check(serde_json::json!({
                "name": "ci",
                "kind": { "type": "status", "state": state },
            })))
        };
        assert_eq!(status("ERROR"), CheckOutcome::Error);
        assert_eq!(status("PENDING").bucket(), CheckBucket::Running);
        assert_eq!(status("EXPECTED").bucket(), CheckBucket::Running);
    }

    #[test]
    fn summary_and_order_put_failures_first() {
        let checks: Vec<Check> = [
            ("lint", "COMPLETED", Some("SUCCESS")),
            ("test", "IN_PROGRESS", None),
            ("build", "COMPLETED", Some("FAILURE")),
            ("docs", "COMPLETED", Some("NEUTRAL")),
        ]
        .into_iter()
        .map(|(name, status, conclusion)| {
            check(serde_json::json!({
                "name": name,
                "kind": { "type": "run", "status": status, "conclusion": conclusion },
            }))
        })
        .collect();
        assert_eq!(
            summarize_checks(&checks),
            CheckSummary {
                passing: 1,
                failing: 1,
                running: 1
            }
        );
        let names: Vec<&str> = sorted_checks(&checks)
            .iter()
            .map(|check| check.name.as_str())
            .collect();
        assert_eq!(names, ["build", "test", "docs", "lint"]);
    }

    #[test]
    fn durations_and_relative_times() {
        assert_eq!(format_duration(42), "42s");
        assert_eq!(format_duration(120), "2m");
        assert_eq!(format_duration(125), "2m 5s");
        assert_eq!(format_duration(3_900), "1h 5m");
        assert_eq!(format_relative(30), "just now");
        assert_eq!(format_relative(600), "10 min ago");
        assert_eq!(format_relative(7_300), "2h ago");
        let now = parse_timestamp("2026-11-15T12:00:00Z").expect("valid timestamp");
        let finished = check(serde_json::json!({
            "name": "build",
            "kind": { "type": "run", "status": "COMPLETED", "conclusion": "SUCCESS" },
            "started_at": "2026-11-15T11:50:00Z",
            "completed_at": "2026-11-15T11:53:20Z",
        }));
        assert_eq!(check_duration(&finished, now).as_deref(), Some("3m 20s"));
        let running = check(serde_json::json!({
            "name": "test",
            "kind": { "type": "run", "status": "IN_PROGRESS" },
            "started_at": "2026-11-15T11:59:15Z",
        }));
        assert_eq!(check_duration(&running, now).as_deref(), Some("45s"));
    }

    #[test]
    fn attention_lists_conflicts_failures_and_change_requests() {
        let pull_request: PullRequest = serde_json::from_value(serde_json::json!({
            "base_ref_name": "main",
            "mergeable": "CONFLICTING",
            "review_decision": "CHANGES_REQUESTED",
            "latest_reviews": [
                { "login": "ana", "state": "COMMENTED" },
                { "login": "ana", "state": "CHANGES_REQUESTED" },
                { "login": "bo", "state": "APPROVED" },
            ],
            "checks": [
                { "name": "build", "kind": { "type": "run", "status": "COMPLETED", "conclusion": "FAILURE" } },
            ],
        }))
        .expect("valid pull request");
        let titles: Vec<String> = attention_items(&pull_request)
            .into_iter()
            .map(|item| item.title)
            .collect();
        assert_eq!(
            titles,
            [
                "Conflicts block this PR",
                "1 check failed",
                "Changes requested"
            ]
        );
        let latest: Vec<(&str, ReviewState)> = latest_reviews(&pull_request)
            .into_iter()
            .map(|review| (review.login.as_str(), review.state))
            .collect();
        assert_eq!(
            latest,
            [
                ("ana", ReviewState::ChangesRequested),
                ("bo", ReviewState::Approved)
            ]
        );
    }

    #[test]
    fn stack_rows_run_from_the_top_and_fill_in_the_current_pr() {
        let status: PrStatus = serde_json::from_value(serde_json::json!({
            "pull_request": { "number": 11, "title": "Middle", "head_ref_name": "middle", "checks_rollup": "failure" },
            "known": true,
            "stack": {
                "number": 4, "position": 2, "size": 3, "base_ref": "main", "out_of_date": true,
                "entries": [
                    { "position": 1, "number": 10, "state": "merged" },
                    { "position": 3, "number": 12, "state": "open" },
                    { "position": 2, "number": 11, "state": "open", "merge_state_status": "BLOCKED" },
                ],
            },
        }))
        .expect("valid status");
        let (Some(stack), Some(pull_request)) = (status.stack(), status.pull_request.as_ref())
        else {
            panic!("fixture has a stack and a pull request");
        };
        let rows = stack_rows(stack, pull_request);
        let numbers: Vec<u64> = rows.iter().map(|row| row.number).collect();
        assert_eq!(numbers, [12, 11, 10]);
        assert_eq!(rows[1].title.as_deref(), Some("Middle"));
        assert_eq!(
            stack_entry_badge(&rows[1]),
            ("Checks failed", Tone::Blocked)
        );
    }

    #[test]
    fn avatar_fields_are_optional_and_the_author_fills_the_current_stack_row() {
        let status: PrStatus = serde_json::from_value(serde_json::json!({
            "pull_request": {
                "number": 11, "author": "maria", "author_avatar_url": "https://avatars.example/maria",
                "requested_reviewers": [
                    { "kind": "user", "login": "devon" },
                    { "kind": "team", "slug": "acme/payments", "avatar_url": "https://avatars.example/t" },
                ],
                "latest_reviews": [{ "login": "sam", "state": "APPROVED" }],
            },
            "known": true,
            "stack": { "number": 4, "position": 1, "size": 1, "base_ref": "main", "entries": [{ "position": 1, "number": 11 }] },
            "previews": [{ "app": "frontend", "state": "success", "url": "https://pr-11.example" }],
        }))
        .expect("valid status");
        let (Some(stack), Some(pull_request)) = (status.stack(), status.pull_request.as_ref())
        else {
            panic!("fixture has a stack and a pull request");
        };
        assert_eq!(
            pull_request.requested_reviewers[1],
            RequestedReviewer::Team {
                slug: "acme/payments".into(),
                avatar_url: Some("https://avatars.example/t".into()),
            }
        );
        assert_eq!(pull_request.latest_reviews[0].author_avatar_url, None);
        let rows = stack_rows(stack, pull_request);
        assert_eq!(rows[0].author.as_deref(), Some("maria"));
        assert_eq!(
            rows[0].author_avatar_url.as_deref(),
            Some("https://avatars.example/maria")
        );
        assert_eq!(status.previews.len(), 1);
    }

    #[test]
    fn avatar_fallbacks_use_the_first_letter() {
        assert_eq!(initials("octocat"), "O");
        assert_eq!(initials("acme/payments"), "P");
        assert_eq!(initials("-bot"), "B");
        assert_eq!(initials(""), "");
    }

    #[test]
    fn checks_fold_settled_ones_and_summarize_like_orca() {
        let mut checks: Vec<Check> = Vec::new();
        for (index, (status, conclusion)) in [
            ("COMPLETED", Some("SUCCESS")),
            ("COMPLETED", Some("SUCCESS")),
            ("COMPLETED", Some("SKIPPED")),
            ("IN_PROGRESS", None),
        ]
        .into_iter()
        .enumerate()
        {
            checks.push(check(serde_json::json!({
                "name": format!("check {index}"),
                "kind": { "type": "run", "status": status, "conclusion": conclusion },
            })));
        }
        let counts = CheckCounts::of(&checks);
        assert_eq!(
            counts.parts(),
            [
                ("1 pending".to_owned(), Color::Warning),
                ("2 passed".to_owned(), Color::Muted),
                ("1 skipped".to_owned(), Color::Muted),
            ]
        );
        assert!(!counts.all_green());
        assert_eq!(counts.settled(), 3);
        assert_eq!(counts.settled_label(), "2 passed · 1 skipped");
        assert!(CheckCounts::of(&checks[..3]).all_green());
    }

    #[test]
    fn pr_sections_follow_the_imported_orca_order() {
        let layout = Layout::from_settings(&serde_json::json!({
            "order": ["pr-attention", "pr-checks", "pr-pending-review", "pr-comments", "pr-resolved"],
            "hidden": [],
        }));
        assert_eq!(
            layout.visible::<PrSection>(),
            [
                PrSection::Attention,
                PrSection::Previews,
                PrSection::Checks,
                PrSection::PendingReview,
                PrSection::Comments,
                PrSection::Resolved,
            ],
            "Icarus's own sections slot in after their default neighbours"
        );
        assert!(
            PrSection::parse("pr-review").is_none(),
            "Review moved to Source Control, as in Orca"
        );
    }

    fn panel_fixture() -> serde_json::Value {
        serde_json::json!({
            "now": "2026-11-15T12:00:00Z",
            "current": "ws-1",
            "workspaces": [{ "id": "ws-1", "path": "/project", "branch": "feature" }],
            "pr_status": {
                "known": true,
                "pull_request": {
                    "number": 7, "title": "Feature", "base_ref_name": "main", "head_ref_name": "feature",
                    "checks": [
                        { "name": "lint", "kind": { "type": "run", "status": "COMPLETED", "conclusion": "SUCCESS" } },
                    ],
                },
            },
            "threads": [
                { "id": "T1", "path": "src/app.rs", "line": 2, "is_resolved": false, "viewer_can_reply": true,
                  "viewer_can_resolve": true, "placement": { "line": 2 },
                  "comments": [{ "id": "C1", "author": "ana", "body": "**Why** this?" },
                               { "id": "C2", "author": "ci-bot", "is_bot": true, "body": "Looks fine" }] },
                { "id": "T2", "path": "src/app.rs", "is_resolved": true, "placement": { "line": 1 },
                  "comments": [{ "id": "C3", "author": "bo", "body": "done" }] },
            ],
        })
    }

    async fn panel_in_workspace(
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<GitHubPanel>,
        std::sync::Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
        gpui::VisualTestContext,
    ) {
        let (workspace, mut cx) = crate::diff_tab::tests::git_workspace(cx).await;
        let calls =
            cx.update(|_, cx| crate::review::tests::stub_rpc(cx, |_, _| Ok(serde_json::json!({}))));
        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            let weak = workspace.weak_handle();
            let panel = cx.new(|cx| GitHubPanel::preview(weak, panel_fixture(), cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        (workspace, panel, calls, cx)
    }

    #[gpui::test]
    async fn sections_reorder_hide_and_collapse_and_the_layout_is_saved(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_workspace, panel, calls, mut cx) = panel_in_workspace(cx).await;
        let cx = &mut cx;
        panel.update(cx, |panel, cx| {
            assert!(
                panel.is_collapsed(PrSection::Checks),
                "all green starts collapsed"
            );
            assert!(panel.is_collapsed(PrSection::Resolved));
            assert!(!panel.is_collapsed(PrSection::Comments));
            panel.drop_section(PrSection::Comments, PrSection::Attention, cx);
            panel.toggle_section_hidden(PrSection::Previews, cx);
            panel.toggle_collapsed(PrSection::Checks, cx);
        });
        cx.run_until_parked();
        let saved = crate::review::tests::calls_to(&calls, "config.set");
        assert_eq!(
            saved.last(),
            Some(&serde_json::json!({
                "key": "sourceControl.sections.pr",
                "value": {
                    "order": ["pr-comments", "pr-attention", "pr-previews", "pr-checks",
                              "pr-pending-review", "pr-resolved"],
                    "hidden": ["pr-previews"],
                },
            }))
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.layout().visible::<PrSection>()[0],
                PrSection::Comments
            );
            assert!(
                !panel.is_collapsed(PrSection::Checks),
                "the toggle is remembered"
            );
        });

        panel.update(cx, |panel, cx| {
            panel.set_section_height(PrSection::Comments, Some(px(120.)), cx);
            panel.snapshot.as_mut().expect("fixture").current = None;
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.section_height(PrSection::Comments),
                None,
                "sizes are per workspace"
            );
            assert!(panel.is_collapsed(PrSection::Resolved));
        });
    }

    #[gpui::test]
    async fn threads_filter_by_audience_render_markdown_and_open_their_line(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, panel, calls, mut cx) = panel_in_workspace(cx).await;
        let cx = &mut cx;
        cx.run_until_parked();
        let threads = panel.read_with(cx, |panel, _| {
            panel.snapshot.as_ref().expect("fixture").threads.clone()
        });
        assert_eq!(
            pr_comments::Audience::ALL.map(|audience| pr_comments::audience_count(
                &threads[..1],
                &[],
                audience
            )),
            [2, 1, 1, 2]
        );
        assert!(!pr_comments::thread_in_audience(
            &threads[1],
            pr_comments::Audience::Unresolved
        ));
        panel.update(cx, |panel, cx| {
            assert_eq!(panel.audience(), pr_comments::Audience::Humans);
            panel.set_audience(pr_comments::Audience::Bots, cx);
            assert_eq!(panel.audience(), pr_comments::Audience::Bots);
        });
        panel.update_in(cx, |panel, window, cx| panel.sync_markdown(window, cx));
        panel.read_with(cx, |panel, cx| {
            let (body, markdown) = panel.markdown.get("C1").expect("rendered as Markdown");
            assert_eq!(body, "**Why** this?");
            assert_eq!(markdown.read(cx).source(), "**Why** this?");
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.open_location("src/app.rs", Some(2), window, cx)
        });
        cx.run_until_parked();
        let tab = workspace
            .read_with(cx, |workspace, cx| {
                workspace.items_of_type::<crate::DiffTab>(cx).next()
            })
            .expect("the diff tab opened");
        let row = tab.update(cx, |tab, cx| {
            tab.editor().update(cx, |editor, cx| {
                let snapshot = editor.display_snapshot(cx);
                let head = editor
                    .selections
                    .newest::<language::Point>(&snapshot)
                    .head();
                let buffer = editor.buffer().read(cx).snapshot(cx);
                buffer
                    .point_to_buffer_point(head)
                    .map(|(_, point)| point.row)
            })
        });
        assert_eq!(row, Some(1), "the cursor sits on line 2");

        panel.update(cx, |panel, cx| {
            panel.set_thread_resolved("T1".into(), true, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            crate::review::tests::calls_to(&calls, "review.resolve"),
            [serde_json::json!({ "thread": "T1", "workspace": "/project" })]
        );
    }

    #[gpui::test]
    async fn a_diff_comment_shows_in_the_diff_and_the_panel_before_github_answers(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::review::tests::{calls_to, hold_rpc, stub_rpc};
        use crate::review::{ComposerTarget, Destination};

        let (workspace, panel, _, mut cx) = panel_in_workspace(cx).await;
        let cx = &mut cx;
        let calls = cx.update(|_, cx| {
            stub_rpc(cx, |method, params| match method {
                "review.threads" => Ok(serde_json::json!({
                    "threads": panel_fixture()["threads"], "conversation": [],
                    "viewer": { "login": "me", "avatar_url": "https://avatars.example/me" },
                })),
                "review.comment.list" => Ok(serde_json::json!({ "drafts": [] })),
                "review.comment.post" if params["body"] == "fail" => {
                    Err(anyhow::anyhow!("GitHub refused the comment"))
                }
                "review.comment.post" => Ok(serde_json::json!({ "thread": {
                    "id": "PRRT_new", "path": "src/app.rs", "line": params["line"],
                    "comments": [{ "id": "PRRC_new", "author": "me", "body": params["body"],
                        "viewer_can_delete": true }],
                    "placement": { "line": params["line"], "label": "Line 3" },
                } })),
                _ => Ok(serde_json::json!({})),
            })
        });
        panel.update_in(cx, |panel, window, cx| {
            panel.open_location("src/app.rs", Some(1), window, cx)
        });
        cx.run_until_parked();
        let tab = workspace
            .read_with(cx, |workspace, cx| {
                workspace.items_of_type::<crate::DiffTab>(cx).next()
            })
            .expect("the diff tab opened");
        let review = tab.update(cx, |tab, cx| {
            tab.enable_review("/project".into(), "src/app.rs".into(), cx);
            tab.review().cloned().expect("review")
        });
        review.update(cx, |review, cx| review.reload(cx));
        cx.run_until_parked();
        let shown = |cx: &mut gpui::VisualTestContext| {
            let diff: Vec<(String, String)> = review.read_with(cx, |review, _| {
                review
                    .threads()
                    .iter()
                    .map(|t| (t.id.clone(), t.comments[0].body.clone()))
                    .collect()
            });
            let checks: Vec<(String, String)> = panel.read_with(cx, |panel, _| {
                panel
                    .snapshot
                    .as_ref()
                    .expect("snapshot")
                    .threads
                    .iter()
                    .map(|t| (t.id.clone(), t.comments[0].body.clone()))
                    .collect()
            });
            (diff, checks)
        };
        let comment = |line: u32, body: &str, cx: &mut gpui::VisualTestContext| {
            review.update_in(cx, |review, window, cx| {
                review.open_composer(
                    ComposerTarget::Lines {
                        start: line,
                        end: line,
                    },
                    body.into(),
                    window,
                    cx,
                );
                review.submit_composer(Destination::Now, window, cx);
            });
        };

        // GitHub is slow: the comment is in both views at once, written by the viewer.
        cx.update(|_, cx| hold_rpc(cx, &["review.comment.post"]));
        comment(1, "Pending one", cx);
        let (diff, checks) = shown(cx);
        assert_eq!(diff, checks, "one state for both views");
        assert!(
            diff.iter()
                .any(|(id, body)| id.starts_with("pending-") && body == "Pending one")
        );
        let author = review.read_with(cx, |review, _| {
            let thread = review
                .threads()
                .iter()
                .find(|t| t.id.starts_with("pending-"));
            thread.and_then(|t| t.comments[0].author.clone())
        });
        assert_eq!(author.as_deref(), Some("me"));

        // A refresh with the same data keeps every block in place.
        let blocks = review.read_with(cx, |review, _| review.block_ids().to_vec());
        review.update(cx, |review, cx| review.reload(cx));
        cx.run_until_parked();
        assert_eq!(
            review.read_with(cx, |review, _| review.block_ids().to_vec()),
            blocks,
            "no block is removed and put back: nothing flickers"
        );
        assert_eq!(
            shown(cx).0,
            diff,
            "the pending comment stays over the reload"
        );

        // GitHub answers: the server's thread replaces the pending one without a refetch.
        cx.update(|_, cx| hold_rpc(cx, &[]));
        let reads = calls_to(&calls, "review.threads").len();
        comment(3, "Looks off", cx);
        cx.run_until_parked();
        let (diff, checks) = shown(cx);
        assert_eq!(diff, checks);
        assert!(diff.contains(&("PRRT_new".to_owned(), "Looks off".to_owned())));

        // GitHub refuses: it disappears from both and the panel says why.
        comment(3, "fail", cx);
        assert!(shown(cx).1.iter().any(|(_, body)| body == "fail"));
        cx.run_until_parked();
        let (diff, checks) = shown(cx);
        assert!(!diff.iter().chain(&checks).any(|(_, body)| body == "fail"));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.action_error.clone()),
            Some("GitHub refused the comment".to_owned())
        );
        assert_eq!(
            calls_to(&calls, "review.threads").len(),
            reads,
            "no refetch after a change"
        );
    }

    #[gpui::test]
    async fn conversation_cards_react_reply_edit_and_delete_like_orca(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_workspace, panel, calls, mut cx) = panel_in_workspace(cx).await;
        let cx = &mut cx;
        cx.run_until_parked();
        let conversation = |panel: &GitHubPanel| {
            panel
                .snapshot
                .as_ref()
                .expect("fixture")
                .conversation
                .clone()
        };
        panel.update(cx, |panel, cx| {
            let conversation = serde_json::from_value(serde_json::json!([
                    { "id": "IC_1", "kind": "comment", "database_id": 5, "author": "me", "body": "old",
                      "viewer_can_update": true, "viewer_can_delete": true,
                      "reactions": [{ "content": "heart", "count": 1, "viewer_has_reacted": true }] },
                    { "id": "PRR_1", "kind": "review", "author": "carol", "review_state": "CHANGES_REQUESTED",
                      "body": "Needs tests", "viewer_can_update": true, "viewer_can_delete": true },
                    { "id": "IC_2", "kind": "comment", "database_id": 6, "author": "farm-bot", "body": "ci" },
                ]))
                .expect("conversation");
            panel.store.update(cx, |store, cx| {
                store.set_loaded("/project", None, Some(conversation), None, None, cx)
            });
        });
        panel.read_with(cx, |panel, _| {
            let all = conversation(panel);
            assert!(all[0].can_edit() && all[0].can_delete());
            assert!(
                all[1].can_edit() && !all[1].can_delete(),
                "a review can't be deleted"
            );
            assert!(!all[2].can_edit() && !all[2].can_delete());
        });

        panel.update(cx, |panel, cx| {
            panel.react("IC_1".into(), "heart".into(), true, cx);
            panel.react("PRR_1".into(), "+1".into(), false, cx);
        });
        panel.read_with(cx, |panel, _| {
            let all = conversation(panel);
            assert!(all[0].reactions.is_empty(), "removed at once");
            assert!(all[1].reactions[0].viewer_has_reacted);
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.start_reply("PRR_1".into(), Some("carol".into()), window, cx);
            let (_, editor) = panel.reply.clone().expect("reply box");
            editor.update(cx, |editor, cx| editor.set_text("Added them.", window, cx));
            panel.send_reply(cx);

            panel.start_edit("IC_1".into(), window, cx);
            let (_, editor) = panel.editing.clone().expect("edit box");
            editor.update(cx, |editor, cx| editor.set_text("new", window, cx));
            panel.save_edit(cx);
            panel.delete_comment("IC_2".into(), cx);
        });
        panel.read_with(cx, |panel, _| {
            let all = conversation(panel);
            let bodies: Vec<&str> = all.iter().map(|c| c.body.as_str()).collect();
            assert_eq!(bodies, ["new", "Needs tests", "@carol Added them."]);
            assert!(all[2].is_pending());
        });
        cx.run_until_parked();

        let reacted = crate::review::tests::calls_to(&calls, "review.react");
        assert_eq!(reacted[0]["remove"], true);
        assert_eq!(reacted[1]["subject"], "PRR_1");
        assert_eq!(
            crate::review::tests::calls_to(&calls, "review.conversation.post"),
            [
                serde_json::json!({ "body": "Added them.", "reply_to": "carol", "workspace": "/project" })
            ]
        );
        assert_eq!(
            crate::review::tests::calls_to(&calls, "review.comment.edit_published"),
            [
                serde_json::json!({ "kind": "issue_comment", "id": 5, "body": "new", "workspace": "/project" })
            ]
        );
        assert_eq!(
            crate::review::tests::calls_to(&calls, "review.comment.delete_published"),
            [serde_json::json!({ "kind": "issue_comment", "id": 6, "workspace": "/project" })]
        );
        panel.update(cx, |panel, cx| {
            panel.send_conversation_to_agent("PRR_1".into(), cx)
        });
        let attached = cx.update(|_, cx| crate::pr_store::local_attachment_texts(cx));
        assert!(
            attached.iter().any(
                |text| text.starts_with("@carol's review (changes requested):")
                    && text.ends_with("Needs tests")
            ),
            "attached at once from the shown comment: {attached:?}"
        );
        assert!(crate::review::tests::calls_to(&calls, "attach.pr_comments").is_empty());
    }

    /// The app's own wiring: a real agent tab, its composer in the footer, and the app's handler.
    pub(crate) fn real_agent_composer(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<agent_ui::agent_tab::AgentTab> {
        cx.update(|_, cx| {
            agent_ui::test_support::init_agent_tabs(
                fs::FakeFs::new(cx.background_executor().clone()),
                cx,
            );
            agent_ui::set_attachment_resolver(cx, crate::fetch_attachment_text);
            crate::set_composer_attach_handler(cx, |attachment, workspace, window, cx| {
                agent_ui::agent_tab::AgentComposer::attach_in_workspace(
                    attachment.id,
                    attachment.kind,
                    attachment.title,
                    workspace,
                    window,
                    cx,
                )
            });
        });
        let tab = workspace.update_in(cx, |workspace, window, cx| {
            agent_ui::test_support::open_stub_agent_tab(workspace, window, cx)
        });
        cx.run_until_parked();
        tab.expect("agent tab")
    }

    pub(crate) fn draft(
        tab: &Entity<agent_ui::agent_tab::AgentTab>,
        cx: &mut gpui::VisualTestContext,
    ) -> String {
        cx.update(|_, cx| agent_ui::test_support::draft_text(tab, cx))
            .expect("connected agent")
    }

    #[gpui::test]
    async fn send_to_agent_from_a_card_puts_a_chip_in_the_composer(cx: &mut gpui::TestAppContext) {
        let (workspace, panel, calls, mut cx) = panel_in_workspace(cx).await;
        let cx = &mut cx;
        let tab = real_agent_composer(&workspace, cx);
        panel.update(cx, |panel, cx| {
            let conversation: Vec<pr_comments::ConversationComment> =
                serde_json::from_value(serde_json::json!([
                    { "id": "IC_9", "kind": "comment", "database_id": 9, "author": "martin",
                      "body": "Please split this up", "url": "https://github.com/o/r/pull/7#issuecomment-9" },
                ]))
                .expect("conversation");
            let threads: Vec<ReviewThread> =
                serde_json::from_value(panel_fixture()["threads"].clone()).expect("threads");
            panel.store.update(cx, |store, cx| {
                store.set_loaded("/project", Some(threads), Some(conversation), None, None, cx)
            });
        });
        cx.run_until_parked();

        let menu_entry =
            |panel: &Entity<GitHubPanel>, conversation: bool, cx: &mut gpui::VisualTestContext| {
                panel.update(cx, |panel, cx| {
                    let send = if conversation {
                        panel.handler(
                            |panel, _, cx| panel.send_conversation_to_agent("IC_9".into(), cx),
                            cx,
                        )
                    } else {
                        panel.handler(
                            |panel, _, cx| panel.send_thread_to_agent("T1".into(), cx),
                            cx,
                        )
                    };
                    let comment = crate::review::ThreadComment {
                        id: "IC_9".into(),
                        ..Default::default()
                    };
                    let mut menu = panel.comment_menu(&comment, send, false, false, cx);
                    assert_eq!(menu[0].label, "Send to Agent");
                    menu.remove(0).handler
                })
            };
        // As the menu and the card's button do: from inside the window's own update.
        let send = menu_entry(&panel, true, cx);
        cx.update(|window, cx| send(window, cx));
        cx.run_until_parked();
        let text = draft(&tab, cx);
        assert!(
            text.contains("icarus://attachment/"),
            "the conversation comment lands as a chip: {text:?}"
        );

        let send = menu_entry(&panel, false, cx);
        cx.update(|window, cx| send(window, cx));
        cx.run_until_parked();
        let text = draft(&tab, cx);
        assert_eq!(
            text.matches("icarus://attachment/").count(),
            2,
            "the thread lands too: {text:?}"
        );
        assert!(
            workspace.read_with(cx, |workspace, _| workspace.notification_ids().is_empty()),
            "both chips resolved from the local text"
        );
        let attached = cx.update(|_, cx| crate::pr_store::local_attachment_texts(cx));
        assert!(
            attached
                .iter()
                .any(|text| text.contains("Please split this up"))
        );
        assert!(attached.iter().any(|text| text.starts_with("src/app.rs:2")));
        assert!(
            crate::review::tests::calls_to(&calls, "attach.pr_comments").is_empty(),
            "nothing asks the server"
        );
    }

    #[gpui::test]
    async fn a_send_to_agent_that_fails_says_so(cx: &mut gpui::TestAppContext) {
        let (workspace, panel, _calls, mut cx) = panel_in_workspace(cx).await;
        let cx = &mut cx;
        cx.update(|_, cx| {
            crate::set_composer_attach_handler(cx, |_, _, _, _| {
                Err(anyhow::anyhow!("open an agent tab first"))
            })
        });
        panel.update(cx, |panel, cx| {
            let threads: Vec<ReviewThread> =
                serde_json::from_value(panel_fixture()["threads"].clone()).expect("threads");
            panel.store.update(cx, |store, cx| {
                store.set_loaded("/project", Some(threads), None, None, None, cx)
            });
        });
        let toasts = |cx: &mut gpui::VisualTestContext| {
            workspace.update(cx, |workspace, cx| {
                let count = workspace.notification_ids().len();
                workspace.clear_all_notifications(cx);
                count
            })
        };
        for thread in ["T1", "missing"] {
            panel.update(cx, |panel, cx| {
                panel.send_thread_to_agent(thread.into(), cx)
            });
            cx.run_until_parked();
            assert_eq!(toasts(cx), 1, "sending {thread} shows why it failed");
        }
    }

    #[gpui::test]
    async fn the_description_loads_once_per_pr_update_and_folds(cx: &mut gpui::TestAppContext) {
        let (_workspace, panel, _calls, mut cx) = panel_in_workspace(cx).await;
        let cx = &mut cx;
        let calls = cx.update(|_, cx| {
            crate::review::tests::stub_rpc(cx, |method, _| {
                Ok(match method {
                    "pr.body" => serde_json::json!({ "number": 7, "body": "**Why** ![s](https://github.com/user-attachments/assets/a)" }),
                    _ => serde_json::json!({}),
                })
            })
        });
        panel.update(cx, |panel, cx| panel.load_description(cx));
        cx.run_until_parked();
        panel.update(cx, |panel, cx| {
            panel.load_description(cx);
            assert_eq!(
                panel.current_description(),
                Some("**Why** ![s](https://github.com/user-attachments/assets/a)")
            );
            assert!(
                panel.description_expanded.is_empty(),
                "the description starts folded"
            );
            panel.toggle_description(cx);
            assert_eq!(panel.description_expanded.len(), 1);
            assert!(panel.render_description(cx).is_some());
        });
        panel.update_in(cx, |panel, window, cx| panel.sync_markdown(window, cx));
        panel.read_with(cx, |panel, cx| {
            let (_, markdown) = panel.markdown.get(DESCRIPTION_KEY).expect("rendered");
            assert_eq!(
                markdown.read(cx).source(),
                "**Why**",
                "media is shown apart from the text"
            );
        });
        cx.run_until_parked();
        assert_eq!(
            crate::review::tests::calls_to(&calls, "pr.body").len(),
            1,
            "an unchanged PR isn't asked again"
        );
    }

    #[gpui::test]
    async fn panel_keys_focus_the_panel_and_hide_it_when_pressed_again(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, panel, _calls, mut cx) = panel_in_workspace(cx).await;
        let cx = &mut cx;
        workspace.update_in(cx, |workspace, window, cx| {
            toggle_panel::<GitHubPanel>(workspace, window, cx)
        });
        cx.run_until_parked();
        let focused = panel.update_in(cx, |panel, window, cx| {
            panel.focus_handle(cx).contains_focused(window, cx)
        });
        assert!(focused);
        workspace.update_in(cx, |workspace, window, cx| {
            toggle_panel::<GitHubPanel>(workspace, window, cx)
        });
        cx.run_until_parked();
        let open = workspace.read_with(cx, |workspace, cx| {
            workspace.right_dock().read(cx).is_open()
        });
        assert!(!open, "pressed again while focused, the panel hides");
    }

    #[test]
    fn preview_states_map_to_pills() {
        assert_eq!(preview_state_badge("success"), ("Ready", Tone::Ready));
        assert_eq!(
            preview_state_badge("in_progress"),
            ("Building", Tone::Pending)
        );
        assert_eq!(preview_state_badge("failure"), ("Failed", Tone::Blocked));
        assert_eq!(preview_state_badge("inactive"), ("Inactive", Tone::Neutral));
        assert_eq!(preview_state_badge("mystery"), ("Unknown", Tone::Neutral));
    }

    #[test]
    fn a_spent_quota_reads_as_a_retry_time() {
        let status: PrStatus = serde_json::from_value(serde_json::json!({
            "known": false, "error": "GitHub graphql rate limit nearly exhausted",
            "rate_limited_until": 1_763_400_000,
        }))
        .expect("status");
        let note = rate_limit_note(status.rate_limited_until.expect("until"));
        assert!(
            note.starts_with("GitHub rate limit, retrying at "),
            "{note}"
        );
        let time = note.rsplit(' ').next().unwrap_or_default();
        assert_eq!(time.len(), 5, "HH:MM, got {time}");
        assert_eq!(&time[2..3], ":");
    }
}
