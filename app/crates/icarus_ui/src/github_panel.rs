use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use gpui::{
    Action, App, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Hsla, Pixels, Render, Task, WeakEntity, Window, actions, px, rgb,
};
use gpui_tokio::Tokio;
use icarus_client::Client;
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use ui::{Avatar, CommonAnimationExt as _, Indicator, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::dock::{DockPosition, Panel, PanelEvent, PanelSizeState};
use workspace::{OpenOptions, Workspace};

use crate::links::{PreviewDeployment, ordered_previews};
use crate::review::{ReviewSection, ReviewView};
use crate::status_style;

actions!(
    icarus,
    [
        /// Shows or hides the Icarus GitHub panel.
        ToggleGitHubPanel,
        /// Reloads this workspace's pull request from the Icarus server.
        RefreshGitHubPanel,
        /// Reveals the GitHub panel and puts the cursor in the review summary.
        FocusReviewSummary,
    ]
);

const GITHUB_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

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

fn tone_color(tone: Tone, cx: &App) -> Option<Hsla> {
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
}

async fn fetch_snapshot(client: Arc<Client>, paths: Vec<PathBuf>) -> Result<Snapshot> {
    let workspaces: Vec<WorkspaceEntry> = serde_json::from_value(
        client
            .request("workspace.list", serde_json::json!({}))
            .await?,
    )?;
    let current = workspaces
        .iter()
        .find(|workspace| paths.contains(&workspace.path))
        .cloned();
    let status = match &current {
        Some(current) => Some(serde_json::from_value(
            client
                .request("pr.status", serde_json::json!({ "workspace": current.id }))
                .await?,
        )?),
        None => None,
    };
    Ok(Snapshot {
        workspaces,
        current,
        status,
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
    _poll: Task<()>,
}

impl GitHubPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        cx.spawn(async move |cx| cx.update(|_window, cx| cx.new(|cx| Self::new(workspace, cx))))
    }

    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let mut panel = Self::empty(workspace, cx);
        panel.animate = true;
        if !crate::enabled(cx) {
            return panel;
        }
        panel._poll = cx.spawn(async move |this, cx| {
            loop {
                let Ok(workspace) = this.read_with(cx, |panel, _| panel.workspace.clone()) else {
                    return;
                };
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
                let Ok(paths) = this.read_with(cx, |panel, cx| panel.project_paths(cx)) else {
                    return;
                };
                let snapshot = Tokio::spawn_result(cx, fetch_snapshot(client.clone(), paths)).await;
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
        Self {
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
            _poll: Task::ready(()),
        }
    }

    pub fn review_section(&self) -> Option<&Entity<ReviewSection>> {
        self.review.as_ref()
    }

    /// Keeps the Review section on the current workspace's PR, reloading it when the PR changes.
    fn sync_review_section(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                panel.snapshot = Some(Snapshot {
                    workspaces: fixture.workspaces,
                    current,
                    status: fixture.pr_status,
                });
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

    fn apply_snapshot(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        self.offline = None;
        self.refreshing = false;
        self.snapshot = Some(snapshot);
        cx.notify();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.refreshing = true;
        cx.notify();
        let paths = self.project_paths(cx);
        cx.spawn(async move |this, cx| {
            let snapshot = Tokio::spawn_result(cx, fetch_snapshot(client, paths)).await;
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
        window: &mut Window,
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
                let path = target.path.clone();
                self.workspace
                    .update(cx, |workspace, cx| {
                        workspace
                            .open_paths(vec![path], OpenOptions::default(), None, window, cx)
                            .detach();
                    })
                    .log_err();
            }
            _ => cx.open_url(&url),
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

    fn render_section_header(&self, title: &'static str) -> Div {
        h_flex().gap_1p5().child(
            Label::new(title)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .weight(gpui::FontWeight::SEMIBOLD),
        )
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
                        .on_click(cx.listener(move |panel, event: &ClickEvent, window, cx| {
                            let modifiers = event.modifiers();
                            let force_url =
                                modifiers.platform || modifiers.control || modifiers.shift;
                            panel.open_stack_entry(
                                number,
                                head_ref_name.clone(),
                                url.clone(),
                                force_url,
                                window,
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

    fn render_attention(&self, pull_request: &PullRequest) -> Option<impl IntoElement> {
        let items = attention_items(pull_request);
        if items.is_empty() {
            return None;
        }
        let mut section = v_flex()
            .gap_1()
            .child(self.render_section_header("Needs attention"));
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
        Some(section)
    }

    fn render_previews(
        &self,
        status: &PrStatus,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let previews = ordered_previews(&status.previews, status.primary_preview.as_ref());
        if previews.is_empty() {
            return None;
        }
        let colors = cx.theme().colors();
        let mut section = v_flex().gap_0p5().child(
            self.render_section_header("Previews")
                .child(div().flex_1())
                .child(
                    Label::new(previews.len().to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        );
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
        Some(section)
    }

    fn render_checks(&self, pull_request: &PullRequest) -> impl IntoElement {
        let summary = summarize_checks(&pull_request.checks);
        let now = self.now();
        let mut parts: Vec<(String, Color)> = Vec::new();
        if summary.passing > 0 {
            parts.push((format!("{} passing", summary.passing), Color::Muted));
        }
        if summary.failing > 0 {
            parts.push((format!("{} failing", summary.failing), Color::Error));
        }
        if summary.running > 0 {
            parts.push((format!("{} running", summary.running), Color::Warning));
        }
        let mut header = self.render_section_header("Checks").child(div().flex_1());
        for (index, (text, color)) in parts.into_iter().enumerate() {
            if index > 0 {
                header = header.child(Label::new("·").size(LabelSize::XSmall).color(Color::Muted));
            }
            header = header.child(Label::new(text).size(LabelSize::XSmall).color(color));
        }
        let mut section = v_flex().gap_0p5().child(header);
        if pull_request.checks.is_empty() {
            section = section.child(
                Label::new("No checks reported.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }
        for (index, check) in sorted_checks(&pull_request.checks).into_iter().enumerate() {
            let outcome = check_outcome(check);
            let conclusion_color = match outcome.bucket() {
                CheckBucket::Failing => Color::Error,
                CheckBucket::Running => Color::Warning,
                CheckBucket::Passing | CheckBucket::Neutral => Color::Muted,
            };
            section = section.child(
                h_flex()
                    .gap_1p5()
                    .py_0p5()
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
                                .color(Color::Muted),
                        )
                    })
                    .child(
                        Label::new(outcome.label())
                            .size(LabelSize::XSmall)
                            .color(conclusion_color),
                    )
                    .when_some(check.url.clone(), |row, url| {
                        row.child(
                            IconButton::new(
                                SharedString::from(format!("icarus-check-link-{index}")),
                                IconName::ArrowUpRight,
                            )
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Open check details"))
                            .on_click(move |_, _window, cx| cx.open_url(&url)),
                        )
                    }),
            );
        }
        section
    }

    fn render_reviews(
        &self,
        pull_request: &PullRequest,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let decision = pull_request
            .review_decision
            .and_then(|decision| match decision {
                ReviewDecision::Approved => Some(("Approved", Tone::Ready)),
                ReviewDecision::ChangesRequested => Some(("Changes requested", Tone::Blocked)),
                ReviewDecision::ReviewRequired => Some(("Review required", Tone::Pending)),
                ReviewDecision::Unknown => None,
            });
        let mut header = self.render_section_header("Reviews").child(div().flex_1());
        if let Some((label, tone)) = decision {
            let color = tone_color(tone, cx)
                .map(Color::Custom)
                .unwrap_or(Color::Muted);
            header = header.child(Label::new(label).size(LabelSize::XSmall).color(color));
        }
        let mut section = v_flex().gap_0p5().child(header);
        let reviews = latest_reviews(pull_request);
        if pull_request.requested_reviewers.is_empty() && reviews.is_empty() {
            section = section.child(
                Label::new("No reviews yet.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }
        for reviewer in &pull_request.requested_reviewers {
            let (avatar_kind, name, avatar_url) = match reviewer {
                RequestedReviewer::User { login, avatar_url } => {
                    (AvatarKind::Person, format!("@{login}"), avatar_url)
                }
                RequestedReviewer::Team { slug, avatar_url } => {
                    (AvatarKind::Team, slug.clone(), avatar_url)
                }
                RequestedReviewer::Other => continue,
            };
            section = section.child(
                h_flex()
                    .gap_1p5()
                    .py_0p5()
                    .child(render_avatar(
                        avatar_url.as_deref(),
                        &name,
                        avatar_kind,
                        px(16.),
                        cx,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(name).size(LabelSize::Small).truncate()),
                    )
                    .child(Indicator::dot().color(Color::Warning))
                    .child(
                        Label::new("Awaiting review")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            );
        }
        for review in reviews {
            let (icon, verdict, color) = match review.state {
                ReviewState::Approved => (IconName::Check, "Approved", Color::Success),
                ReviewState::ChangesRequested => {
                    (IconName::XCircle, "Changes requested", Color::Error)
                }
                ReviewState::Dismissed => (IconName::Dash, "Dismissed", Color::Muted),
                ReviewState::Commented | ReviewState::Pending | ReviewState::Unknown => {
                    (IconName::Chat, "Commented", Color::Muted)
                }
            };
            section = section.child(
                h_flex()
                    .gap_1p5()
                    .py_0p5()
                    .child(render_avatar(
                        review.author_avatar_url.as_deref(),
                        &review.login,
                        AvatarKind::Person,
                        px(16.),
                        cx,
                    ))
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(format!("@{}", review.login))
                                .size(LabelSize::Small)
                                .truncate(),
                        ),
                    )
                    .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                    .child(Label::new(verdict).size(LabelSize::XSmall).color(color)),
            );
        }
        section
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
            .gap_3()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(Label::new(pull_request.title.clone()))
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
            .when_some(status.stack(), |column, stack| {
                column.child(self.render_stack(stack, pull_request, cx))
            })
            .children(self.render_merge_queue(pull_request, cx))
            .children(self.render_attention(pull_request))
            .children(self.render_previews(status, cx))
            .child(self.render_checks(pull_request))
            .map(|column| match &self.review {
                Some(section) => column.child(section.clone()),
                None => column.child(self.render_reviews(pull_request, cx)),
            })
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
        match &status.pull_request {
            Some(pull_request) => self
                .render_pull_request(status, pull_request, cx)
                .into_any_element(),
            None if !status.known => self
                .render_message("Couldn't load the pull request.", status.error.clone())
                .into_any_element(),
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
        if crate::enabled(cx) || self.preview_review.is_some() {
            self.sync_review_section(window, cx);
        }
        v_flex()
            .key_context("IcarusGitHubPanel")
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
                    .p_2()
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
        Some(IconName::PullRequest)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Icarus GitHub")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        ToggleGitHubPanel.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        4
    }
}

pub(crate) fn register(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleGitHubPanel, window, cx| {
            workspace.toggle_panel_focus::<GitHubPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &FocusReviewSummary, window, cx| {
            workspace.open_panel::<GitHubPanel>(window, cx);
            let section = workspace
                .panel::<GitHubPanel>(cx)
                .and_then(|panel| panel.read(cx).review_section().cloned());
            if let Some(section) = section {
                window.focus(&section.read(cx).summary_focus_handle(cx), cx);
            }
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
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
}
