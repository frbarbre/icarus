//! GitHub review mode (github-review.md): the Review section in the GitHub panel, and draft comments,
//! inline PR threads and the comment composer inside diff tabs. Everything goes through `review.*` RPCs.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use collections::{HashMap, HashSet};
use editor::display_map::{
    BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId,
};
use editor::{Editor, EditorEvent};
use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    PathPromptOptions, Render, SharedString, Subscription, Task, WeakEntity, Window, actions, img,
    px,
};
use gpui_tokio::Tokio;
use icarus_client::Client;
use language::{Buffer, Point};
use serde::Deserialize;
use serde_json::{Value, json};
use ui::{ContextMenu, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;

use crate::AppConnection;
use crate::github_panel::{
    AvatarKind, PullRequest, RequestedReviewer, Review, ReviewState, render_avatar,
};

actions!(
    icarus,
    [
        /// Opens a review comment box on the selected lines of the diff tab (⌘⇧A).
        AddReviewComment,
        /// Sends the open review comment box (⌘Enter).
        SubmitReviewComment,
        /// Closes the open review comment box (Escape).
        CancelReviewComment,
        /// Requests the reviewers typed into the Review section.
        RequestTypedReviewers,
    ]
);

const THREAD_REFRESH_INTERVAL: Duration = Duration::from_secs(45);
const SUMMARY_SAVE_DELAY: Duration = Duration::from_millis(600);
/// GitHub's body cap is 65,536 characters; stay clear of it.
const MAX_BODY_CHARS: usize = 65_000;

// ---------------------------------------------------------------------------------------------
// Wire types (the shapes `review.get`, `review.threads` and `review.comment.list` return)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ReviewView {
    pub workspace: String,
    pub pr: ReviewPr,
    pub viewer: Option<String>,
    pub viewer_did_author: bool,
    pub standing: Option<String>,
    pub verdicts: Vec<String>,
    pub can_submit: HashMap<String, bool>,
    pub base: ReviewBaseView,
    pub drafts: Vec<ReviewDraft>,
    pub summary: String,
    pub verdict: Option<String>,
    pub context_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ReviewPr {
    pub repo: String,
    pub number: u64,
    pub url: Option<String>,
    pub base_ref_name: Option<String>,
    pub author: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ReviewBaseView {
    pub selected: Option<String>,
    pub default: Option<String>,
    pub options: Vec<BaseOption>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct BaseOption {
    pub value: String,
    pub label: String,
    pub short: Option<String>,
    pub headline: Option<String>,
    pub tags: Vec<String>,
    pub available: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ReviewDraft {
    pub id: String,
    pub path: String,
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub subject: String,
    pub body: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ReviewThread {
    pub id: String,
    pub path: String,
    pub line: Option<u32>,
    pub is_resolved: bool,
    pub viewer_can_resolve: bool,
    pub viewer_can_unresolve: bool,
    pub viewer_can_reply: bool,
    pub comments: Vec<ThreadComment>,
    pub placement: ThreadPlacement,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ThreadPlacement {
    pub file: bool,
    pub outdated: bool,
    pub line: Option<u32>,
    pub label: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ThreadComment {
    pub id: String,
    pub author: Option<String>,
    pub author_avatar_url: Option<String>,
    pub is_bot: bool,
    pub body: String,
    pub url: Option<String>,
    pub reactions: Vec<ReactionSummary>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ReactionSummary {
    pub content: String,
    pub count: u64,
    pub viewer_has_reacted: bool,
}

/// A note on lines of a workspace file (`note.list`): written by the user or by an agent.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct WorkspaceNote {
    pub id: String,
    pub path: String,
    pub line: u32,
    pub end_line: u32,
    /// `user` or `agent`.
    pub author: String,
    pub agent_id: Option<String>,
    pub body: String,
    pub resolved: bool,
    pub replies: Vec<NoteReply>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct NoteReply {
    pub id: String,
    /// `user`, `agent`, or `reply-agent` (the throwaway session that answers a user reply).
    pub author: String,
    pub body: String,
    /// For `reply-agent` entries: `pending`, `running`, `done` or `failed`.
    pub status: Option<String>,
    pub error: Option<String>,
    pub changed_files: Vec<String>,
}

impl NoteReply {
    pub fn is_thinking(&self) -> bool {
        matches!(self.status.as_deref(), Some("pending" | "running"))
    }

    pub fn is_failed(&self) -> bool {
        self.status.as_deref() == Some("failed")
    }
}

impl WorkspaceNote {
    pub fn by_agent(&self) -> bool {
        self.author == "agent"
    }

    /// The user reply a failed answer was for, so Retry can send it again.
    pub fn retry_target(&self, failed_reply: &str) -> Option<(&NoteReply, &NoteReply)> {
        let index = self
            .replies
            .iter()
            .position(|reply| reply.id == failed_reply)?;
        let failed = &self.replies[index];
        let asked = self.replies[..index]
            .iter()
            .rev()
            .find(|reply| reply.author == "user")?;
        Some((asked, failed))
    }
}

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewerState {
    Pending,
    ReRequested,
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
}

impl ReviewerState {
    fn label(self) -> &'static str {
        match self {
            Self::Pending => "Awaiting review",
            Self::ReRequested => "Asked to review again",
            Self::Approved => "Approved",
            Self::ChangesRequested => "Requested changes",
            Self::Commented => "Left comments",
            Self::Dismissed => "Review dismissed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewerRow {
    /// A login, or `org/team` for a team.
    pub name: String,
    pub team: bool,
    pub avatar_url: Option<String>,
    pub state: ReviewerState,
    pub can_rerequest: bool,
    pub can_remove: bool,
}

/// Each reviewer's verdict: the opinionated review when there is one, else their latest (a plain comment).
fn merged_reviews(pull_request: &PullRequest) -> Vec<Review> {
    let opinionated = pull_request
        .latest_opinionated_reviews
        .as_deref()
        .unwrap_or_default();
    let mut merged: Vec<Review> = Vec::new();
    for review in pull_request.latest_reviews.iter().chain(opinionated) {
        if review.state == ReviewState::Pending {
            continue;
        }
        let preferred = opinionated
            .iter()
            .rev()
            .find(|candidate| {
                candidate.login.eq_ignore_ascii_case(&review.login)
                    && candidate.state != ReviewState::Pending
            })
            .unwrap_or(review);
        if !merged
            .iter()
            .any(|existing| existing.login.eq_ignore_ascii_case(&review.login))
        {
            merged.push(preferred.clone());
        }
    }
    merged
}

/// github-review.md §6.1: open requests first (in GitHub's order), then everyone else who reviewed.
pub fn reviewer_rows(pull_request: &PullRequest) -> Vec<ReviewerRow> {
    let reviews = merged_reviews(pull_request);
    let reviewed = |login: &str| {
        reviews
            .iter()
            .any(|review| review.login.eq_ignore_ascii_case(login))
    };
    let mut rows = Vec::new();
    let mut seen: HashSet<String> = HashSet::default();
    for request in &pull_request.requested_reviewers {
        let row = match request {
            RequestedReviewer::User { login, avatar_url } => ReviewerRow {
                name: login.clone(),
                team: false,
                avatar_url: avatar_url.clone(),
                state: if reviewed(login) {
                    ReviewerState::ReRequested
                } else {
                    ReviewerState::Pending
                },
                can_rerequest: false,
                can_remove: !reviewed(login),
            },
            RequestedReviewer::Team { slug, avatar_url } => ReviewerRow {
                name: slug.clone(),
                team: true,
                avatar_url: avatar_url.clone(),
                state: ReviewerState::Pending,
                can_rerequest: false,
                can_remove: true,
            },
            RequestedReviewer::Other => continue,
        };
        seen.insert(row.name.to_lowercase());
        rows.push(row);
    }
    for review in &reviews {
        let author_is_viewer = pull_request
            .author
            .as_deref()
            .is_some_and(|author| author.eq_ignore_ascii_case(&review.login));
        if author_is_viewer || !seen.insert(review.login.to_lowercase()) {
            continue;
        }
        let state = match review.state {
            ReviewState::Approved => ReviewerState::Approved,
            ReviewState::ChangesRequested => ReviewerState::ChangesRequested,
            ReviewState::Commented => ReviewerState::Commented,
            ReviewState::Dismissed => ReviewerState::Dismissed,
            ReviewState::Pending | ReviewState::Unknown => continue,
        };
        rows.push(ReviewerRow {
            name: review.login.clone(),
            team: false,
            avatar_url: review.author_avatar_url.clone(),
            state,
            can_rerequest: true,
            can_remove: false,
        });
    }
    rows
}

/// The standing pill (§6 item 4): label and tone, or nothing for COMMENTED, DISMISSED and no review.
pub fn standing_pill(standing: Option<&str>) -> Option<(&'static str, Color, IconName)> {
    match standing? {
        "pending" => Some(("Your review is pending", Color::Warning, IconName::Pencil)),
        "changes-requested" => Some(("You requested changes", Color::Error, IconName::XCircle)),
        "approved" => Some(("You approved the changes", Color::Success, IconName::Check)),
        "requested" => Some(("Your review was requested", Color::Muted, IconName::Eye)),
        _ => None,
    }
}

pub fn verdict_label(verdict: &str) -> &'static str {
    match verdict {
        "approve" => "Approve",
        "request-changes" => "Request changes",
        _ => "Only comment",
    }
}

/// 1-based lines a comment covers, from a selection's start and end rows (0-based) and the end column;
/// a selection ending at column 0 leaves its last line out (§2.1).
pub fn comment_lines(start_row: u32, end_row: u32, end_column: u32) -> (u32, u32) {
    let (start_row, mut end_row) = if start_row <= end_row {
        (start_row, end_row)
    } else {
        (end_row, start_row)
    };
    if end_column == 0 && end_row > start_row {
        end_row -= 1;
    }
    (start_row + 1, end_row + 1)
}

pub fn lines_label(start: u32, end: u32) -> String {
    if start == end {
        format!("Line {end}")
    } else {
        format!("Lines {start}-{end}")
    }
}

/// Image URLs in a markdown body (`![alt](url)`), for previews.
pub fn markdown_images(body: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("![") {
        rest = &rest[start + 2..];
        let Some(close) = rest.find("](") else { break };
        rest = &rest[close + 2..];
        let Some(end) = rest.find(')') else { break };
        urls.push(rest[..end].trim().to_owned());
        rest = &rest[end + 1..];
    }
    urls
}

/// Text lines a body takes at roughly 90 characters a line, plus room for image previews.
fn body_lines(body: &str) -> u32 {
    let text: u32 = body
        .lines()
        .map(|line| (line.chars().count() as u32).div_ceil(90).max(1))
        .sum::<u32>()
        .max(1);
    text + markdown_images(body).len() as u32 * 6
}

/// Editor lines a thread card needs; a resolved thread starts as one collapsed row.
pub fn thread_height(thread: &ReviewThread, expanded: bool) -> u32 {
    if thread.is_resolved && !expanded {
        return 2;
    }
    let comments: u32 = thread
        .comments
        .iter()
        .map(|comment| 1 + body_lines(&comment.body) + u32::from(!comment.reactions.is_empty()))
        .sum();
    3 + comments
}

pub fn draft_height(draft: &ReviewDraft) -> u32 {
    3 + body_lines(&draft.body)
}

/// Editor lines a note card needs; a resolved note starts as one collapsed row.
pub fn note_height(note: &WorkspaceNote, expanded: bool) -> u32 {
    if note.resolved && !expanded {
        return 2;
    }
    let replies: u32 = note
        .replies
        .iter()
        .map(|reply| {
            let body = if reply.is_thinking() {
                1
            } else if reply.is_failed() {
                2
            } else {
                body_lines(&reply.body)
            };
            1 + body + u32::from(!reply.changed_files.is_empty())
        })
        .sum();
    3 + body_lines(&note.body) + replies
}

fn reaction_emoji(content: &str) -> &'static str {
    match content {
        "+1" => "👍",
        "-1" => "👎",
        "laugh" => "😄",
        "confused" => "😕",
        "heart" => "❤️",
        "hooray" => "🎉",
        "rocket" => "🚀",
        "eyes" => "👀",
        _ => "•",
    }
}

fn app_client(cx: &App) -> Option<Arc<Client>> {
    cx.try_global::<AppConnection>()
        .and_then(|connection| connection.client.clone())
}

/// Open review views, refreshed when the server announces `review.changed` (a CLI or agent edit).
#[derive(Default)]
struct ReviewListeners {
    diffs: Vec<WeakEntity<DiffReview>>,
    sections: Vec<WeakEntity<ReviewSection>>,
}

impl gpui::Global for ReviewListeners {}

/// Reloads every open review view; the server sends this after any review change.
pub fn review_changed(cx: &mut App) {
    let Some(listeners) = cx.try_global::<ReviewListeners>() else {
        return;
    };
    let diffs = listeners.diffs.clone();
    let sections = listeners.sections.clone();
    for diff in diffs {
        diff.update(cx, |review, cx| review.reload(cx)).ok();
    }
    for section in sections {
        section
            .update(cx, |section, cx| section.reload_soon(cx))
            .ok();
    }
    cx.update_global::<ReviewListeners, _>(|listeners, _| {
        listeners.diffs.retain(|diff| diff.upgrade().is_some());
        listeners
            .sections
            .retain(|section| section.upgrade().is_some());
    });
}

/// Answers review RPCs in tests, recording each call.
#[cfg(test)]
pub(crate) struct RpcStub(pub Arc<dyn Fn(&str, &Value) -> Result<Value> + Send + Sync>);

#[cfg(test)]
impl gpui::Global for RpcStub {}

pub(crate) fn request(
    client: Option<Arc<Client>>,
    method: &'static str,
    params: Value,
    cx: &mut App,
) -> Option<Task<Result<Value>>> {
    #[cfg(test)]
    if let Some(stub) = cx.try_global::<RpcStub>() {
        return Some(Task::ready((stub.0)(method, &params)));
    }
    let client = client?;
    Some(Tokio::spawn_result(cx, async move {
        client.request(method, params).await
    }))
}

// ---------------------------------------------------------------------------------------------
// The Review section in the GitHub panel
// ---------------------------------------------------------------------------------------------

pub struct ReviewSection {
    workspace: String,
    client: Option<Arc<Client>>,
    view: Option<ReviewView>,
    rows: Vec<ReviewerRow>,
    uncommitted: usize,
    /// The last action's failure, kept across the reload that follows it.
    error: Option<String>,
    load_error: Option<String>,
    busy: Option<String>,
    summary_editor: Entity<Editor>,
    reviewer_input: Entity<Editor>,
    applying_summary: bool,
    reload_requested: bool,
    _save_summary: Task<()>,
    _load: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ReviewSection {
    pub fn summary_focus_handle(&self, cx: &App) -> FocusHandle {
        self.summary_editor.focus_handle(cx)
    }

    /// Reloads on the next frame, which has the window a reload needs.
    pub fn reload_soon(&mut self, cx: &mut Context<Self>) {
        self.reload_requested = true;
        cx.notify();
    }

    pub fn new(
        workspace: String,
        client: Option<Arc<Client>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let summary_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 10, window, cx);
            editor.set_placeholder_text("Add review summary…", window, cx);
            editor.set_soft_wrap();
            editor
        });
        let reviewer_input = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Request a review: login or org/team", window, cx);
            editor
        });
        let subscriptions =
            vec![
                cx.subscribe(&summary_editor, |this, _, event: &EditorEvent, cx| {
                    if matches!(event, EditorEvent::BufferEdited) && !this.applying_summary {
                        this.schedule_summary_save(cx);
                    }
                }),
            ];
        let mut section = Self {
            workspace,
            client,
            view: None,
            rows: Vec::new(),
            uncommitted: 0,
            error: None,
            load_error: None,
            busy: None,
            summary_editor,
            reviewer_input,
            applying_summary: false,
            reload_requested: false,
            _save_summary: Task::ready(()),
            _load: Task::ready(()),
            _subscriptions: subscriptions,
        };
        let this = cx.entity().downgrade();
        cx.default_global::<ReviewListeners>().sections.push(this);
        section.reload(window, cx);
        section
    }

    /// A section that never talks to a server, for previews and visual tests.
    pub fn preview(
        view: ReviewView,
        pull_request: &PullRequest,
        uncommitted: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut section = Self::new(view.workspace.clone(), None, window, cx);
        section.set_pull_request(pull_request);
        section.uncommitted = uncommitted;
        section.apply_view(view, window, cx);
        section
    }

    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    pub fn view(&self) -> Option<&ReviewView> {
        self.view.as_ref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn set_pull_request(&mut self, pull_request: &PullRequest) {
        self.rows = reviewer_rows(pull_request);
    }

    fn apply_view(&mut self, view: ReviewView, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.summary_editor.read(cx).text(cx);
        if typed.trim().is_empty() && !view.summary.is_empty() {
            self.applying_summary = true;
            self.summary_editor.update(cx, |editor, cx| {
                editor.set_text(view.summary.clone(), window, cx)
            });
            self.applying_summary = false;
        }
        self.view = Some(view);
        cx.notify();
    }

    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let Some(get) = request(
            self.client.clone(),
            "review.get",
            json!({ "workspace": workspace }),
            cx,
        ) else {
            return;
        };
        let changes = request(
            self.client.clone(),
            "workspace.changes",
            json!({ "id": workspace, "scope": "uncommitted" }),
            cx,
        );
        self._load = cx.spawn_in(window, async move |this, cx| {
            let view = get.await;
            let uncommitted = match changes {
                Some(changes) => changes
                    .await
                    .ok()
                    .and_then(|listing| listing["rows"].as_array().map(Vec::len))
                    .unwrap_or(0),
                None => 0,
            };
            this.update_in(cx, |this, window, cx| {
                this.uncommitted = uncommitted;
                match view.and_then(|value| Ok(serde_json::from_value::<ReviewView>(value)?)) {
                    Ok(view) => {
                        this.load_error = None;
                        this.apply_view(view, window, cx);
                    }
                    Err(error) => {
                        this.load_error = Some(format!("{error:#}"));
                        cx.notify();
                    }
                }
            })
            .log_err();
        });
    }

    fn schedule_summary_save(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let workspace = self.workspace.clone();
        self._save_summary = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SUMMARY_SAVE_DELAY).await;
            let Ok(body) = this.read_with(cx, |this, cx| this.summary_editor.read(cx).text(cx))
            else {
                return;
            };
            let Some(save) = cx.update(|cx| {
                request(
                    client,
                    "review.set_summary",
                    json!({ "workspace": workspace, "body": body }),
                    cx,
                )
            }) else {
                return;
            };
            if let Err(error) = save.await {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("{error:#}"));
                    cx.notify();
                })
                .log_err();
            }
        });
    }

    /// Runs one review mutation, then reloads; GitHub's error text is kept verbatim.
    fn run(
        &mut self,
        label: impl Into<String>,
        method: &'static str,
        params: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
        after: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if self.busy.is_some() {
            return;
        }
        let Some(call) = request(self.client.clone(), method, params, cx) else {
            return;
        };
        self.busy = Some(label.into());
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = call.await;
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(_) => after(this, window, cx),
                    Err(error) => this.error = Some(format!("{error:#}")),
                }
                this.reload(window, cx);
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    pub fn submit(&mut self, verdict: &str, window: &mut Window, cx: &mut Context<Self>) {
        let body = self.summary_editor.read(cx).text(cx);
        let params = json!({ "workspace": self.workspace, "verdict": verdict, "body": body });
        self.run(
            verdict.to_owned(),
            "review.submit",
            params,
            window,
            cx,
            |this, window, cx| {
                this.applying_summary = true;
                this.summary_editor
                    .update(cx, |editor, cx| editor.set_text("", window, cx));
                this.applying_summary = false;
            },
        );
    }

    pub fn set_base(&mut self, value: String, window: &mut Window, cx: &mut Context<Self>) {
        let params = json!({ "workspace": self.workspace, "base": value });
        self.run("base", "review.set_base", params, window, cx, |_, _, _| {});
    }

    pub fn request_reviewers(
        &mut self,
        names: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let params = json!({ "workspace": self.workspace, "reviewers": names });
        self.run(
            "request",
            "review.request_reviewers",
            params,
            window,
            cx,
            |this, window, cx| {
                this.reviewer_input
                    .update(cx, |editor, cx| editor.set_text("", window, cx));
            },
        );
    }

    pub fn remove_reviewer(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let params = json!({ "workspace": self.workspace, "reviewers": [name] });
        self.run(
            "remove",
            "review.remove_reviewers",
            params,
            window,
            cx,
            |_, _, _| {},
        );
    }

    fn request_typed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.reviewer_input.read(cx).text(cx);
        if typed.trim().is_empty() {
            return;
        }
        self.request_reviewers(vec![typed], window, cx);
    }

    fn render_reviewer(&self, row: &ReviewerRow, cx: &mut Context<Self>) -> impl IntoElement {
        let (dot, icon, color) = match row.state {
            ReviewerState::Pending | ReviewerState::ReRequested => {
                (true, IconName::Circle, Color::Warning)
            }
            ReviewerState::Approved => (false, IconName::Check, Color::Success),
            ReviewerState::ChangesRequested => (false, IconName::Diff, Color::Error),
            ReviewerState::Commented => (false, IconName::Chat, Color::Muted),
            ReviewerState::Dismissed => (false, IconName::Close, Color::Muted),
        };
        let display = if row.team {
            row.name.clone()
        } else {
            format!("@{}", row.name)
        };
        let blocked = self.uncommitted > 0;
        let rerequest_tooltip: SharedString = if blocked {
            format!(
                "Commit your changes before asking for another review ({} uncommitted)",
                self.uncommitted
            )
            .into()
        } else {
            "Re-request review".into()
        };
        let name = row.name.clone();
        let remove_name = row.name.clone();
        let state_label = row.state.label();
        h_flex()
            .h_7()
            .gap_1p5()
            .child(render_avatar(
                row.avatar_url.as_deref(),
                &row.name,
                if row.team {
                    AvatarKind::Team
                } else {
                    AvatarKind::Person
                },
                px(16.),
                cx,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(display).size(LabelSize::Small).truncate()),
            )
            .when(row.can_rerequest, |row_el| {
                row_el.child(
                    IconButton::new(
                        SharedString::from(format!("rerequest-{name}")),
                        IconName::RotateCw,
                    )
                    .icon_size(IconSize::XSmall)
                    .disabled(blocked || self.busy.is_some())
                    .tooltip(Tooltip::text(rerequest_tooltip))
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| {
                            this.request_reviewers(vec![name.clone()], window, cx)
                        },
                    )),
                )
            })
            .when(row.can_remove, |row_el| {
                row_el.child(
                    IconButton::new(
                        SharedString::from(format!("remove-{remove_name}")),
                        IconName::Close,
                    )
                    .icon_size(IconSize::XSmall)
                    .disabled(self.busy.is_some())
                    .tooltip(Tooltip::text(format!("Remove {remove_name}")))
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| {
                            this.remove_reviewer(remove_name.clone(), window, cx)
                        },
                    )),
                )
            })
            .child(
                div()
                    .id(SharedString::from(format!("state-{}", row.name)))
                    .tooltip(Tooltip::text(state_label))
                    .child(if dot {
                        ui::Indicator::dot().color(color).into_any_element()
                    } else {
                        Icon::new(icon)
                            .size(IconSize::XSmall)
                            .color(color)
                            .into_any_element()
                    }),
            )
    }

    fn render_base_picker(&self, view: &ReviewView, cx: &mut Context<Self>) -> Option<AnyElement> {
        view.pr.base_ref_name.as_ref()?;
        let selected = view
            .base
            .selected
            .clone()
            .or_else(|| view.base.default.clone())
            .unwrap_or_else(|| "whole".to_owned());
        let current = view
            .base
            .options
            .iter()
            .find(|option| option.value == selected)
            .map(|option| option.label.clone())
            .unwrap_or_else(|| selected.clone());
        let options = view.base.options.clone();
        let this = cx.entity().downgrade();
        Some(
            v_flex()
                .gap_0p5()
                .child(
                    PopoverMenu::new("icarus-review-base")
                        .full_width(true)
                        .trigger(
                            Button::new("icarus-review-base-trigger", current)
                                .full_width()
                                .style(ButtonStyle::Outlined)
                                .label_size(LabelSize::Small)
                                .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                                .tooltip(Tooltip::text("What the diff compares against")),
                        )
                        .menu(move |window, cx| {
                            let options = options.clone();
                            let this = this.clone();
                            let selected = selected.clone();
                            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                for (index, option) in options.iter().enumerate() {
                                    let first_commit = index == 1;
                                    if first_commit || option.value == "whole" {
                                        menu = menu.separator();
                                    }
                                    let mut label = option.label.clone();
                                    if !option.tags.is_empty() {
                                        label = format!("{label}  ·  {}", option.tags.join(", "));
                                    }
                                    let value = option.value.clone();
                                    let this = this.clone();
                                    menu = menu.toggleable_entry_disabled_when(
                                        label,
                                        option.value == selected,
                                        !option.available,
                                        IconPosition::Start,
                                        None,
                                        move |window, cx| {
                                            this.update(cx, |this, cx| {
                                                this.set_base(value.clone(), window, cx)
                                            })
                                            .log_err();
                                        },
                                    );
                                }
                                menu
                            }))
                        }),
                )
                .when_some(view.base.error.clone(), |column, error| {
                    column.child(
                        Label::new(error)
                            .size(LabelSize::XSmall)
                            .color(Color::Error),
                    )
                })
                .into_any_element(),
        )
    }
}

impl Render for ReviewSection {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.reload_requested) {
            self.reload(window, cx);
        }
        let mut section = v_flex()
            .key_context("IcarusReviewSection")
            .gap_1p5()
            .on_action(cx.listener(|this, _: &RequestTypedReviewers, window, cx| {
                this.request_typed(window, cx)
            }))
            .child(
                h_flex().child(
                    Label::new("REVIEW")
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .weight(gpui::FontWeight::SEMIBOLD),
                ),
            );
        for row in self.rows.clone() {
            section = section.child(self.render_reviewer(&row, cx));
        }
        section = section.child(
            div()
                .key_context("IcarusReviewerInput")
                .px_1()
                .py_0p5()
                .border_1()
                .rounded_sm()
                .border_color(cx.theme().colors().border_variant)
                .child(self.reviewer_input.clone()),
        );
        let Some(view) = self.view.clone() else {
            let problem = self.load_error.clone().or_else(|| self.error.clone());
            return section.child(
                Label::new(
                    problem
                        .clone()
                        .unwrap_or_else(|| "Loading review…".to_owned()),
                )
                .size(LabelSize::XSmall)
                .color(if problem.is_some() {
                    Color::Error
                } else {
                    Color::Muted
                }),
            );
        };
        if let Some(picker) = self.render_base_picker(&view, cx) {
            section = section.child(picker);
        }
        if let Some((label, color, icon)) = standing_pill(view.standing.as_deref()) {
            section = section.child(
                h_flex()
                    .gap_1()
                    .px_1p5()
                    .py_0p5()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                    .child(Label::new(label).size(LabelSize::XSmall).color(color)),
            );
        }
        section = section.child(
            div()
                .p_1()
                .border_1()
                .rounded_sm()
                .border_color(cx.theme().colors().border_variant)
                .child(self.summary_editor.clone()),
        );
        let drafts = view.drafts.len();
        let mut buttons = h_flex().gap_1().justify_end().flex_wrap();
        if drafts > 0 {
            buttons = buttons.child(
                Label::new(format!(
                    "{drafts} comment{} queued",
                    if drafts == 1 { "" } else { "s" }
                ))
                .size(LabelSize::XSmall)
                .color(Color::Warning),
            );
        }
        for verdict in view.verdicts.clone() {
            let enabled = view.can_submit.get(&verdict).copied().unwrap_or(false);
            let sending = self.busy.as_deref() == Some(verdict.as_str());
            let (icon, color) = match verdict.as_str() {
                "approve" => (IconName::Check, Color::Success),
                "request-changes" => (IconName::Close, Color::Error),
                _ => (IconName::Chat, Color::Muted),
            };
            let label = if sending {
                "Sending…"
            } else {
                verdict_label(&verdict)
            };
            buttons = buttons.child(
                Button::new(SharedString::from(format!("verdict-{verdict}")), label)
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(icon).size(IconSize::XSmall).color(color))
                    .disabled(!enabled || self.busy.is_some())
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.submit(&verdict, window, cx)
                    })),
            );
        }
        section = section.child(buttons);
        for error in [self.error.clone(), self.load_error.clone()]
            .into_iter()
            .flatten()
        {
            section = section.child(
                Label::new(error)
                    .size(LabelSize::XSmall)
                    .color(Color::Error),
            );
        }
        section
    }
}

// ---------------------------------------------------------------------------------------------
// Review comments in a diff tab
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ComposerTarget {
    /// A new comment on lines `start..=end`.
    Lines {
        start: u32,
        end: u32,
    },
    EditDraft {
        id: String,
    },
    Reply {
        thread: String,
    },
    /// A reply in a note thread; the server answers it with a throwaway agent session.
    NoteReply {
        note: String,
    },
    /// The note's own text, or one of its replies.
    EditNote {
        note: String,
        reply: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    /// A queued draft for the next review.
    Review,
    /// A note on these lines (`note.add`), which the agent side of Icarus reads and answers.
    Agent,
}

/// Asks the diff tab to open another file's diff, e.g. one a note's answer changed.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenChangedFile(pub String);

/// Draft comments, PR threads and the open composer for one diff tab's file, drawn as editor blocks.
pub struct DiffReview {
    editor: WeakEntity<Editor>,
    buffer: Entity<Buffer>,
    /// The worktree root, which the server resolves to its workspace.
    workspace_key: String,
    /// The file relative to the repository root.
    path: String,
    client: Option<Arc<Client>>,
    threads: Vec<ReviewThread>,
    drafts: Vec<ReviewDraft>,
    notes: Vec<WorkspaceNote>,
    expanded: HashSet<String>,
    composer: Option<(Entity<CommentComposer>, CustomBlockId)>,
    blocks: Vec<CustomBlockId>,
    /// The last action's failure, kept across the reload that follows it.
    error: Option<String>,
    load_error: Option<String>,
    _poll: Task<()>,
}

impl DiffReview {
    pub fn new(
        editor: WeakEntity<Editor>,
        buffer: Entity<Buffer>,
        workspace_key: String,
        path: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let client = app_client(cx);
        let mut review = Self {
            editor,
            buffer,
            workspace_key,
            path,
            client,
            threads: Vec::new(),
            drafts: Vec::new(),
            notes: Vec::new(),
            expanded: HashSet::default(),
            composer: None,
            blocks: Vec::new(),
            error: None,
            load_error: None,
            _poll: Task::ready(()),
        };
        let this = cx.entity().downgrade();
        cx.default_global::<ReviewListeners>().diffs.push(this);
        if review.client.is_some() {
            review._poll = cx.spawn(async move |this, cx| {
                loop {
                    let Ok(load) = this.update(cx, |this, cx| this.load(cx)) else {
                        return;
                    };
                    load.await;
                    cx.background_executor()
                        .timer(THREAD_REFRESH_INTERVAL)
                        .await;
                }
            });
        }
        review
    }

    pub fn threads(&self) -> &[ReviewThread] {
        &self.threads
    }

    pub fn drafts(&self) -> &[ReviewDraft] {
        &self.drafts
    }

    pub fn notes(&self) -> &[WorkspaceNote] {
        &self.notes
    }

    /// Shows fixed notes without a server (previews, visual tests).
    pub fn set_preview_notes(&mut self, notes: Vec<WorkspaceNote>, cx: &mut Context<Self>) {
        self.notes = notes;
        self.rebuild_blocks(cx);
    }

    pub fn composer(&self) -> Option<&Entity<CommentComposer>> {
        self.composer.as_ref().map(|(composer, _)| composer)
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref().or(self.load_error.as_deref())
    }

    /// Shows fixed threads and drafts without a server (previews, visual tests).
    pub fn set_preview(
        &mut self,
        threads: Vec<ReviewThread>,
        drafts: Vec<ReviewDraft>,
        cx: &mut Context<Self>,
    ) {
        self.threads = threads;
        self.drafts = drafts;
        self.rebuild_blocks(cx);
    }

    fn load(&mut self, cx: &mut Context<Self>) -> Task<()> {
        // Notes don't need a pull request, so they load on their own and survive a review failure.
        let notes = request(
            self.client.clone(),
            "note.list",
            json!({ "cwd": self.workspace_key, "path": self.path, "include_resolved": true }),
            cx,
        );
        let (Some(threads), Some(drafts)) = (
            request(
                self.client.clone(),
                "review.threads",
                json!({ "workspace": self.workspace_key, "path": self.path }),
                cx,
            ),
            request(
                self.client.clone(),
                "review.comment.list",
                json!({ "workspace": self.workspace_key }),
                cx,
            ),
        ) else {
            return Task::ready(());
        };
        cx.spawn(async move |this, cx| {
            let threads = threads.await;
            let drafts = drafts.await;
            let notes = match notes {
                Some(notes) => Some(notes.await),
                None => None,
            };
            this.update(cx, |this, cx| {
                match notes {
                    Some(Ok(notes)) => {
                        this.notes = serde_json::from_value(notes).unwrap_or_default();
                    }
                    Some(Err(error)) => log::warn!("loading notes failed: {error:#}"),
                    None => {}
                }
                match (threads, drafts) {
                    (Ok(threads), Ok(drafts)) => {
                        this.load_error = None;
                        this.threads =
                            serde_json::from_value(threads["threads"].clone()).unwrap_or_default();
                        let drafts: Vec<ReviewDraft> =
                            serde_json::from_value(drafts["drafts"].clone()).unwrap_or_default();
                        this.drafts = drafts
                            .into_iter()
                            .filter(|draft| draft.path == this.path)
                            .collect();
                    }
                    // A branch without a PR, or a repo not on GitHub, is normal: notes still work.
                    (Err(error), _) | (_, Err(error)) => {
                        let text = format!("{error:#}");
                        this.load_error = (!text.contains("no_pr") && !text.contains("no_github"))
                            .then_some(text);
                        this.threads.clear();
                        this.drafts.clear();
                    }
                }
                this.rebuild_blocks(cx);
            })
            .log_err();
        })
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load(cx).detach();
    }

    /// Where a 1-based line's block goes; lines past the end stack after the last line.
    fn anchor_for_line(&self, line: Option<u32>, cx: &App) -> Option<editor::Anchor> {
        let editor = self.editor.upgrade()?;
        let buffer = self.buffer.read(cx);
        let last_row = buffer.max_point().row;
        let row = line
            .map(|line| line.saturating_sub(1))
            .filter(|row| *row <= last_row)
            .unwrap_or(last_row);
        let text_anchor = buffer.anchor_after(Point::new(row, 0));
        editor
            .read(cx)
            .buffer()
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(text_anchor)
    }

    fn rebuild_blocks(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let this = cx.entity().downgrade();
        let mut cards: Vec<(Option<u32>, usize, u32, AnyCard)> = Vec::new();
        for thread in &self.threads {
            let line = (!thread.placement.outdated && !thread.placement.file)
                .then_some(thread.placement.line)
                .flatten();
            let order = if line.is_some() { 0 } else { 1_000_000 };
            let expanded = self.expanded.contains(&thread.id);
            cards.push((
                line,
                order,
                thread_height(thread, expanded),
                AnyCard::Thread(thread.clone(), expanded),
            ));
        }
        for draft in &self.drafts {
            let line = (draft.subject != "file").then_some(draft.line).flatten();
            let order = if line.is_some() { 1 } else { 1_000_001 };
            cards.push((
                line,
                order,
                draft_height(draft),
                AnyCard::Draft(draft.clone()),
            ));
        }
        for note in &self.notes {
            let expanded = self.expanded.contains(&note.id);
            cards.push((
                Some(note.end_line.max(note.line)),
                2,
                note_height(note, expanded),
                AnyCard::Note(note.clone(), expanded),
            ));
        }
        cards.sort_by_key(|(line, order, _, _)| (line.is_none(), *line, *order));
        let mut blocks = Vec::new();
        for (line, _, height, card) in cards {
            let Some(anchor) = self.anchor_for_line(line, cx) else {
                continue;
            };
            let card = cx.new(|_| ReviewCard {
                card,
                review: this.clone(),
            });
            blocks.push(BlockProperties {
                placement: BlockPlacement::Below(anchor),
                height: Some(height),
                style: BlockStyle::Flex,
                render: card_renderer(card),
                priority: 1,
            });
        }
        let old: HashSet<CustomBlockId> = self.blocks.drain(..).collect();
        let ids = editor.update(cx, |editor, cx| {
            editor.remove_blocks(old, None, cx);
            editor.insert_blocks(blocks, None, cx)
        });
        self.blocks = ids;
        cx.notify();
    }

    /// Opens the composer on the editor's selection (§2.1).
    pub fn open_on_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        if self.composer.is_some() {
            return;
        }
        let lines = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let selection = editor.selections.newest::<Point>(&snapshot);
            let buffer = snapshot.buffer_snapshot();
            let (_, start) = buffer.point_to_buffer_point(selection.start)?;
            let (_, end) = buffer.point_to_buffer_point(selection.end)?;
            Some(comment_lines(start.row, end.row, end.column))
        });
        let Some((start, end)) = lines else {
            return;
        };
        self.open_composer(
            ComposerTarget::Lines { start, end },
            String::new(),
            window,
            cx,
        );
    }

    pub fn open_composer(
        &mut self,
        target: ComposerTarget,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_composer(cx);
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let line = match &target {
            ComposerTarget::Lines { end, .. } => Some(*end),
            ComposerTarget::EditDraft { id } => self
                .drafts
                .iter()
                .find(|draft| &draft.id == id)
                .and_then(|draft| draft.line),
            ComposerTarget::Reply { thread } => self
                .threads
                .iter()
                .find(|candidate| &candidate.id == thread)
                .and_then(|thread| thread.placement.line),
            ComposerTarget::NoteReply { note } | ComposerTarget::EditNote { note, .. } => self
                .notes
                .iter()
                .find(|candidate| &candidate.id == note)
                .map(|note| note.end_line.max(note.line)),
        };
        let Some(anchor) = self.anchor_for_line(line, cx) else {
            return;
        };
        let review = cx.entity().downgrade();
        let composer = cx.new(|cx| CommentComposer::new(target, text, review, window, cx));
        let height = composer.read(cx).height(cx);
        let ids = editor.update(cx, |editor, cx| {
            editor.insert_blocks(
                [BlockProperties {
                    placement: BlockPlacement::Below(anchor),
                    height: Some(height),
                    style: BlockStyle::Flex,
                    render: composer_renderer(composer.clone()),
                    // Sorts before the saved threads on the same line (§2.3).
                    priority: 0,
                }],
                None,
                cx,
            )
        });
        if let Some(id) = ids.first() {
            self.composer = Some((composer.clone(), *id));
        }
        let focus = composer.read(cx).editor.focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// The composer passes its own height: it is mid-update when its text changes.
    fn resize_composer(&mut self, height: u32, cx: &mut Context<Self>) {
        let (Some((_, id)), Some(editor)) = (&self.composer, self.editor.upgrade()) else {
            return;
        };
        let id = *id;
        editor.update(cx, |editor, cx| {
            editor.resize_blocks(HashMap::from_iter([(id, height)]), None, cx)
        });
    }

    pub fn close_composer(&mut self, cx: &mut Context<Self>) {
        if let (Some((_, id)), Some(editor)) = (self.composer.take(), self.editor.upgrade()) {
            editor.update(cx, |editor, cx| {
                editor.remove_blocks(HashSet::from_iter([id]), None, cx)
            });
        }
        cx.notify();
    }

    fn call(
        &mut self,
        method: &'static str,
        params: Value,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<Value>>> {
        request(self.client.clone(), method, params, cx)
    }

    /// Sends the composer: a queued draft, a draft edit, a reply, or a note for the agent.
    pub fn submit_composer(
        &mut self,
        destination: Destination,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((composer, _)) = self.composer.clone() else {
            return;
        };
        let (target, body) = {
            let composer = composer.read(cx);
            (composer.target.clone(), composer.editor.read(cx).text(cx))
        };
        let body = body.trim().to_owned();
        if body.is_empty() {
            return;
        }
        if body.chars().count() > MAX_BODY_CHARS {
            composer.update(cx, |composer, cx| {
                composer.error = Some("Comment is too large to submit safely.".into());
                cx.notify();
            });
            return;
        }
        let workspace = self.workspace_key.clone();
        let (method, params) = match (&target, destination) {
            (ComposerTarget::Lines { start, end }, Destination::Agent) => (
                "note.add",
                json!({
                    "cwd": workspace, "path": self.path, "line": start, "end_line": end, "body": body,
                }),
            ),
            (ComposerTarget::NoteReply { note }, _) => {
                ("note.reply", json!({ "id": note, "body": body }))
            }
            (ComposerTarget::EditNote { note, reply }, _) => (
                "note.edit",
                json!({ "id": note, "reply_id": reply, "body": body }),
            ),
            (ComposerTarget::Lines { start, end }, Destination::Review) => (
                "review.comment.add",
                json!({
                    "workspace": workspace, "path": self.path, "line": end,
                    "start_line": (start != end).then_some(*start), "body": body,
                }),
            ),
            (ComposerTarget::EditDraft { id }, _) => (
                "review.comment.edit",
                json!({ "workspace": workspace, "id": id, "body": body }),
            ),
            (ComposerTarget::Reply { thread }, _) => (
                "review.reply",
                json!({ "workspace": workspace, "thread": thread, "body": body }),
            ),
        };
        let Some(call) = self.call(method, params, cx) else {
            return;
        };
        composer.update(cx, |composer, cx| {
            composer.busy = true;
            composer.error = None;
            cx.notify();
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = call.await;
            this.update_in(cx, |this, _window, cx| match result {
                Ok(_) => {
                    this.close_composer(cx);
                    this.reload(cx);
                }
                Err(error) => composer.update(cx, |composer, cx| {
                    composer.busy = false;
                    composer.error = Some(format!("{error:#}"));
                    cx.notify();
                }),
            })
            .log_err();
        })
        .detach();
    }

    fn mutate(&mut self, method: &'static str, params: Value, cx: &mut Context<Self>) {
        let Some(call) = self.call(method, params, cx) else {
            return;
        };
        self.error = None;
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.error = Some(format!("{error:#}"));
                }
                this.reload(cx);
            })
            .log_err();
        })
        .detach();
    }

    pub fn remove_draft(&mut self, id: &str, cx: &mut Context<Self>) {
        let params = json!({ "workspace": self.workspace_key, "id": id });
        self.mutate("review.comment.remove", params, cx);
    }

    pub fn set_resolved(&mut self, thread: &str, resolved: bool, cx: &mut Context<Self>) {
        let method = if resolved {
            "review.resolve"
        } else {
            "review.unresolve"
        };
        let params = json!({ "workspace": self.workspace_key, "thread": thread });
        self.mutate(method, params, cx);
    }

    pub fn toggle_reaction(&mut self, comment: &str, reacted: bool, cx: &mut Context<Self>) {
        let params = json!({
            "workspace": self.workspace_key, "subject": comment, "reaction": "+1", "remove": reacted,
        });
        self.mutate("review.react", params, cx);
    }

    /// Drops the thread into this workspace's composer as an attachment chip (§10).
    pub fn add_to_agent(&mut self, thread: &str, cx: &mut Context<Self>) {
        let params = json!({ "workspace": self.workspace_key, "ids": [thread] });
        self.mutate("attach.pr_comments", params, cx);
    }

    pub fn remove_note(&mut self, note: &str, reply: Option<&str>, cx: &mut Context<Self>) {
        self.mutate("note.remove", json!({ "id": note, "reply_id": reply }), cx);
    }

    pub fn set_note_resolved(&mut self, note: &str, resolved: bool, cx: &mut Context<Self>) {
        let method = if resolved {
            "note.resolve"
        } else {
            "note.unresolve"
        };
        self.mutate(method, json!({ "id": note }), cx);
    }

    /// Drops the whole note conversation into this workspace's composer as one chip.
    pub fn add_note_to_agent(&mut self, note: &str, cx: &mut Context<Self>) {
        self.mutate("attach.note", json!({ "id": note }), cx);
    }

    /// Sends a failed reply again: drops the failed answer and its question, then asks anew.
    pub fn retry_note_reply(&mut self, note: &str, failed_reply: &str, cx: &mut Context<Self>) {
        let Some((asked, failed)) = self
            .notes
            .iter()
            .find(|candidate| candidate.id == note)
            .and_then(|note| note.retry_target(failed_reply))
            .map(|(asked, failed)| (asked.clone(), failed.clone()))
        else {
            return;
        };
        let client = self.client.clone();
        let note = note.to_owned();
        let calls = [
            request(
                client.clone(),
                "note.remove",
                json!({ "id": note, "reply_id": failed.id }),
                cx,
            ),
            request(
                client,
                "note.remove",
                json!({ "id": note, "reply_id": asked.id }),
                cx,
            ),
        ];
        self.error = None;
        cx.spawn(async move |this, cx| {
            for call in calls.into_iter().flatten() {
                if let Err(error) = call.await {
                    this.update(cx, |this, cx| {
                        this.error = Some(format!("{error:#}"));
                        this.reload(cx);
                    })
                    .log_err();
                    return;
                }
            }
            this.update(cx, |this, cx| {
                this.mutate("note.reply", json!({ "id": note, "body": asked.body }), cx)
            })
            .log_err();
        })
        .detach();
    }

    fn toggle_expanded(&mut self, thread: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(thread) {
            self.expanded.insert(thread.to_owned());
        }
        self.rebuild_blocks(cx);
    }

    /// Uploads an image or video to GitHub and puts its markdown into the composer (§11.5).
    pub fn attach_media(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some((composer, _)) = self.composer.clone() else {
            return;
        };
        let params = json!({ "workspace": self.workspace_key, "path": path });
        let Some(call) = self.call("review.upload", params, cx) else {
            return;
        };
        composer.update(cx, |composer, cx| {
            composer.busy = true;
            cx.notify();
        });
        cx.spawn_in(window, async move |_, cx| {
            let result = call.await;
            composer
                .update_in(cx, |composer, window, cx| {
                    composer.busy = false;
                    match result {
                        // The composer's own edit hook resizes its block.
                        Ok(value) => {
                            let markdown = value["markdown"].as_str().unwrap_or_default();
                            composer.editor.update(cx, |editor, cx| {
                                editor.insert(&format!("{markdown}\n"), window, cx)
                            });
                        }
                        Err(error) => composer.error = Some(format!("{error:#}")),
                    }
                    cx.notify();
                })
                .log_err();
        })
        .detach();
    }
}

impl EventEmitter<()> for DiffReview {}
impl EventEmitter<OpenChangedFile> for DiffReview {}

#[derive(Debug, Clone)]
enum AnyCard {
    Thread(ReviewThread, bool),
    Draft(ReviewDraft),
    Note(WorkspaceNote, bool),
}

fn card_renderer(card: Entity<ReviewCard>) -> editor::display_map::RenderBlock {
    Arc::new(move |cx: &mut BlockContext| {
        div()
            .w_full()
            .pl(cx.margins.gutter.full_width())
            .pr_4()
            .py_1()
            .child(div().max_w(px(760.)).child(card.clone()))
            .into_any_element()
    })
}

fn composer_renderer(composer: Entity<CommentComposer>) -> editor::display_map::RenderBlock {
    Arc::new(move |cx: &mut BlockContext| {
        div()
            .w_full()
            .pl(cx.margins.gutter.full_width())
            .pr_4()
            .py_1()
            .child(div().max_w(px(760.)).child(composer.clone()))
            .into_any_element()
    })
}

fn card_frame(cx: &App) -> Div {
    v_flex()
        .w_full()
        .p_2()
        .gap_1()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border)
        .bg(cx.theme().colors().editor_background)
}

fn render_body(body: &str) -> impl IntoElement {
    let images = markdown_images(body);
    v_flex()
        .gap_1()
        .child(Label::new(body.to_owned()).size(LabelSize::Small))
        .children(images.into_iter().map(|url| {
            img(url)
                .max_h(px(96.))
                .max_w(px(320.))
                .rounded_sm()
                .into_any_element()
        }))
}

/// One inline card: a PR thread or a queued draft.
pub struct ReviewCard {
    card: AnyCard,
    review: WeakEntity<DiffReview>,
}

impl ReviewCard {
    fn render_thread(
        &self,
        thread: &ReviewThread,
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let review = self.review.clone();
        let id = thread.id.clone();
        if thread.is_resolved && !expanded {
            let first = thread.comments.first();
            let summary = format!(
                "Resolved · {}: {}",
                first.and_then(|c| c.author.clone()).unwrap_or_default(),
                first
                    .map(|c| c.body.lines().next().unwrap_or_default().to_owned())
                    .unwrap_or_default()
            );
            return card_frame(cx)
                .child(
                    Button::new(SharedString::from(format!("expand-{id}")), summary)
                        .style(ButtonStyle::Transparent)
                        .label_size(LabelSize::Small)
                        .start_icon(
                            Icon::new(IconName::Check)
                                .size(IconSize::XSmall)
                                .color(Color::Success),
                        )
                        .on_click(move |_, _, cx| {
                            review
                                .update(cx, |review, cx| review.toggle_expanded(&id, cx))
                                .log_err();
                        }),
                )
                .into_any_element();
        }
        let mut card = card_frame(cx);
        if !thread.placement.label.is_empty() {
            card = card.child(
                Label::new(thread.placement.label.clone())
                    .size(LabelSize::XSmall)
                    .color(if thread.placement.outdated {
                        Color::Warning
                    } else {
                        Color::Muted
                    }),
            );
        }
        for comment in &thread.comments {
            let author = comment.author.clone().unwrap_or_else(|| "ghost".into());
            let liked = comment
                .reactions
                .iter()
                .any(|r| r.content == "+1" && r.viewer_has_reacted);
            let comment_id = comment.id.clone();
            let review_for_like = self.review.clone();
            card = card.child(
                v_flex()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(render_avatar(
                                comment.author_avatar_url.as_deref(),
                                &author,
                                AvatarKind::Person,
                                px(16.),
                                cx,
                            ))
                            .child(
                                Label::new(format!("@{author}"))
                                    .size(LabelSize::Small)
                                    .weight(gpui::FontWeight::SEMIBOLD),
                            )
                            .when(comment.is_bot, |row| {
                                row.child(
                                    Label::new("bot")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            })
                            .child(div().flex_1())
                            .child(
                                IconButton::new(
                                    SharedString::from(format!("like-{}", comment.id)),
                                    IconName::ThumbsUp,
                                )
                                .icon_size(IconSize::XSmall)
                                .toggle_state(liked)
                                .tooltip(Tooltip::text(if liked { "Remove 👍" } else { "👍" }))
                                .on_click(move |_, _, cx| {
                                    review_for_like
                                        .update(cx, |review, cx| {
                                            review.toggle_reaction(&comment_id, liked, cx)
                                        })
                                        .log_err();
                                }),
                            ),
                    )
                    .child(render_body(&comment.body))
                    .when(!comment.reactions.is_empty(), |column| {
                        column.child(h_flex().gap_1().children(comment.reactions.iter().map(|r| {
                            Label::new(format!("{} {}", reaction_emoji(&r.content), r.count))
                                .size(LabelSize::XSmall)
                                .color(if r.viewer_has_reacted {
                                    Color::Accent
                                } else {
                                    Color::Muted
                                })
                        })))
                    }),
            );
        }
        let resolve_review = self.review.clone();
        let reply_review = self.review.clone();
        let agent_review = self.review.clone();
        let collapse_review = self.review.clone();
        let (reply_id, resolve_id, agent_id, collapse_id) =
            (id.clone(), id.clone(), id.clone(), id.clone());
        let resolved = thread.is_resolved;
        let can_toggle = if resolved {
            thread.viewer_can_unresolve
        } else {
            thread.viewer_can_resolve
        };
        card.child(
            h_flex()
                .gap_1()
                .child(
                    Button::new(SharedString::from(format!("agent-{id}")), "Add to agent")
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::ZedAgent).size(IconSize::XSmall))
                        .on_click(move |_, _, cx| {
                            agent_review
                                .update(cx, |review, cx| review.add_to_agent(&agent_id, cx))
                                .log_err();
                        }),
                )
                .when(thread.viewer_can_reply, |row| {
                    row.child(
                        Button::new(SharedString::from(format!("reply-{id}")), "Reply")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(move |_, window, cx| {
                                reply_review
                                    .update(cx, |review, cx| {
                                        review.open_composer(
                                            ComposerTarget::Reply {
                                                thread: reply_id.clone(),
                                            },
                                            String::new(),
                                            window,
                                            cx,
                                        )
                                    })
                                    .log_err();
                            }),
                    )
                })
                .when(can_toggle, |row| {
                    row.child(
                        Button::new(
                            SharedString::from(format!("resolve-{id}")),
                            if resolved { "Unresolve" } else { "Resolve" },
                        )
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .on_click(move |_, _, cx| {
                            resolve_review
                                .update(cx, |review, cx| {
                                    review.set_resolved(&resolve_id, !resolved, cx)
                                })
                                .log_err();
                        }),
                    )
                })
                .when(resolved, |row| {
                    row.child(
                        Button::new(SharedString::from(format!("collapse-{id}")), "Collapse")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(move |_, _, cx| {
                                collapse_review
                                    .update(cx, |review, cx| {
                                        review.toggle_expanded(&collapse_id, cx)
                                    })
                                    .log_err();
                            }),
                    )
                }),
        )
        .into_any_element()
    }

    fn render_draft(&self, draft: &ReviewDraft, cx: &mut Context<Self>) -> AnyElement {
        let edit_review = self.review.clone();
        let delete_review = self.review.clone();
        let (edit_id, delete_id, body) = (draft.id.clone(), draft.id.clone(), draft.body.clone());
        let location = match (draft.subject.as_str(), draft.start_line, draft.line) {
            ("file", _, _) | (_, _, None) => "File comment".to_owned(),
            (_, Some(start), Some(end)) => lines_label(start, end),
            (_, None, Some(line)) => lines_label(line, line),
        };
        card_frame(cx)
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .px_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(cx.theme().status().warning_border)
                            .bg(cx.theme().status().warning_background)
                            .child(
                                Label::new("PENDING")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Warning)
                                    .weight(gpui::FontWeight::SEMIBOLD),
                            ),
                    )
                    .child(
                        Label::new(location)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        IconButton::new(
                            SharedString::from(format!("edit-{}", draft.id)),
                            IconName::Pencil,
                        )
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Edit comment"))
                        .on_click(move |_, window, cx| {
                            edit_review
                                .update(cx, |review, cx| {
                                    review.open_composer(
                                        ComposerTarget::EditDraft {
                                            id: edit_id.clone(),
                                        },
                                        body.clone(),
                                        window,
                                        cx,
                                    )
                                })
                                .log_err();
                        }),
                    )
                    .child(
                        IconButton::new(
                            SharedString::from(format!("delete-{}", draft.id)),
                            IconName::Trash,
                        )
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Delete comment"))
                        .on_click(move |_, _, cx| {
                            delete_review
                                .update(cx, |review, cx| review.remove_draft(&delete_id, cx))
                                .log_err();
                        }),
                    ),
            )
            .child(render_body(&draft.body))
            .into_any_element()
    }
}

impl ReviewCard {
    fn note_frame(cx: &App) -> Div {
        // An accent edge tells notes apart from PR threads at a glance.
        card_frame(cx)
            .border_l_2()
            .border_color(cx.theme().colors().text_accent)
    }

    fn note_badge(note: &WorkspaceNote, cx: &App) -> impl IntoElement {
        let (label, icon) = if note.by_agent() {
            ("AGENT NOTE", IconName::ZedAgent)
        } else {
            ("NOTE", IconName::Notepad)
        };
        h_flex()
            .gap_0p5()
            .px_1()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Accent))
            .child(
                Label::new(label)
                    .size(LabelSize::XSmall)
                    .color(Color::Accent)
                    .weight(gpui::FontWeight::SEMIBOLD),
            )
    }

    fn render_note(
        &self,
        note: &WorkspaceNote,
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = note.id.clone();
        if note.resolved && !expanded {
            let review = self.review.clone();
            let summary = format!(
                "Resolved note: {}",
                note.body.lines().next().unwrap_or_default()
            );
            return Self::note_frame(cx)
                .child(
                    Button::new(SharedString::from(format!("note-expand-{id}")), summary)
                        .style(ButtonStyle::Transparent)
                        .label_size(LabelSize::Small)
                        .start_icon(
                            Icon::new(IconName::Check)
                                .size(IconSize::XSmall)
                                .color(Color::Success),
                        )
                        .on_click(move |_, _, cx| {
                            review
                                .update(cx, |review, cx| review.toggle_expanded(&id, cx))
                                .log_err();
                        }),
                )
                .into_any_element();
        }
        let lines = lines_label(note.line, note.end_line.max(note.line));
        let (edit_review, delete_review) = (self.review.clone(), self.review.clone());
        let (edit_id, delete_id, body) = (id.clone(), id.clone(), note.body.clone());
        let mut card = Self::note_frame(cx).child(
            h_flex()
                .gap_1p5()
                .child(Self::note_badge(note, cx))
                .child(
                    Label::new(lines)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(div().flex_1())
                .child(
                    IconButton::new(
                        SharedString::from(format!("note-edit-{id}")),
                        IconName::Pencil,
                    )
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("Edit note"))
                    .on_click(move |_, window, cx| {
                        edit_review
                            .update(cx, |review, cx| {
                                review.open_composer(
                                    ComposerTarget::EditNote {
                                        note: edit_id.clone(),
                                        reply: None,
                                    },
                                    body.clone(),
                                    window,
                                    cx,
                                )
                            })
                            .log_err();
                    }),
                )
                .child(
                    IconButton::new(
                        SharedString::from(format!("note-delete-{id}")),
                        IconName::Trash,
                    )
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("Delete note"))
                    .on_click(move |_, _, cx| {
                        delete_review
                            .update(cx, |review, cx| review.remove_note(&delete_id, None, cx))
                            .log_err();
                    }),
                ),
        );
        card = card.child(render_body(&note.body));
        for reply in &note.replies {
            card = card.child(self.render_note_reply(&note.id, reply));
        }
        let (reply_review, agent_review, resolve_review, collapse_review) = (
            self.review.clone(),
            self.review.clone(),
            self.review.clone(),
            self.review.clone(),
        );
        let (reply_id, agent_id, resolve_id, collapse_id) =
            (id.clone(), id.clone(), id.clone(), id.clone());
        let resolved = note.resolved;
        card.child(
            h_flex()
                .gap_1()
                .child(
                    Button::new(SharedString::from(format!("note-reply-{id}")), "Reply")
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .tooltip(Tooltip::text(
                            "A separate agent session reads the whole note and answers",
                        ))
                        .on_click(move |_, window, cx| {
                            reply_review
                                .update(cx, |review, cx| {
                                    review.open_composer(
                                        ComposerTarget::NoteReply {
                                            note: reply_id.clone(),
                                        },
                                        String::new(),
                                        window,
                                        cx,
                                    )
                                })
                                .log_err();
                        }),
                )
                .child(
                    Button::new(
                        SharedString::from(format!("note-agent-{id}")),
                        "Add to agent",
                    )
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::ZedAgent).size(IconSize::XSmall))
                    .on_click(move |_, _, cx| {
                        agent_review
                            .update(cx, |review, cx| review.add_note_to_agent(&agent_id, cx))
                            .log_err();
                    }),
                )
                .child(
                    Button::new(
                        SharedString::from(format!("note-resolve-{id}")),
                        if resolved { "Unresolve" } else { "Resolve" },
                    )
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .on_click(move |_, _, cx| {
                        resolve_review
                            .update(cx, |review, cx| {
                                review.set_note_resolved(&resolve_id, !resolved, cx)
                            })
                            .log_err();
                    }),
                )
                .when(resolved, |row| {
                    row.child(
                        Button::new(
                            SharedString::from(format!("note-collapse-{id}")),
                            "Collapse",
                        )
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .on_click(move |_, _, cx| {
                            collapse_review
                                .update(cx, |review, cx| review.toggle_expanded(&collapse_id, cx))
                                .log_err();
                        }),
                    )
                }),
        )
        .into_any_element()
    }

    fn render_note_reply(&self, note: &str, reply: &NoteReply) -> AnyElement {
        let author = match reply.author.as_str() {
            "user" => "You",
            "reply-agent" => "Answer",
            _ => "Agent",
        };
        let mut column = v_flex().gap_0p5().pl_2().child(
            h_flex()
                .gap_1()
                .when(reply.author != "user", |row| {
                    row.child(
                        Icon::new(IconName::ZedAgent)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                })
                .child(
                    Label::new(author)
                        .size(LabelSize::Small)
                        .weight(gpui::FontWeight::SEMIBOLD),
                )
                .when(reply.author == "user", |row| {
                    let review = self.review.clone();
                    let (note, reply_id) = (note.to_owned(), reply.id.clone());
                    row.child(div().flex_1()).child(
                        IconButton::new(
                            SharedString::from(format!("note-reply-delete-{}", reply.id)),
                            IconName::Trash,
                        )
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Delete reply"))
                        .on_click(move |_, _, cx| {
                            review
                                .update(cx, |review, cx| {
                                    review.remove_note(&note, Some(&reply_id), cx)
                                })
                                .log_err();
                        }),
                    )
                }),
        );
        if reply.is_thinking() {
            column = column.child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::ArrowCircle)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new("Thinking…")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            );
        } else if reply.is_failed() {
            let review = self.review.clone();
            let (note, reply_id) = (note.to_owned(), reply.id.clone());
            column = column.child(
                h_flex()
                    .gap_1()
                    .child(
                        Label::new(
                            reply
                                .error
                                .clone()
                                .unwrap_or_else(|| "The answer failed".into()),
                        )
                        .size(LabelSize::Small)
                        .color(Color::Error),
                    )
                    .child(
                        Button::new(
                            SharedString::from(format!("note-retry-{}", reply.id)),
                            "Retry",
                        )
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .on_click(move |_, _, cx| {
                            review
                                .update(cx, |review, cx| {
                                    review.retry_note_reply(&note, &reply_id, cx)
                                })
                                .log_err();
                        }),
                    ),
            );
        } else {
            column = column.child(render_body(&reply.body));
        }
        if !reply.changed_files.is_empty() {
            let review = self.review.clone();
            column = column.child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .child(
                        Label::new("Changed")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .children(reply.changed_files.iter().map(|path| {
                        let review = review.clone();
                        let path_for_click = path.clone();
                        Button::new(
                            SharedString::from(format!("note-file-{}-{path}", reply.id)),
                            path.clone(),
                        )
                        .style(ButtonStyle::Transparent)
                        .label_size(LabelSize::XSmall)
                        .color(Color::Accent)
                        .tooltip(Tooltip::text("Open this file's diff"))
                        .on_click(move |_, _, cx| {
                            review
                                .update(cx, |_, cx| {
                                    cx.emit(OpenChangedFile(path_for_click.clone()))
                                })
                                .log_err();
                        })
                    })),
            );
        }
        column.into_any_element()
    }
}

impl Render for ReviewCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.card.clone() {
            AnyCard::Thread(thread, expanded) => self.render_thread(&thread, expanded, cx),
            AnyCard::Draft(draft) => self.render_draft(&draft, cx),
            AnyCard::Note(note, expanded) => self.render_note(&note, expanded, cx),
        }
    }
}

/// The comment box opened under a line, a thread or a draft (§2).
pub struct CommentComposer {
    target: ComposerTarget,
    editor: Entity<Editor>,
    review: WeakEntity<DiffReview>,
    destination: Destination,
    busy: bool,
    error: Option<String>,
    _subscription: Subscription,
}

impl CommentComposer {
    fn new(
        target: ComposerTarget,
        text: String,
        review: WeakEntity<DiffReview>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = match &target {
            ComposerTarget::Reply { .. } => "Reply…",
            ComposerTarget::EditDraft { .. } => "Edit comment",
            ComposerTarget::Lines { .. } => "Leave a review comment on this line",
            ComposerTarget::NoteReply { .. } => "Ask about this note; a separate agent answers…",
            ComposerTarget::EditNote { .. } => "Edit note",
        };
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 14, window, cx);
            editor.set_placeholder_text(placeholder, window, cx);
            editor.set_soft_wrap();
            if !text.is_empty() {
                editor.set_text(text, window, cx);
            }
            editor
        });
        let review_for_resize = review.clone();
        let subscription = cx.subscribe(&editor, move |this, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                let height = this.height(cx);
                review_for_resize
                    .update(cx, |review, cx| review.resize_composer(height, cx))
                    .log_err();
            }
        });
        Self {
            target,
            editor,
            review,
            destination: Destination::Review,
            busy: false,
            error: None,
            _subscription: subscription,
        }
    }

    pub fn target(&self) -> &ComposerTarget {
        &self.target
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn error_text(&self) -> Option<String> {
        self.error.clone()
    }

    pub fn set_destination(&mut self, destination: Destination, cx: &mut Context<Self>) {
        self.destination = destination;
        cx.notify();
    }

    fn height(&self, cx: &App) -> u32 {
        let text = self.editor.read(cx).text(cx);
        let lines = (text.lines().count() as u32).clamp(3, 14);
        let images = markdown_images(&text).len() as u32;
        4 + lines + images * 5 + u32::from(self.error.is_some())
    }

    // Both deferred: the review updates this composer while sending, which would double-lease it.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let destination = self.destination;
        let review = self.review.clone();
        window.defer(cx, move |window, cx| {
            review
                .update(cx, |review, cx| {
                    review.submit_composer(destination, window, cx)
                })
                .log_err();
        });
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let review = self.review.clone();
        window.defer(cx, move |_, cx| {
            review
                .update(cx, |review, cx| review.close_composer(cx))
                .log_err();
        });
    }

    fn pick_media(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Attach image or video".into()),
        });
        let review = self.review.clone();
        cx.spawn_in(window, async move |_, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(path) = paths.into_iter().next()
            {
                review
                    .update_in(cx, |review, window, cx| {
                        review.attach_media(path, window, cx)
                    })
                    .log_err();
            }
        })
        .detach();
    }
}

impl Focusable for CommentComposer {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for CommentComposer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = match &self.target {
            ComposerTarget::Lines { start, end } => lines_label(*start, *end).to_uppercase(),
            ComposerTarget::EditDraft { .. } => "EDIT COMMENT".to_owned(),
            ComposerTarget::Reply { .. } => "REPLY".to_owned(),
            ComposerTarget::NoteReply { .. } => "REPLY TO NOTE".to_owned(),
            ComposerTarget::EditNote { .. } => "EDIT NOTE".to_owned(),
        };
        let new_comment = matches!(self.target, ComposerTarget::Lines { .. });
        let submit_label = match (&self.target, self.destination) {
            (_, _) if self.busy => "Posting…",
            (ComposerTarget::Lines { .. }, Destination::Review) => "Add to review",
            (ComposerTarget::Lines { .. }, Destination::Agent) => "Add note",
            (ComposerTarget::EditDraft { .. } | ComposerTarget::EditNote { .. }, _) => "Save",
            (ComposerTarget::Reply { .. } | ComposerTarget::NoteReply { .. }, _) => "Reply",
        };
        let empty = self.editor.read(cx).text(cx).trim().is_empty();
        let images = markdown_images(&self.editor.read(cx).text(cx));
        card_frame(cx)
            .key_context("IcarusReviewComposer")
            .on_action(
                cx.listener(|this, _: &SubmitReviewComment, window, cx| this.submit(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &CancelReviewComment, window, cx| this.cancel(window, cx)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(header)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(div().flex_1())
                    .when(new_comment, |row| {
                        row.child(
                            ui::ToggleButtonGroup::single_row(
                                "review-destination",
                                [
                                    ui::ToggleButtonSimple::new(
                                        "Agent",
                                        cx.listener(|this, _, _, cx| {
                                            this.set_destination(Destination::Agent, cx)
                                        }),
                                    ),
                                    ui::ToggleButtonSimple::new(
                                        "Add to review",
                                        cx.listener(|this, _, _, cx| {
                                            this.set_destination(Destination::Review, cx)
                                        }),
                                    ),
                                ],
                            )
                            .selected_index(match self.destination {
                                Destination::Agent => 0,
                                Destination::Review => 1,
                            })
                            .style(ui::ToggleButtonGroupStyle::Outlined)
                            .label_size(LabelSize::XSmall),
                        )
                    }),
            )
            .child(
                div()
                    .p_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.editor.clone()),
            )
            .children(images.into_iter().map(|url| {
                img(url)
                    .max_h(px(72.))
                    .max_w(px(240.))
                    .rounded_sm()
                    .into_any_element()
            }))
            .when_some(self.error.clone(), |column, error| {
                column.child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
            })
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("review-attach-media", "Image or video")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Image).size(IconSize::XSmall))
                            .disabled(self.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.pick_media(window, cx)),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("review-cancel", "Cancel")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
                    )
                    .child(
                        Button::new("review-submit", submit_label)
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .disabled(self.busy || empty)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Installs an RPC stub; `answer` decides each reply, and every call is recorded.
    pub(crate) fn stub_rpc(
        cx: &mut App,
        answer: impl Fn(&str, &Value) -> Result<Value> + Send + Sync + 'static,
    ) -> Arc<Mutex<Vec<(String, Value)>>> {
        let calls: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let recorded = calls.clone();
        cx.set_global(RpcStub(Arc::new(move |method, params| {
            recorded
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push((method.to_owned(), params.clone()));
            answer(method, params)
        })));
        calls
    }

    pub(crate) fn calls_to(calls: &Mutex<Vec<(String, Value)>>, method: &str) -> Vec<Value> {
        calls
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .filter(|(name, _)| name == method)
            .map(|(_, params)| params.clone())
            .collect()
    }

    fn review_view(drafts: usize) -> Value {
        json!({
            "workspace": "ws-1",
            "pr": { "repo": "acme/widgets", "number": 7, "base_ref_name": "main", "author": "someone" },
            "viewer": "me",
            "standing": if drafts > 0 { "pending" } else { "requested" },
            "verdicts": ["comment", "request-changes", "approve"],
            "can_submit": { "comment": true, "request-changes": true, "approve": true },
            "base": {
                "selected": "since-review", "default": "since-review",
                "options": [
                    { "value": "since-review", "label": "Since your last review (Review 1)", "available": true },
                    { "value": "commit:abc1234", "label": "abc1234 Fix", "tags": ["Review 1"], "available": true },
                    { "value": "whole", "label": "main · target branch", "available": true },
                ],
            },
            "drafts": (0..drafts).map(|i| json!({ "id": format!("d{i}"), "path": "src/app.rs", "line": 2, "subject": "line", "body": "queued" })).collect::<Vec<_>>(),
            "summary": "",
        })
    }

    #[gpui::test]
    async fn the_review_section_submits_sets_the_base_and_keeps_github_errors(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let calls = cx.update(|cx| {
            stub_rpc(cx, |method, params| match method {
                "review.get" => Ok(review_view(1)),
                "workspace.changes" => Ok(json!({ "rows": [{ "path": "a" }] })),
                "review.submit" if params["verdict"] == "approve" => Err(anyhow::anyhow!(
                    "Pull request review thread line must be part of the diff"
                )),
                _ => Ok(json!({})),
            })
        });
        let window = cx.add_empty_window();
        let section = window
            .update(|window, cx| cx.new(|cx| ReviewSection::new("ws-1".into(), None, window, cx)));
        window.run_until_parked();
        section.read_with(window, |section, _| {
            let view = section.view().expect("loaded");
            assert_eq!(view.drafts.len(), 1);
            assert_eq!(view.standing.as_deref(), Some("pending"));
            assert_eq!(section.uncommitted, 1, "re-request stays disabled");
        });

        section.update_in(window, |section, window, cx| {
            section
                .summary_editor
                .update(cx, |editor, cx| editor.set_text("Looks good", window, cx));
            section.submit("comment", window, cx);
        });
        window.run_until_parked();
        let submits = calls_to(&calls, "review.submit");
        assert_eq!(
            submits[0],
            json!({ "workspace": "ws-1", "verdict": "comment", "body": "Looks good" })
        );
        section.read_with(window, |section, cx| {
            assert!(section.error().is_none());
            assert_eq!(
                section.summary_editor.read(cx).text(cx),
                "",
                "a sent summary clears"
            );
        });

        section.update_in(window, |section, window, cx| {
            section.submit("approve", window, cx)
        });
        window.run_until_parked();
        section.read_with(window, |section, _| {
            assert_eq!(
                section.error(),
                Some("Pull request review thread line must be part of the diff"),
                "GitHub's message is shown verbatim"
            );
            assert_eq!(
                section.view().map(|v| v.drafts.len()),
                Some(1),
                "drafts are kept"
            );
        });

        section.update_in(window, |section, window, cx| {
            section.set_base("commit:abc1234".into(), window, cx)
        });
        window.run_until_parked();
        assert_eq!(
            calls_to(&calls, "review.set_base"),
            [json!({ "workspace": "ws-1", "base": "commit:abc1234" })]
        );

        section.update_in(window, |section, window, cx| {
            section.request_reviewers(vec!["alice, acme/backend".into()], window, cx)
        });
        window.run_until_parked();
        section.update_in(window, |section, window, cx| {
            section.remove_reviewer("alice".into(), window, cx)
        });
        window.run_until_parked();
        assert_eq!(
            calls_to(&calls, "review.request_reviewers")[0]["reviewers"],
            json!(["alice, acme/backend"])
        );
        assert_eq!(
            calls_to(&calls, "review.remove_reviewers")[0]["reviewers"],
            json!(["alice"])
        );
    }

    fn pull_request(value: Value) -> PullRequest {
        serde_json::from_value(value).expect("pull request")
    }

    #[test]
    fn reviewer_rows_follow_requests_then_reviews() {
        let pr = pull_request(json!({
            "number": 7, "title": "t", "url": "u", "state": "OPEN", "head_ref_name": "h",
            "base_ref_name": "main", "head_ref_oid": "abc", "author": "me",
            "requested_reviewers": [
                { "kind": "user", "login": "alice" },
                { "kind": "team", "slug": "acme/backend" },
                { "kind": "user", "login": "bob" },
            ],
            "latest_reviews": [
                { "login": "bob", "state": "APPROVED" },
                { "login": "me", "state": "COMMENTED" },
                { "login": "dave", "state": "PENDING" },
                { "login": "erin", "state": "COMMENTED" },
            ],
            "latest_opinionated_reviews": [
                { "login": "bob", "state": "APPROVED" },
                { "login": "carol", "state": "CHANGES_REQUESTED" },
            ],
        }));
        let rows = reviewer_rows(&pr);
        let summary: Vec<(&str, ReviewerState, bool, bool)> = rows
            .iter()
            .map(|r| (r.name.as_str(), r.state, r.can_rerequest, r.can_remove))
            .collect();
        assert_eq!(
            summary,
            [
                ("alice", ReviewerState::Pending, false, true),
                ("acme/backend", ReviewerState::Pending, false, true),
                ("bob", ReviewerState::ReRequested, false, false),
                ("erin", ReviewerState::Commented, true, false),
                ("carol", ReviewerState::ChangesRequested, true, false),
            ],
            "the author and pending reviews are left out"
        );
        assert!(rows[1].team);
    }

    #[test]
    fn selections_become_comment_lines() {
        assert_eq!(comment_lines(4, 4, 3), (5, 5));
        assert_eq!(
            comment_lines(4, 7, 0),
            (5, 7),
            "column 0 drops the last line"
        );
        assert_eq!(comment_lines(7, 4, 2), (5, 8), "backwards selections work");
        assert_eq!(lines_label(5, 5), "Line 5");
        assert_eq!(lines_label(5, 8), "Lines 5-8");
    }

    #[test]
    fn standing_pill_hides_commented_and_dismissed() {
        assert_eq!(
            standing_pill(Some("pending")).map(|p| p.0),
            Some("Your review is pending")
        );
        assert_eq!(
            standing_pill(Some("approved")).map(|p| p.0),
            Some("You approved the changes")
        );
        assert!(standing_pill(Some("commented")).is_none());
        assert!(standing_pill(Some("dismissed")).is_none());
        assert!(standing_pill(None).is_none());
    }

    #[test]
    fn heights_grow_with_bodies_images_and_collapse_when_resolved() {
        let mut thread = ReviewThread {
            comments: vec![ThreadComment {
                body: "one\ntwo".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(thread_height(&thread, false), 3 + 1 + 2);
        thread.is_resolved = true;
        assert_eq!(thread_height(&thread, false), 2);
        let draft = ReviewDraft {
            body: "look ![shot](https://x/y.png)".into(),
            ..Default::default()
        };
        assert_eq!(markdown_images(&draft.body), ["https://x/y.png"]);
        assert_eq!(draft_height(&draft), 3 + 1 + 6);
    }
}
