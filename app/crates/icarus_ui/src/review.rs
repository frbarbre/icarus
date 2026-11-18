//! GitHub review mode (github-review.md): the Review section in Source Control, and draft comments,
//! inline PR threads and the comment composer inside diff tabs. Everything goes through `review.*` RPCs.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use collections::{HashMap, HashSet};
use editor::display_map::{
    BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId,
};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    Focusable, PathPromptOptions, Render, SharedString, Subscription, Task, WeakEntity, Window,
    actions, px,
};
use gpui_tokio::Tokio;
use icarus_client::Client;
use language::{Buffer, Point};
use picker::{Picker, PickerDelegate};
use serde::Deserialize;
use serde_json::{Value, json};
use ui::{ButtonLike, ContextMenu, PopoverMenu, PopoverMenuHandle, Tooltip, prelude::*};
use util::ResultExt as _;

use crate::AppConnection;
use crate::comment_input::{
    MentionSource, Mentionable, UploadSource, clipboard_images, enable_completions, upload_into,
};
use crate::github_panel::{
    AvatarKind, PullRequest, RequestedReviewer, Review, ReviewState, render_avatar,
};
use crate::pr_comments;
use crate::pr_store::{PrReviewData, PrReviewFailed, PrReviewStore, ReviewOp};

actions!(
    icarus,
    [
        /// Opens a review comment box on the selected lines of the diff tab (⌘⇧A).
        AddReviewComment,
        /// Sends the open review comment box (⌘Enter).
        SubmitReviewComment,
        /// Closes the open review comment box (Escape).
        CancelReviewComment,
    ]
);

const THREAD_REFRESH_INTERVAL: Duration = Duration::from_secs(45);
const SUMMARY_SAVE_DELAY: Duration = Duration::from_millis(600);
const REVIEWER_SLOT: gpui::Pixels = gpui::px(20.);
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
    pub original_line: Option<u32>,
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
    pub created_at: Option<String>,
    pub last_edited_at: Option<String>,
    pub reactions: Vec<ReactionSummary>,
    pub viewer_can_update: bool,
    pub viewer_can_delete: bool,
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
    /// For `reply-agent` entries: the throwaway session, whose transcript stays in history.
    pub agent_id: Option<String>,
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
    let (text, media) = crate::media::split_media(body);
    let lines: u32 = text
        .lines()
        .map(|line| (line.chars().count() as u32).div_ceil(90).max(1))
        .sum::<u32>()
        .max(1);
    // Markdown code blocks carry padding and margins worth about two more lines each.
    let fences = text
        .lines()
        .filter(|line| line.trim_start().starts_with("```"))
        .count() as u32;
    // A row of up to three thumbnails, each at most 180 px tall.
    lines + fences + (media.len() as u32).div_ceil(3) * 10
}

/// Editor lines a thread card needs; a resolved thread starts as one collapsed row.
pub fn thread_height(thread: &ReviewThread, expanded: bool) -> u32 {
    if thread.is_resolved && !expanded {
        return 2;
    }
    // Orca's card: a two-row header on the first comment and one row on replies, the body at a
    // taller line height than the editor's (about 6 lines for every 5), its padding and reactions.
    let comments: u32 = thread
        .comments
        .iter()
        .enumerate()
        .map(|(index, comment)| {
            let header = if index == 0 { 3 } else { 2 };
            header + (body_lines(&comment.body) * 6).div_ceil(5) + 3
        })
        .sum();
    1 + comments
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

/// Shows the viewer's reaction added or removed before GitHub confirms it.
pub(crate) fn toggle_reaction_locally(
    reactions: &mut Vec<ReactionSummary>,
    content: &str,
    react: bool,
) {
    match reactions.iter_mut().find(|r| r.content == content) {
        Some(summary) if summary.viewer_has_reacted != react => {
            summary.viewer_has_reacted = react;
            summary.count = if react {
                summary.count + 1
            } else {
                summary.count.saturating_sub(1)
            };
        }
        Some(_) => {}
        None if react => reactions.push(ReactionSummary {
            content: content.to_owned(),
            count: 1,
            viewer_has_reacted: true,
        }),
        None => {}
    }
    reactions.retain(|r| r.count > 0);
}

pub(crate) fn reaction_emoji(content: &str) -> &'static str {
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

/// GitHub's eight reactions, in the order GitHub's own picker shows them.
pub const REACTIONS: [&str; 8] = [
    "+1", "-1", "laugh", "hooray", "confused", "heart", "rocket", "eyes",
];

pub(crate) fn app_client(cx: &App) -> Option<Arc<Client>> {
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

/// Marks an editor that already has a review layer (a diff tab's), and keeps a file editor's notes alive.
#[derive(Default)]
pub struct ReviewAttached(pub Option<Entity<DiffReview>>);

impl editor::Addon for ReviewAttached {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Shows PR threads, review drafts and notes in plain file editors of Icarus workspaces too.
pub fn attach_notes_to_file_editors(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, window, cx| {
        let Some(window) = window else {
            return;
        };
        if !editor.mode().is_full() || !crate::enabled(cx) {
            return;
        }
        // Deferred: a diff tab marks its editor right after creating it.
        cx.defer_in(window, |editor, _window, cx| attach_notes(editor, cx));
    })
    .detach();
}

fn attach_notes(editor: &mut Editor, cx: &mut Context<Editor>) {
    if editor.addon::<ReviewAttached>().is_some() {
        return;
    }
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    let Some(project) = editor.project().cloned() else {
        return;
    };
    let Some(file) = buffer
        .read(cx)
        .file()
        .and_then(|file| project::File::from_dyn(Some(file)).cloned())
    else {
        return;
    };
    let Some(root) = project
        .read(cx)
        .worktree_for_id(file.worktree_id(cx), cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    else {
        return;
    };
    if crate::routing::server_for_path(&root, cx).is_none() {
        return;
    }
    let relative = file.path.as_unix_str().to_owned();
    let client = crate::routing::client_for_project(project.read(cx), cx);
    let weak = cx.entity().downgrade();
    let workspace_key = root.to_string_lossy().into_owned();
    let review =
        cx.new(|cx| DiffReview::file_editor(weak, buffer, workspace_key, relative, client, cx));
    editor.register_addon(ReviewAttached(Some(review)));
}

/// Someone the reviewer picker offers.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ReviewerCandidate {
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
    /// GitHub's suggestion for this PR, or a frequent reviewer in the repository.
    #[serde(skip)]
    pub suggested: bool,
}

/// `review.reviewer_suggestions`' answer: the suggestions first, then everyone else who can review.
pub fn reviewer_candidates_from(value: &Value) -> Vec<ReviewerCandidate> {
    let list = |key: &str, suggested: bool| {
        serde_json::from_value::<Vec<ReviewerCandidate>>(value[key].clone())
            .unwrap_or_default()
            .into_iter()
            .map(move |candidate| ReviewerCandidate {
                suggested,
                ..candidate
            })
    };
    list("suggested", true)
        .chain(list("everyone", false))
        .collect()
}

/// `review.mentionables`' answer as a list: the PR's author first, then the assignable users.
pub fn mentionables_from(value: &Value) -> Vec<Mentionable> {
    let mut people: Vec<Mentionable> = value["author"]
        .as_str()
        .map(|login| Mentionable {
            login: login.to_owned(),
            name: None,
        })
        .into_iter()
        .collect();
    let users: Vec<Mentionable> =
        serde_json::from_value(value["users"].clone()).unwrap_or_default();
    for user in users {
        if !people.iter().any(|known| known.login == user.login) {
            people.push(user);
        }
    }
    people
}

pub(crate) fn request(
    client: Option<Arc<Client>>,
    method: &'static str,
    params: Value,
    cx: &mut App,
) -> Option<Task<Result<Value>>> {
    #[cfg(test)]
    if let Some(stub) = cx.try_global::<RpcStub>() {
        let answer = (stub.0)(method, &params);
        if cx
            .try_global::<tests::HeldRpc>()
            .is_some_and(|held| held.0.contains(&method))
        {
            return Some(cx.background_spawn(futures::future::pending()));
        }
        return Some(Task::ready(answer));
    }
    let client = client?;
    Some(Tokio::spawn_result(cx, async move {
        client.request(method, params).await
    }))
}

// ---------------------------------------------------------------------------------------------
// The Review section in Source Control
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
    candidates: Vec<ReviewerCandidate>,
    candidates_loaded: bool,
    candidates_error: Option<String>,
    picker_menu: PopoverMenuHandle<Picker<ReviewerPickerDelegate>>,
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
        let people = MentionSource::default();
        let summary_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 10, window, cx);
            editor.set_placeholder_text("Add review summary…", window, cx);
            editor.set_soft_wrap();
            editor.set_show_gutter(false, cx);
            editor.set_text_style_refinement(ui_text_style(SUMMARY_TEXT, cx));
            enable_completions(&mut editor, people.clone(), false, cx);
            editor
        });
        if let Some(call) = request(
            client.clone(),
            "review.mentionables",
            json!({ "workspace": workspace }),
            cx,
        ) {
            let people = people.clone();
            cx.spawn(async move |_, _| match call.await {
                Ok(value) => people.set(mentionables_from(&value)),
                Err(error) => log::warn!("loading assignable users failed: {error:#}"),
            })
            .detach();
        }
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
            candidates: Vec::new(),
            candidates_loaded: false,
            candidates_error: None,
            picker_menu: PopoverMenuHandle::default(),
            applying_summary: false,
            reload_requested: false,
            _save_summary: Task::ready(()),
            _load: Task::ready(()),
            _subscriptions: subscriptions,
        };
        let this = cx.entity().downgrade();
        cx.default_global::<ReviewListeners>().sections.push(this);
        section.load_candidates(cx);
        section.reload(window, cx);
        section
    }

    fn load_candidates(&mut self, cx: &mut Context<Self>) {
        let Some(call) = request(
            self.client.clone(),
            "review.reviewer_suggestions",
            json!({ "workspace": self.workspace }),
            cx,
        ) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                this.candidates_loaded = true;
                match result {
                    Ok(value) => {
                        this.candidates = reviewer_candidates_from(&value);
                        this.candidates_error = None;
                    }
                    Err(error) => this.candidates_error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    /// What the reviewer picker lists, as Orca's: suggestions first, without anyone who already
    /// reviewed (their row offers re-request). Someone asked but not yet reviewing stays, ticked, and
    /// picking them again takes the request back.
    pub fn picker_candidates(&self) -> Vec<ReviewerCandidate> {
        self.candidates
            .iter()
            .filter(|candidate| {
                !self.rows.iter().any(|row| {
                    !row.team && !row.can_remove && row.name.eq_ignore_ascii_case(&candidate.login)
                })
            })
            .cloned()
            .collect()
    }

    /// Whether `login` is asked and can still be taken off, which the picker shows as a tick.
    pub fn is_requested(&self, login: &str) -> bool {
        self.rows
            .iter()
            .any(|row| row.can_remove && row.name.eq_ignore_ascii_case(login))
    }

    /// Shows fixed picker candidates without a server (previews, tests).
    pub fn set_preview_candidates(&mut self, candidates: Vec<ReviewerCandidate>) {
        self.candidates = candidates;
        self.candidates_loaded = true;
    }

    /// Opens the reviewer picker under the Review header's pencil.
    /// Takes the entity, not `&self`: building the picker reads the section, which can't be mid-update.
    pub fn open_reviewer_picker(this: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let menu = this.read(cx).picker_menu.clone();
        menu.show(window, cx);
    }

    pub fn close_reviewer_picker(&self, cx: &mut App) {
        self.picker_menu.hide(cx);
    }

    /// Requests `login`, or takes the request back when they're already asked (Orca's picker toggle).
    pub fn toggle_reviewer(&mut self, login: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_requested(&login) {
            self.remove_reviewer(login, window, cx);
        } else {
            self.request_reviewers(vec![login], window, cx);
        }
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

    /// Uploads pasted or dropped files into the summary at its cursor.
    pub fn upload_to_summary(
        &mut self,
        sources: Vec<UploadSource>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        let weak = cx.entity().downgrade();
        let on_error: Rc<dyn Fn(String, &mut Window, &mut App)> =
            Rc::new(move |error, _window, cx| {
                weak.update(cx, |section, cx| {
                    section.error = Some(error);
                    cx.notify();
                })
                .log_err();
            });
        for source in sources {
            upload_into(
                self.summary_editor.clone(),
                source,
                self.workspace.clone(),
                self.client.clone(),
                on_error.clone(),
                window,
                cx,
            );
        }
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
            |_, _, _| {},
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
        let display = row.name.clone();
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
        let warning = cx.theme().status().warning;
        h_flex()
            .h(px(28.))
            .gap(px(8.))
            .child(render_avatar(
                row.avatar_url.as_deref(),
                &row.name,
                if row.team {
                    AvatarKind::Team
                } else {
                    AvatarKind::Person
                },
                REVIEWER_SLOT,
                cx,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(display).size(LabelSize::Small).truncate()),
            )
            .child(
                h_flex()
                    .flex_none()
                    .when(row.can_rerequest, |actions| {
                        actions.child(
                            IconButton::new(
                                SharedString::from(format!("rerequest-{name}")),
                                IconName::RotateCw,
                            )
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .disabled(blocked || self.busy.is_some())
                            .tooltip(Tooltip::text(rerequest_tooltip))
                            .on_click(cx.listener(
                                move |this, _: &ClickEvent, window, cx| {
                                    this.request_reviewers(vec![name.clone()], window, cx)
                                },
                            )),
                        )
                    })
                    .when(row.can_remove, |actions| {
                        actions.child(
                            IconButton::new(
                                SharedString::from(format!("remove-{remove_name}")),
                                IconName::Close,
                            )
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .disabled(self.busy.is_some())
                            .tooltip(Tooltip::text(format!("Remove {remove_name}")))
                            .on_click(cx.listener(
                                move |this, _: &ClickEvent, window, cx| {
                                    this.remove_reviewer(remove_name.clone(), window, cx)
                                },
                            )),
                        )
                    }),
            )
            .child(
                h_flex()
                    .id(SharedString::from(format!("state-{}", row.name)))
                    .size(REVIEWER_SLOT)
                    .flex_none()
                    .justify_center()
                    .tooltip(Tooltip::text(state_label))
                    .child(if dot {
                        div()
                            .size(px(8.))
                            .rounded_full()
                            .bg(warning)
                            .into_any_element()
                    } else {
                        Icon::new(icon)
                            .size(IconSize::Small)
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
                            ButtonLike::new("icarus-review-base-trigger")
                                .style(ButtonStyle::Outlined)
                                .size(ButtonSize::Medium)
                                .full_width()
                                .child(
                                    h_flex()
                                        .w_full()
                                        .min_w_0()
                                        .px(px(4.))
                                        .gap(px(8.))
                                        .justify_between()
                                        .text_left()
                                        .child(
                                            div().flex_1().min_w_0().child(
                                                Label::new(current).single_line().truncate(),
                                            ),
                                        )
                                        .child(
                                            Icon::new(IconName::ChevronDown)
                                                .size(IconSize::Medium)
                                                .color(Color::Muted),
                                        ),
                                )
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

/// Orca's `parseGitHubReviewerInputLogins`: logins or `org/team`s split on commas and spaces, `@` dropped.
pub fn typed_reviewers(input: &str) -> Vec<String> {
    input
        .split(|character: char| character == ',' || character.is_whitespace())
        .map(|login| login.trim().trim_start_matches('@'))
        .filter(|login| !login.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Orca's reviewer picker: a search field over the people who can review, suggestions first.
/// Picking someone asks them for a review and ticks them; picking a ticked one takes it back. It
/// stays open for more, and Enter on text that matches nobody requests what was typed (a team too).
pub struct ReviewerPickerDelegate {
    section: WeakEntity<ReviewSection>,
    candidates: Vec<ReviewerCandidate>,
    matches: Vec<fuzzy::StringMatch>,
    selected_index: usize,
    query: String,
    loaded: bool,
    error: Option<String>,
    _observe: Option<Subscription>,
}

impl ReviewerPickerDelegate {
    fn all_matches(&self) -> Vec<fuzzy::StringMatch> {
        self.candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| fuzzy::StringMatch {
                candidate_id: index,
                score: 0.0,
                positions: Vec::new(),
                string: candidate.login.clone(),
            })
            .collect()
    }

    fn refresh_candidates(&mut self, cx: &App) {
        if let Some(section) = self.section.upgrade() {
            let section = section.read(cx);
            self.candidates = section.picker_candidates();
            self.loaded = section.candidates_loaded;
            self.error = section.candidates_error.clone();
        }
    }

    /// The visible logins in order, for tests.
    pub fn visible(&self) -> Vec<String> {
        self.matches
            .iter()
            .filter_map(|found| self.candidates.get(found.candidate_id))
            .map(|candidate| candidate.login.clone())
            .collect()
    }
}

pub fn reviewer_picker(
    section: WeakEntity<ReviewSection>,
    window: &mut Window,
    cx: &mut Context<Picker<ReviewerPickerDelegate>>,
) -> Picker<ReviewerPickerDelegate> {
    let mut delegate = ReviewerPickerDelegate {
        section: section.clone(),
        candidates: Vec::new(),
        matches: Vec::new(),
        selected_index: 0,
        query: String::new(),
        loaded: false,
        error: None,
        _observe: None,
    };
    delegate.refresh_candidates(cx);
    delegate.matches = delegate.all_matches();
    if let Some(section) = section.upgrade() {
        delegate._observe = Some(cx.observe_in(&section, window, |picker, _, window, cx| {
            let query = picker.delegate.query.clone();
            picker.delegate.refresh_candidates(cx);
            picker.update_matches(query, window, cx);
        }));
    }
    Picker::list(delegate, window, cx)
        .initial_width(rems_from_px(330_f32))
        .max_height(rems_from_px(420_f32))
        .popover()
}

impl PickerDelegate for ReviewerPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "icarus reviewer picker"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Type or choose a user".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(if !self.loaded {
            "Loading…".into()
        } else if let Some(error) = &self.error {
            error.clone().into()
        } else {
            "No matching reviewers.".into()
        })
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query.clone();
        let query = query.trim().trim_start_matches('@').to_owned();
        if query.is_empty() {
            self.matches = self.all_matches();
            self.selected_index = self
                .selected_index
                .min(self.matches.len().saturating_sub(1));
            return Task::ready(());
        }
        let candidates: Vec<fuzzy::StringMatchCandidate> = self
            .candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let haystack = match &candidate.name {
                    Some(name) => format!("{} {name}", candidate.login),
                    None => candidate.login.clone(),
                };
                fuzzy::StringMatchCandidate::new(index, &haystack)
            })
            .collect();
        let background = cx.background_executor().clone();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = fuzzy::match_strings(
                &candidates,
                &query,
                false,
                true,
                100,
                &Default::default(),
                background,
            )
            .await;
            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = 0;
                    cx.notify();
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let picked = self
            .matches
            .get(self.selected_index)
            .and_then(|found| self.candidates.get(found.candidate_id))
            .map(|candidate| candidate.login.clone());
        let section = self.section.clone();
        match picked {
            Some(login) => section
                .update(cx, |section, cx| section.toggle_reviewer(login, window, cx))
                .log_err(),
            None => {
                let typed = typed_reviewers(&self.query);
                if typed.is_empty() {
                    return;
                }
                section
                    .update(cx, |section, cx| {
                        section.request_reviewers(typed, window, cx)
                    })
                    .log_err()
            }
        };
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(gpui::DismissEvent);
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let found = self.matches.get(index)?;
        let candidate = self.candidates.get(found.candidate_id)?;
        let requested = self
            .section
            .upgrade()
            .is_some_and(|section| section.read(cx).is_requested(&candidate.login));
        let previous = index
            .checked_sub(1)
            .and_then(|previous| self.matches.get(previous))
            .and_then(|found| self.candidates.get(found.candidate_id));
        let any_suggested = self.candidates.iter().any(|candidate| candidate.suggested);
        let heading = if !self.query.trim().is_empty() || !any_suggested {
            None
        } else if previous.is_none() && candidate.suggested {
            Some("Suggestions")
        } else if !candidate.suggested && previous.is_none_or(|previous| previous.suggested) {
            Some("Everyone else")
        } else {
            None
        };
        let colors = cx.theme().colors();
        let row = h_flex()
            .id(("icarus-reviewer-option", index))
            .w_full()
            .min_h(px(40.))
            .px(px(12.))
            .py(px(8.))
            .gap(px(8.))
            .border_b_1()
            .border_color(colors.border_variant)
            .when(selected, |row| row.bg(colors.element_hover))
            .child(
                h_flex()
                    .size(px(16.))
                    .flex_none()
                    .justify_center()
                    .when(requested, |slot| {
                        slot.child(Icon::new(IconName::Check).size(IconSize::Small))
                    }),
            )
            .child(render_avatar(
                candidate.avatar_url.as_deref(),
                &candidate.login,
                AvatarKind::Person,
                REVIEWER_SLOT,
                cx,
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_1()
                            .child(
                                Label::new(candidate.login.clone())
                                    .size(LabelSize::Custom(rems_from_px(13_f32)))
                                    .weight(gpui::FontWeight::SEMIBOLD)
                                    .single_line(),
                            )
                            .when_some(candidate.name.clone(), |line, name| {
                                line.child(
                                    Label::new(name)
                                        .size(LabelSize::Custom(rems_from_px(13_f32)))
                                        .color(Color::Muted)
                                        .single_line()
                                        .truncate(),
                                )
                            }),
                    )
                    .when(candidate.suggested, |column| {
                        column.child(
                            Label::new("Recently edited these files")
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .single_line()
                                .truncate(),
                        )
                    }),
            );
        Some(
            v_flex()
                .w_full()
                .when_some(heading, |column, heading| {
                    column.child(
                        div()
                            .w_full()
                            .px(px(12.))
                            .py(px(6.))
                            .border_b_1()
                            .border_color(colors.border_variant)
                            .bg(colors.element_background)
                            .child(
                                Label::new(heading)
                                    .size(LabelSize::Small)
                                    .weight(gpui::FontWeight::SEMIBOLD),
                            ),
                    )
                })
                .child(row)
                .into_any_element(),
        )
    }
}

/// The Review header's pencil, which opens the reviewer picker under it.
pub fn reviewer_picker_button(section: &Entity<ReviewSection>, cx: &App) -> AnyElement {
    let busy = section.read(cx).busy.is_some();
    let handle = section.read(cx).picker_menu.clone();
    let weak = section.downgrade();
    PopoverMenu::new("icarus-reviewer-picker")
        .with_handle(handle)
        .trigger(
            IconButton::new("icarus-reviewer-picker-button", IconName::Pencil)
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .disabled(busy)
                .tooltip(Tooltip::text("Reviewer")),
        )
        .anchor(gpui::Anchor::TopRight)
        .offset(gpui::Point {
            x: px(0.),
            y: px(6.),
        })
        .menu(move |window, cx| {
            let weak = weak.clone();
            Some(cx.new(|cx| reviewer_picker(weak, window, cx)))
        })
        .into_any_element()
}

/// The Review section's text: Orca's 14px in the UI font, not the buffer's.
const SUMMARY_TEXT: f32 = 14.;

fn ui_text_style(size: f32, cx: &App) -> gpui::TextStyleRefinement {
    gpui::TextStyleRefinement {
        font_family: Some(theme::theme_settings(cx).ui_font(cx).family.clone()),
        font_size: Some(rems_from_px(size).into()),
        ..Default::default()
    }
}

impl Render for ReviewSection {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.reload_requested) {
            self.reload(window, cx);
        }
        let border = cx.theme().colors().border;
        let drop_bg = cx.theme().colors().drop_target_background;
        let input_background = cx.theme().colors().editor_background;
        let mut section = v_flex()
            .key_context("IcarusReviewSection")
            .debug_selector(|| "review-section".into());
        if !self.rows.is_empty() {
            section = section.child(
                v_flex().px(px(12.)).pt(px(4.)).gap(px(2.)).children(
                    self.rows
                        .clone()
                        .iter()
                        .map(|row| self.render_reviewer(row, cx).into_any_element())
                        .collect::<Vec<_>>(),
                ),
            );
        }
        let Some(view) = self.view.clone() else {
            let problem = self.load_error.clone().or_else(|| self.error.clone());
            return section.child(
                div().px(px(12.)).py(px(8.)).child(
                    Label::new(
                        problem
                            .clone()
                            .unwrap_or_else(|| "Loading review…".to_owned()),
                    )
                    .size(LabelSize::Small)
                    .color(if problem.is_some() {
                        Color::Error
                    } else {
                        Color::Muted
                    }),
                ),
            );
        };
        if let Some(picker) = self.render_base_picker(&view, cx) {
            section = section.child(div().px(px(12.)).pt(px(8.)).child(picker));
        }
        if let Some((label, color, icon)) = standing_pill(view.standing.as_deref()) {
            section = section.child(
                div().px(px(12.)).pt(px(8.)).child(
                    h_flex()
                        .gap(px(6.))
                        .px(px(8.))
                        .py(px(4.))
                        .rounded_md()
                        .border_1()
                        .border_color(border)
                        .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                        .child(
                            Label::new(label)
                                .size(LabelSize::Custom(rems_from_px(11_f32)))
                                .weight(gpui::FontWeight::MEDIUM)
                                .color(color),
                        ),
                ),
            );
        }
        let drafts = view.drafts.len();
        let mut buttons = h_flex()
            .flex_wrap()
            .items_center()
            .justify_end()
            .gap(px(2.))
            .px(px(6.))
            .pb(px(6.));
        if drafts > 0 {
            buttons = buttons.child(
                div().pr_1().child(
                    Label::new(format!(
                        "{drafts} comment{} queued",
                        if drafts == 1 { "" } else { "s" }
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Warning),
                ),
            );
        }
        for verdict in view.verdicts.clone() {
            let enabled = view.can_submit.get(&verdict).copied().unwrap_or(false);
            let sending = self.busy.as_deref() == Some(verdict.as_str());
            let (icon, color) = match verdict.as_str() {
                "approve" => (IconName::Check, Color::Success),
                "request-changes" => (IconName::Diff, Color::Error),
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
                    .size(ButtonSize::Default)
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .start_icon(Icon::new(icon).size(IconSize::XSmall).color(color))
                    .disabled(!enabled || self.busy.is_some())
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.submit(&verdict, window, cx)
                    })),
            );
        }
        let (_, summary_media) = crate::media::split_media(&self.summary_editor.read(cx).text(cx));
        let media = crate::media::render_media_strip(
            "icarus-review-summary",
            &summary_media,
            &summary_media,
            cx,
        );
        section = section.child(
            div().px(px(12.)).pt(px(8.)).pb(px(8.)).child(
                v_flex()
                    .rounded_md()
                    .border_1()
                    .border_color(border)
                    .bg(input_background)
                    .capture_action(cx.listener(|this, _: &editor::actions::Paste, window, cx| {
                        let images = clipboard_images(cx);
                        if images.is_empty() {
                            cx.propagate();
                            return;
                        }
                        this.upload_to_summary(images, window, cx);
                    }))
                    .drag_over::<gpui::ExternalPaths>(move |style, _, _, _| style.bg(drop_bg))
                    .on_drop(
                        cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| {
                            let sources = paths
                                .paths()
                                .iter()
                                .cloned()
                                .map(UploadSource::Path)
                                .collect();
                            this.upload_to_summary(sources, window, cx);
                        }),
                    )
                    .child(
                        div()
                            .px(px(12.))
                            .pt(px(8.))
                            .pb(px(4.))
                            .child(self.summary_editor.clone()),
                    )
                    .children(media)
                    .child(buttons),
            ),
        );
        for error in [self.error.clone(), self.load_error.clone()]
            .into_iter()
            .flatten()
        {
            section = section.child(
                div()
                    .px(px(12.))
                    .pb(px(8.))
                    .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
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
    /// One of your comments already on GitHub, by node id.
    EditPublished {
        comment: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    /// A queued draft for the next review.
    Review,
    /// A note on these lines (`note.add`), which the agent side of Icarus reads and answers.
    Agent,
    /// Posted to GitHub straight away, outside the pending review ("Comment now").
    Now,
}

/// What a gutter marker stands for; each kind has its own icon and colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    Thread,
    Draft,
    Note,
}

impl MarkerKind {
    fn icon(self) -> IconName {
        match self {
            Self::Thread => IconName::Chat,
            Self::Draft => IconName::Pencil,
            Self::Note => IconName::ZedAgent,
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Thread => Color::Accent,
            Self::Draft => Color::Warning,
            Self::Note => Color::Created,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Thread => "a PR thread",
            Self::Draft => "your pending comment",
            Self::Note => "a note",
        }
    }
}

/// "Show a PR thread, your pending comment and a note".
fn marker_tooltip(kinds: &[MarkerKind]) -> String {
    let names: Vec<&str> = kinds.iter().map(|kind| kind.name()).collect();
    let listed = match names.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    format!("Show {listed}")
}

struct ThreadGutter;
struct DraftGutter;
struct NoteGutter;

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
    /// The open comment box and its own block; a reply has none, it sits inside its thread's card.
    composer: Option<(Entity<CommentComposer>, Option<CustomBlockId>)>,
    /// The card holding an inline reply box: its block and its height without the box.
    composer_host: Option<(CustomBlockId, u32)>,
    /// The thread or note whose card holds the open reply box.
    inline_reply: Option<String>,
    blocks: Vec<CustomBlockId>,
    /// Each block's card key and line, in block order: an unchanged layout keeps its blocks.
    block_keys: Vec<(String, Option<u32>)>,
    block_heights: Vec<u32>,
    /// Card views by key, kept across refreshes so their rendered text never blanks.
    cards: HashMap<String, Entity<ReviewCard>>,
    markdown: MarkdownCache,
    store: Entity<PrReviewStore>,
    _store_subscriptions: [Subscription; 2],
    /// The last action's failure, kept across the reload that follows it.
    error: Option<String>,
    load_error: Option<String>,
    /// A plain file editor: its comment box leaves notes, not review drafts.
    notes_only: bool,
    people: MentionSource,
    people_requested: bool,
    /// Multibuffer row → each kind on it (threads, drafts, notes) with the first card of that kind.
    markers: Rc<RefCell<HashMap<u32, Vec<(MarkerKind, String)>>>>,
    _poll: Task<()>,
}

impl DiffReview {
    pub fn new(
        editor: WeakEntity<Editor>,
        buffer: Entity<Buffer>,
        workspace_key: String,
        path: String,
        client: Option<Arc<Client>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(editor, buffer, workspace_key, path, client, false, cx)
    }

    /// A plain file editor of an Icarus workspace: PR threads, drafts and notes on its lines.
    pub fn file_editor(
        editor: WeakEntity<Editor>,
        buffer: Entity<Buffer>,
        workspace_key: String,
        path: String,
        client: Option<Arc<Client>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(editor, buffer, workspace_key, path, client, true, cx)
    }

    fn build(
        editor: WeakEntity<Editor>,
        buffer: Entity<Buffer>,
        workspace_key: String,
        path: String,
        client: Option<Arc<Client>>,
        notes_only: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = PrReviewStore::global(cx);
        let store_subscriptions = [
            cx.observe(&store, |this, _, cx| this.sync_from_store(cx)),
            cx.subscribe(&store, |this, _, failed: &PrReviewFailed, cx| {
                if failed.workspace == this.workspace_key {
                    this.error = Some(failed.message.clone());
                    cx.notify();
                }
            }),
        ];
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
            composer_host: None,
            inline_reply: None,
            blocks: Vec::new(),
            block_keys: Vec::new(),
            block_heights: Vec::new(),
            cards: HashMap::default(),
            markdown: MarkdownCache::default(),
            store,
            _store_subscriptions: store_subscriptions,
            error: None,
            load_error: None,
            notes_only,
            people: MentionSource::default(),
            people_requested: false,
            markers: Rc::default(),
            _poll: Task::ready(()),
        };
        review.install_gutter_hook(cx);
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
        self.sync_from_store(cx);
        let workspace = self.workspace_key.clone();
        let client = self.client.clone();
        self.store
            .update(cx, |store, cx| store.load(&workspace, client, false, cx));
        cx.spawn(async move |this, cx| {
            let Some(notes) = notes else {
                return;
            };
            let notes = notes.await;
            this.update(cx, |this, cx| match notes {
                Ok(notes) => {
                    let notes: Vec<WorkspaceNote> =
                        serde_json::from_value(notes).unwrap_or_default();
                    if notes != this.notes {
                        this.notes = notes;
                        this.rebuild_blocks(cx);
                    }
                }
                Err(error) => log::warn!("loading notes failed: {error:#}"),
            })
            .log_err();
        })
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load(cx).detach();
    }

    /// Takes this file's threads and drafts from the shared store; unchanged data touches nothing.
    fn sync_from_store(&mut self, cx: &mut Context<Self>) {
        let Some(data) = self.store.read(cx).data(&self.workspace_key) else {
            return;
        };
        let threads: Vec<ReviewThread> = data
            .threads
            .iter()
            .filter(|thread| thread.path == self.path)
            .cloned()
            .collect();
        let drafts: Vec<ReviewDraft> = data
            .drafts
            .iter()
            .filter(|draft| draft.path == self.path)
            .cloned()
            .collect();
        let load_error = data.load_error.clone();
        if load_error != self.load_error {
            self.load_error = load_error;
            cx.notify();
        }
        if threads != self.threads || drafts != self.drafts {
            self.threads = threads;
            self.drafts = drafts;
            self.rebuild_blocks(cx);
        }
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
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let mut markers = HashMap::default();
        let mut highlights: [Vec<std::ops::Range<editor::Anchor>>; 3] = Default::default();
        let reply_host = self.inline_reply_host().map(str::to_owned);
        let reply_height = self
            .composer
            .as_ref()
            .map_or(0, |(composer, _)| composer.read(cx).height(cx));
        let mut host_index = None;
        let mut keys = Vec::new();
        let mut heights = Vec::new();
        let mut views = HashMap::default();
        for (line, _, height, card) in cards {
            let Some(anchor) = self.anchor_for_line(line, cx) else {
                continue;
            };
            let key = card.key();
            if line.is_some() {
                let (kind, id, slot) = match &card {
                    AnyCard::Thread(thread, _) => (MarkerKind::Thread, thread.id.clone(), 0),
                    AnyCard::Draft(draft) => (MarkerKind::Draft, draft.id.clone(), 1),
                    AnyCard::Note(note, _) => (MarkerKind::Note, note.id.clone(), 2),
                };
                let row = multi_buffer::ToPoint::to_point(&anchor, &snapshot).row;
                let kinds: &mut Vec<(MarkerKind, String)> = markers.entry(row).or_default();
                if !kinds.iter().any(|(known, _)| *known == kind) {
                    kinds.push((kind, id));
                    kinds.sort_by_key(|(kind, _)| *kind as u8);
                }
                highlights[slot].push(anchor..anchor);
            }
            let hosts_reply = reply_host.as_deref().is_some_and(|host| match &card {
                AnyCard::Thread(thread, _) => thread.id == host,
                AnyCard::Note(note, _) => note.id == host,
                AnyCard::Draft(_) => false,
            });
            let mut height = height;
            if hosts_reply {
                host_index = Some((blocks.len(), height));
                height += reply_height;
            }
            let view = match self.cards.remove(&key) {
                Some(view) => {
                    view.update(cx, |view, cx| view.set_card(card, cx));
                    view
                }
                None => {
                    let markdown = self.markdown.clone();
                    cx.new(|_| ReviewCard::new(card, this.clone(), markdown))
                }
            };
            views.insert(key.clone(), view.clone());
            keys.push((key, line));
            heights.push(height);
            blocks.push(BlockProperties {
                placement: BlockPlacement::Below(anchor),
                height: Some(height),
                style: BlockStyle::Flex,
                render: card_renderer(view),
                priority: 1,
            });
        }
        self.cards = views;
        *self.markers.borrow_mut() = markers;
        // The same cards on the same lines keep their blocks; only changed heights are resized,
        // so a refresh never takes a card out of the editor and puts it back.
        if keys == self.block_keys && self.blocks.len() == keys.len() {
            let resized: HashMap<CustomBlockId, u32> = self
                .blocks
                .iter()
                .zip(heights.iter().zip(&self.block_heights))
                .filter(|(_, (new, old))| new != old)
                .map(|(id, (new, _))| (*id, *new))
                .collect();
            if !resized.is_empty() {
                editor.update(cx, |editor, cx| editor.resize_blocks(resized, None, cx));
            }
            self.block_heights = heights;
            self.composer_host =
                host_index.and_then(|(index, base)| self.blocks.get(index).map(|id| (*id, base)));
            cx.notify();
            return;
        }
        let [threads, drafts, notes] = highlights;
        let old: HashSet<CustomBlockId> = self.blocks.drain(..).collect();
        let ids = editor.update(cx, |editor, cx| {
            editor.remove_blocks(old, None, cx);
            editor.highlight_gutter::<ThreadGutter>(threads, |cx| cx.theme().status().info, cx);
            editor.highlight_gutter::<DraftGutter>(drafts, |cx| cx.theme().status().warning, cx);
            editor.highlight_gutter::<NoteGutter>(notes, |cx| cx.theme().status().created, cx);
            editor.insert_blocks(blocks, None, cx)
        });
        self.composer_host =
            host_index.and_then(|(index, base)| ids.get(index).map(|id| (*id, base)));
        self.blocks = ids;
        self.block_keys = keys;
        self.block_heights = heights;
        cx.notify();
    }

    #[cfg(test)]
    pub fn block_ids(&self) -> &[CustomBlockId] {
        &self.blocks
    }

    /// The thread or note whose card holds the open reply box.
    fn inline_reply_host(&self) -> Option<&str> {
        self.composer.as_ref()?;
        self.inline_reply.as_deref()
    }

    /// The reply box to draw at the bottom of the card for `id`, if it has one open.
    pub fn inline_composer_for(&self, id: &str) -> Option<Entity<CommentComposer>> {
        (self.inline_reply_host() == Some(id))
            .then(|| self.composer.as_ref().map(|(composer, _)| composer.clone()))
            .flatten()
    }

    pub fn is_expanded(&self, id: &str) -> bool {
        self.expanded.contains(id)
    }

    /// What a click on a multibuffer row's marker opens: its first kind (threads, drafts, notes).
    pub fn marker_at(&self, row: u32) -> Option<(MarkerKind, String)> {
        self.markers.borrow().get(&row)?.first().cloned()
    }

    /// Every kind of card on a multibuffer row, each drawn as its own icon.
    pub fn marker_kinds_at(&self, row: u32) -> Vec<MarkerKind> {
        self.markers
            .borrow()
            .get(&row)
            .map(|kinds| kinds.iter().map(|(kind, _)| *kind).collect())
            .unwrap_or_default()
    }

    /// Gives the editor's gutter "+" (click or drag over rows) and the markers to this review.
    fn install_gutter_hook(&self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let review = cx.entity().downgrade();
        let markers = self.markers.clone();
        let on_rows_review = review.clone();
        let hook = editor::GutterCommentHook {
            on_rows: Rc::new(move |window, cx| {
                on_rows_review
                    .update(cx, |review, cx| review.open_on_selection(window, cx))
                    .log_err();
            }),
            marker: Rc::new(move |row, _cx| {
                let markers = markers.borrow();
                let kinds: Vec<MarkerKind> =
                    markers.get(&row.0)?.iter().map(|(kind, _)| *kind).collect();
                Some(editor::GutterCommentMarker {
                    icons: kinds
                        .iter()
                        .map(|kind| (kind.icon(), kind.color()))
                        .collect(),
                    tooltip: marker_tooltip(&kinds).into(),
                })
            }),
            on_marker: Rc::new(move |row, window, cx| {
                review
                    .update(cx, |review, cx| review.reveal_marker(row.0, window, cx))
                    .log_err();
            }),
        };
        // Deferred: file editors create their review while they're mid-update themselves.
        cx.defer(move |cx| {
            editor.update(cx, |editor, cx| {
                editor.set_gutter_comment_hook(Some(hook), cx)
            });
        });
    }

    /// Expands the marked card if it is collapsed and scrolls the editor to its line.
    pub fn reveal_marker(&mut self, row: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, id)) = self.marker_at(row) else {
            return;
        };
        let collapsed = self
            .threads
            .iter()
            .any(|thread| thread.id == id && thread.is_resolved)
            || self.notes.iter().any(|note| note.id == id && note.resolved);
        if collapsed && !self.expanded.contains(&id) {
            self.expanded.insert(id);
            self.rebuild_blocks(cx);
        }
        if let Some(editor) = self.editor.upgrade() {
            editor.update(cx, |editor, cx| {
                let point = Point::new(row, 0);
                editor.change_selections(
                    editor::SelectionEffects::scroll(editor::scroll::Autoscroll::center()),
                    window,
                    cx,
                    |selections| selections.select_ranges([point..point]),
                );
            });
        }
    }

    /// The people @-mentions offer; asked for once per tab, when a comment box first opens.
    fn mention_source(&mut self, cx: &mut Context<Self>) -> MentionSource {
        if !self.people_requested {
            self.people_requested = true;
            let people = self.people.clone();
            if let Some(call) = self.call(
                "review.mentionables",
                json!({ "workspace": self.workspace_key }),
                cx,
            ) {
                cx.spawn(async move |_, _| match call.await {
                    Ok(value) => people.set(mentionables_from(&value)),
                    Err(error) => log::warn!("loading people to mention failed: {error:#}"),
                })
                .detach();
            }
        }
        self.people.clone()
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
            ComposerTarget::EditPublished { comment } => self
                .threads
                .iter()
                .find(|thread| thread.comments.iter().any(|c| &c.id == comment))
                .and_then(|thread| thread.placement.line),
        };
        let inline = match &target {
            ComposerTarget::Reply { thread } => self
                .threads
                .iter()
                .find(|candidate| &candidate.id == thread)
                .map(|thread| {
                    let author = thread.comments.first().and_then(|c| c.author.clone());
                    (thread.id.clone(), author)
                }),
            ComposerTarget::NoteReply { note } => self
                .notes
                .iter()
                .any(|candidate| &candidate.id == note)
                .then(|| (note.clone(), None)),
            _ => None,
        };
        let review = cx.entity().downgrade();
        let people = self.mention_source(cx);
        let notes_only = self.notes_only;
        if let Some((host, author)) = inline {
            let composer = cx.new(|cx| {
                CommentComposer::new(target, text, review, people, notes_only, window, cx)
            });
            if let Some(author) = author {
                composer.update(cx, |composer, cx| {
                    composer.editor.update(cx, |editor, cx| {
                        editor.set_placeholder_text(&format!("Reply to {author}"), window, cx)
                    })
                });
            }
            self.composer = Some((composer.clone(), None));
            self.inline_reply = Some(host.clone());
            self.expanded.insert(host);
            self.rebuild_blocks(cx);
            let focus = composer.read(cx).editor.focus_handle(cx);
            window.focus(&focus, cx);
            return;
        }
        let Some(anchor) = self.anchor_for_line(line, cx) else {
            return;
        };
        let composer =
            cx.new(|cx| CommentComposer::new(target, text, review, people, notes_only, window, cx));
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
            self.composer = Some((composer.clone(), Some(*id)));
        }
        let focus = composer.read(cx).editor.focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// The composer passes its own height: it is mid-update when its text changes.
    fn resize_composer(&mut self, height: u32, cx: &mut Context<Self>) {
        let (Some((_, block)), Some(editor)) = (&self.composer, self.editor.upgrade()) else {
            return;
        };
        let (id, height) = match (block, self.composer_host) {
            (Some(id), _) => (*id, height),
            (None, Some((host, base))) => (host, base + height),
            (None, None) => return,
        };
        editor.update(cx, |editor, cx| {
            editor.resize_blocks(HashMap::from_iter([(id, height)]), None, cx)
        });
    }

    pub fn close_composer(&mut self, cx: &mut Context<Self>) {
        match (self.composer.take(), self.editor.upgrade()) {
            (Some((_, Some(id))), Some(editor)) => {
                editor.update(cx, |editor, cx| {
                    editor.remove_blocks(HashSet::from_iter([id]), None, cx)
                });
            }
            (Some((_, None)), _) => {
                self.inline_reply = None;
                self.composer_host = None;
                self.rebuild_blocks(cx);
            }
            _ => {}
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
        if let Some(op) = self.review_op(&target, destination, &body, cx) {
            // Shown at once in every view; a refusal brings the box back with the text and why.
            self.close_composer(cx);
            let Some(task) = self.change(op, cx) else {
                return;
            };
            cx.spawn_in(window, async move |this, cx| {
                if let Err(error) = task.await {
                    this.update_in(cx, |this, window, cx| {
                        this.open_composer(target, body, window, cx);
                        if let Some(composer) = this.composer() {
                            composer.update(cx, |composer, cx| {
                                composer.error = Some(format!("{error:#}"));
                                cx.notify();
                            });
                        }
                    })
                    .log_err();
                }
            })
            .detach();
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
            _ => return,
        };
        let new_note = method == "note.add";
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
                Ok(added) => {
                    this.close_composer(cx);
                    // A new note goes into the agent's composer at once, so the prompt can follow.
                    match added["id"].as_str().filter(|_| new_note) {
                        Some(note) => {
                            if let Ok(created) =
                                serde_json::from_value::<WorkspaceNote>(added.clone())
                                && created.path == this.path
                            {
                                this.notes.retain(|existing| existing.id != created.id);
                                this.notes.push(created);
                                this.rebuild_blocks(cx);
                            }
                            this.add_note_to_agent(note, cx)
                        }
                        None => this.reload(cx),
                    }
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

    /// Applies a review change to the shared store, which every view of this PR draws at once.
    fn change(&mut self, op: ReviewOp, cx: &mut Context<Self>) -> Option<Task<Result<Value>>> {
        self.error = None;
        let workspace = self.workspace_key.clone();
        let client = self.client.clone();
        self.store
            .update(cx, |store, cx| store.mutate(&workspace, client, op, cx))
    }

    /// The store change a review composer sends, built with the viewer as its author.
    fn review_op(
        &mut self,
        target: &ComposerTarget,
        destination: Destination,
        body: &str,
        cx: &mut Context<Self>,
    ) -> Option<ReviewOp> {
        let viewer = self
            .store
            .read(cx)
            .data(&self.workspace_key)
            .and_then(PrReviewData::viewer)
            .unwrap_or_default();
        let comment = |id: String| ThreadComment {
            id,
            author: (!viewer.login.is_empty()).then(|| viewer.login.clone()),
            author_avatar_url: viewer.avatar_url.clone(),
            body: body.to_owned(),
            created_at: crate::pr_store::now_rfc3339(),
            ..Default::default()
        };
        let range = |start: u32, end: u32| (end, (start != end).then_some(start));
        Some(match (target, destination) {
            (ComposerTarget::Lines { start, end }, Destination::Review) => {
                let (line, start_line) = range(*start, *end);
                let id = self.store.update(cx, |store, _| store.pending_id());
                ReviewOp::NewDraft {
                    draft: ReviewDraft {
                        id,
                        path: self.path.clone(),
                        line: Some(line),
                        start_line,
                        subject: "line".into(),
                        body: body.to_owned(),
                    },
                }
            }
            (ComposerTarget::Lines { start, end }, Destination::Now) => {
                let (line, start_line) = range(*start, *end);
                let (thread_id, comment_id) = self
                    .store
                    .update(cx, |store, _| (store.pending_id(), store.pending_id()));
                let label = match start_line {
                    Some(start) => format!("Lines {start}-{line}"),
                    None => format!("Line {line}"),
                };
                ReviewOp::NewThread {
                    thread: ReviewThread {
                        id: thread_id,
                        path: self.path.clone(),
                        line: Some(line),
                        original_line: Some(line),
                        comments: vec![comment(comment_id)],
                        placement: ThreadPlacement {
                            line: Some(line),
                            label,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    start_line,
                }
            }
            (ComposerTarget::Reply { thread }, _) => {
                let id = self.store.update(cx, |store, _| store.pending_id());
                ReviewOp::Reply {
                    thread: thread.clone(),
                    comment: comment(id),
                }
            }
            (ComposerTarget::EditDraft { id }, _) => ReviewOp::EditDraft {
                id: id.clone(),
                body: body.to_owned(),
            },
            (ComposerTarget::EditPublished { comment }, _) => ReviewOp::EditComment {
                id: comment.clone(),
                body: body.to_owned(),
                published: json!({ "kind": "review_comment", "node_id": comment }),
            },
            _ => return None,
        })
    }

    pub fn remove_draft(&mut self, id: &str, cx: &mut Context<Self>) {
        self.change(ReviewOp::RemoveDraft { id: id.to_owned() }, cx);
    }

    pub fn set_resolved(&mut self, thread: &str, resolved: bool, cx: &mut Context<Self>) {
        let thread = thread.to_owned();
        self.change(ReviewOp::Resolve { thread, resolved }, cx);
    }

    /// Adds or removes one of GitHub's eight reactions (`+1`, `heart`, …) on a comment.
    pub fn toggle_reaction(
        &mut self,
        comment: &str,
        reaction: &str,
        reacted: bool,
        cx: &mut Context<Self>,
    ) {
        let op = ReviewOp::React {
            subject: comment.to_owned(),
            reaction: reaction.to_owned(),
            react: !reacted,
        };
        self.change(op, cx);
    }

    /// Deletes one of your published review comments, after asking.
    pub fn delete_published(&mut self, comment: &str, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            "Delete this comment on GitHub?",
            Some("It's removed from the pull request for everyone."),
            &["Delete", "Cancel"],
            cx,
        );
        let comment = comment.to_owned();
        cx.spawn(async move |this, cx| {
            if answer.await.ok() != Some(0) {
                return;
            }
            this.update(cx, |this, cx| {
                let published = json!({ "kind": "review_comment", "node_id": comment });
                this.change(
                    ReviewOp::DeleteComment {
                        id: comment,
                        published,
                    },
                    cx,
                );
            })
            .log_err();
        })
        .detach();
    }

    /// Opens a reply session's transcript: wakes it from history and shows it as an agent tab.
    pub fn open_transcript(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        let Some(call) = self.call("agent.resume", json!({ "id": agent_id }), cx) else {
            return;
        };
        let agent_id = agent_id.to_owned();
        let path = PathBuf::from(&self.workspace_key);
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| match result {
                Ok(_) => crate::history::open_resumed(agent_id, Some(path), cx),
                Err(error) => {
                    this.error = Some(format!("{error:#}"));
                    cx.notify();
                }
            })
            .log_err();
        })
        .detach();
    }

    /// Drops the thread into this workspace's composer as an attachment chip (§10).
    pub fn add_to_agent(&mut self, thread: &str, cx: &mut Context<Self>) {
        if crate::pr_store::send_thread_to_agent(&self.workspace_key, thread, cx).is_none() {
            log::warn!("no thread {thread} to send to the agent");
        }
    }

    pub fn remove_note(&mut self, note: &str, reply: Option<&str>, cx: &mut Context<Self>) {
        // Gone at once; a failed removal reloads and brings it back with the error.
        match reply {
            Some(reply) => {
                if let Some(owner) = self.notes.iter_mut().find(|n| n.id == note) {
                    owner.replies.retain(|r| r.id != reply);
                }
            }
            None => self.notes.retain(|n| n.id != note),
        }
        self.rebuild_blocks(cx);
        self.mutate("note.remove", json!({ "id": note, "reply_id": reply }), cx);
    }

    pub fn set_note_resolved(&mut self, note: &str, resolved: bool, cx: &mut Context<Self>) {
        let method = if resolved {
            "note.resolve"
        } else {
            "note.unresolve"
        };
        if let Some(found) = self.notes.iter_mut().find(|n| n.id == note) {
            found.resolved = resolved;
            self.rebuild_blocks(cx);
        }
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
        self.upload_to_composer(UploadSource::Path(path), window, cx);
    }

    /// Uploads a picked, pasted or dropped file into the open composer at its cursor.
    pub fn upload_to_composer(
        &mut self,
        source: UploadSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((composer, _)) = self.composer.clone() else {
            return;
        };
        let editor = composer.read(cx).editor.clone();
        let weak = composer.downgrade();
        let on_error: Rc<dyn Fn(String, &mut Window, &mut App)> =
            Rc::new(move |error, _window, cx| {
                weak.update(cx, |composer, cx| {
                    composer.error = Some(error);
                    cx.notify();
                })
                .log_err();
            });
        upload_into(
            editor,
            source,
            self.workspace_key.clone(),
            self.client.clone(),
            on_error,
            window,
            cx,
        );
    }
}

impl EventEmitter<()> for DiffReview {}
impl EventEmitter<OpenChangedFile> for DiffReview {}

#[derive(Debug, Clone, PartialEq)]
enum AnyCard {
    Thread(ReviewThread, bool),
    Draft(ReviewDraft),
    Note(WorkspaceNote, bool),
}

impl AnyCard {
    /// A thread is keyed by its first comment, which keeps its id when a posted thread's real id arrives.
    fn key(&self) -> String {
        match self {
            Self::Thread(thread, _) => match thread.comments.first() {
                Some(first) => format!("thread:{}", first.id),
                None => format!("thread:{}", thread.id),
            },
            Self::Draft(draft) => format!("draft:{}", draft.id),
            Self::Note(note, _) => format!("note:{}", note.id),
        }
    }
}

/// Rendered comment bodies by text, shared by every card of a diff so a rebuilt card draws at once.
#[derive(Clone, Default)]
pub(crate) struct MarkdownCache(Rc<RefCell<HashMap<String, Entity<markdown::Markdown>>>>);

impl MarkdownCache {
    const LIMIT: usize = 400;

    pub(crate) fn get(&self, text: &str, cx: &mut App) -> Entity<markdown::Markdown> {
        if let Some(markdown) = self.0.borrow().get(text) {
            return markdown.clone();
        }
        let markdown = cx.new(|cx| comment_markdown(text.to_owned(), cx));
        let mut cache = self.0.borrow_mut();
        if cache.len() >= Self::LIMIT {
            cache.clear();
        }
        cache.insert(text.to_owned(), markdown.clone());
        markdown
    }
}

/// A comment body as GitHub draws it: Markdown with its HTML subset (links, `<sub>`, `<details>`…).
pub(crate) fn comment_markdown(
    text: String,
    cx: &mut Context<markdown::Markdown>,
) -> markdown::Markdown {
    markdown::Markdown::new_with_options(
        text.into(),
        None,
        None,
        markdown::MarkdownOptions {
            parse_html: true,
            ..Default::default()
        },
        cx,
    )
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

/// Card text stays at the small label size so a body still fits the editor lines its block reserves.
fn card_markdown_style(window: &Window, cx: &App) -> markdown::MarkdownStyle {
    crate::github_panel::compact_markdown_style(
        markdown::MarkdownStyle::themed(markdown::MarkdownFont::Editor, window, cx),
        cx,
    )
}

fn thread_markdown_style(window: &Window, cx: &App) -> markdown::MarkdownStyle {
    pr_comments::comment_markdown_style(window, cx)
}

/// One inline card: a PR thread or a queued draft.
pub struct ReviewCard {
    card: AnyCard,
    review: WeakEntity<DiffReview>,
    markdown: MarkdownCache,
    markdown_style: Option<markdown::MarkdownStyle>,
}

impl ReviewCard {
    fn new(card: AnyCard, review: WeakEntity<DiffReview>, markdown: MarkdownCache) -> Self {
        Self {
            card,
            review,
            markdown,
            markdown_style: None,
        }
    }

    fn set_card(&mut self, card: AnyCard, cx: &mut Context<Self>) {
        if self.card != card {
            self.card = card;
            cx.notify();
        }
    }

    fn markdown_for(&self, _id: &str, text: &str, cx: &mut App) -> Entity<markdown::Markdown> {
        self.markdown.get(text, cx)
    }

    /// A comment's text as Markdown, then its images and videos; `gallery` is what the viewer
    /// steps through, by default the comment's own media.
    fn render_body(
        &self,
        id: &str,
        body: &str,
        gallery: Option<&[crate::media::MediaRef]>,
        cx: &mut App,
    ) -> AnyElement {
        let (text, media) = crate::media::split_media(body);
        let gallery = gallery.unwrap_or(&media).to_vec();
        let text = (!text.is_empty()).then(|| {
            let markdown = self.markdown_for(id, &text, cx);
            // A thread card's body takes Orca's size from the shared card around it.
            let thread = matches!(self.card, AnyCard::Thread(..));
            div()
                .when(!thread, |body| body.text_ui_sm(cx))
                .child(markdown::MarkdownElement::new(
                    markdown,
                    self.markdown_style.clone().unwrap_or_default(),
                ))
                .into_any_element()
        });
        v_flex()
            .gap_1()
            .children(text)
            .children(crate::media::render_media_strip(id, &media, &gallery, cx))
            .into_any_element()
    }

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
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let gallery: Vec<crate::media::MediaRef> = thread
            .comments
            .iter()
            .flat_map(|comment| crate::media::split_media(&comment.body).1)
            .collect();
        let viewer = self.review.upgrade().and_then(|review| {
            let review = review.read(cx);
            review
                .store
                .read(cx)
                .data(&review.workspace_key)
                .and_then(PrReviewData::viewer)
        });
        let on_review =
            |action: Rc<dyn Fn(&mut DiffReview, &mut Window, &mut Context<DiffReview>)>| {
                let review = self.review.clone();
                Rc::new(move |window: &mut Window, cx: &mut App| {
                    review
                        .update(cx, |review, cx| action(review, window, cx))
                        .log_err();
                }) as pr_comments::ClickHandler
            };
        let react_review = self.review.clone();
        let react: pr_comments::ReactHandler = Rc::new(move |subject, content, give, cx| {
            react_review
                .update(cx, |review, cx| {
                    review.toggle_reaction(&subject, &content, !give, cx)
                })
                .log_err();
        });
        let mut card = pr_comments::comment_card(cx)
            .w_full()
            .when(thread.is_resolved, |card| card.opacity(0.6));
        for (index, comment) in thread.comments.iter().enumerate() {
            let first = index == 0;
            let pending = comment.id.starts_with(crate::pr_store::PENDING_PREFIX);
            let mut buttons: Vec<(&'static str, pr_comments::ClickHandler)> = Vec::new();
            let mut menu = Vec::new();
            if !pending {
                if first {
                    let resolved = thread.is_resolved;
                    let can_toggle = if resolved {
                        thread.viewer_can_unresolve
                    } else {
                        thread.viewer_can_resolve
                    };
                    if can_toggle {
                        let id = id.clone();
                        buttons.push((
                            if resolved { "Unresolve" } else { "Resolve" },
                            on_review(Rc::new(move |review, _, cx| {
                                review.set_resolved(&id, !resolved, cx)
                            })),
                        ));
                    }
                    if thread.viewer_can_reply && !resolved {
                        let id = id.clone();
                        buttons.push((
                            "Reply",
                            on_review(Rc::new(move |review, window, cx| {
                                review.open_composer(
                                    ComposerTarget::Reply { thread: id.clone() },
                                    String::new(),
                                    window,
                                    cx,
                                )
                            })),
                        ));
                    }
                    if resolved {
                        let id = id.clone();
                        buttons.push((
                            "Collapse",
                            on_review(Rc::new(move |review, _, cx| {
                                review.toggle_expanded(&id, cx)
                            })),
                        ));
                    }
                }
                let thread_id = id.clone();
                menu.push(pr_comments::CommentAction {
                    label: "Send to Agent",
                    icon: IconName::Send,
                    destructive: false,
                    handler: on_review(Rc::new(move |review, _, cx| {
                        review.add_to_agent(&thread_id, cx)
                    })),
                });
                if let Some(url) = comment.url.clone() {
                    menu.push(pr_comments::CommentAction {
                        label: "Open on GitHub",
                        icon: IconName::ArrowUpRight,
                        destructive: false,
                        handler: Rc::new(move |_, cx| cx.open_url(&url)),
                    });
                }
                let own = pr_comments::is_own(
                    comment.author.as_deref(),
                    viewer.as_ref().map(|viewer| viewer.login.as_str()),
                );
                if own && comment.viewer_can_update {
                    let (comment_id, body) = (comment.id.clone(), comment.body.clone());
                    menu.push(pr_comments::CommentAction {
                        label: "Edit",
                        icon: IconName::Pencil,
                        destructive: false,
                        handler: on_review(Rc::new(move |review, window, cx| {
                            review.open_composer(
                                ComposerTarget::EditPublished {
                                    comment: comment_id.clone(),
                                },
                                body.clone(),
                                window,
                                cx,
                            )
                        })),
                    });
                }
                if own && comment.viewer_can_delete {
                    let comment_id = comment.id.clone();
                    menu.push(pr_comments::CommentAction {
                        label: "Delete",
                        icon: IconName::Trash,
                        destructive: true,
                        handler: on_review(Rc::new(move |review, window, cx| {
                            review.delete_published(&comment_id, window, cx)
                        })),
                    });
                }
            }
            let location = (first && !thread.placement.label.is_empty()).then(|| {
                Label::new(thread.placement.label.clone())
                    .size(LabelSize::Custom(rems(11. / 16.)))
                    .color(Color::Muted)
                    .buffer_font(cx)
                    .single_line()
                    .into_any_element()
            });
            card = card.child(pr_comments::render_comment(
                pr_comments::CommentParts {
                    id: format!("review-comment-{}-{index}", thread.id),
                    comment: comment.clone(),
                    first,
                    now,
                    review_state: None,
                    location,
                    outdated: first && thread.placement.outdated,
                    resolved: thread.is_resolved,
                    buttons,
                    menu,
                    body: self.render_body(
                        &format!("comment-{}", comment.id),
                        &comment.body,
                        Some(&gallery),
                        cx,
                    ),
                    react: (!pending).then(|| react.clone()),
                },
                cx,
            ));
        }
        card.children(
            self.review
                .upgrade()
                .and_then(|review| review.read(cx).inline_composer_for(&thread.id))
                .map(|composer| div().p_2().child(composer)),
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
            .child(self.render_body(&format!("draft-{}", draft.id), &draft.body, None, cx))
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
        card = card.child(self.render_body(&format!("note-{}", note.id), &note.body, None, cx));
        for reply in &note.replies {
            card = card.child(self.render_note_reply(&note.id, reply, cx));
        }
        card = card.children(
            self.review
                .upgrade()
                .and_then(|review| review.read(cx).inline_composer_for(&note.id)),
        );
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

    fn render_note_reply(&self, note: &str, reply: &NoteReply, cx: &mut App) -> AnyElement {
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
                })
                .when_some(
                    reply
                        .agent_id
                        .clone()
                        .filter(|_| reply.author == "reply-agent"),
                    |row, agent_id| {
                        let review = self.review.clone();
                        row.child(div().flex_1()).child(
                            Button::new(
                                SharedString::from(format!("note-transcript-{}", reply.id)),
                                "Transcript",
                            )
                            .style(ButtonStyle::Transparent)
                            .label_size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .end_icon(Icon::new(IconName::ArrowUpRight).size(IconSize::XSmall))
                            .tooltip(Tooltip::text("Open the session that wrote this answer"))
                            .on_click(move |_, _, cx| {
                                review
                                    .update(cx, |review, cx| review.open_transcript(&agent_id, cx))
                                    .log_err();
                            }),
                        )
                    },
                ),
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
            column = column.child(self.render_body(
                &format!("note-reply-{}", reply.id),
                &reply.body,
                None,
                cx,
            ));
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.markdown_style.is_none() {
            self.markdown_style = Some(match self.card {
                AnyCard::Thread(..) => thread_markdown_style(window, cx),
                _ => card_markdown_style(window, cx),
            });
        }
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
    /// A plain file editor's box: notes only, no review toggle or "Comment now".
    notes_only: bool,
    busy: bool,
    error: Option<String>,
    _subscription: Subscription,
}

impl CommentComposer {
    fn new(
        target: ComposerTarget,
        text: String,
        review: WeakEntity<DiffReview>,
        people: MentionSource,
        notes_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = match &target {
            ComposerTarget::Reply { .. } => "Reply…",
            ComposerTarget::EditDraft { .. } | ComposerTarget::EditPublished { .. } => {
                "Edit comment"
            }
            ComposerTarget::Lines { .. } if notes_only => "Leave a note on this line",
            ComposerTarget::Lines { .. } => "Leave a review comment on this line",
            ComposerTarget::NoteReply { .. } => "Ask about this note; a separate agent answers…",
            ComposerTarget::EditNote { .. } => "Edit note",
        };
        let min_lines = if is_reply(&target) { 1 } else { 3 };
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(min_lines, 14, window, cx);
            editor.set_placeholder_text(placeholder, window, cx);
            editor.set_soft_wrap();
            enable_completions(&mut editor, people, false, cx);
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
            destination: if notes_only
                || crate::client_settings::get(
                    Some(crate::client_settings::COMMENT_DESTINATION),
                    cx,
                ) == "agent"
            {
                Destination::Agent
            } else {
                Destination::Review
            },
            notes_only,
            busy: false,
            error: None,
            _subscription: subscription,
        }
    }

    /// Pastes images from the clipboard as uploads; text pastes go on to the editor.
    fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let images = clipboard_images(cx);
        if images.is_empty() {
            cx.propagate();
            return;
        }
        self.upload(images, window, cx);
    }

    fn upload(&mut self, sources: Vec<UploadSource>, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        let review = self.review.clone();
        // Deferred: uploading reads this composer, which is mid-update here.
        window.defer(cx, move |window, cx| {
            review
                .update(cx, |review, cx| {
                    for source in sources {
                        review.upload_to_composer(source, window, cx);
                    }
                })
                .log_err();
        });
    }

    fn send_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let review = self.review.clone();
        window.defer(cx, move |window, cx| {
            review
                .update(cx, |review, cx| {
                    review.submit_composer(Destination::Now, window, cx)
                })
                .log_err();
        });
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

    pub fn destination(&self) -> Destination {
        self.destination
    }

    /// Switches the box's tab, which later boxes open on too.
    pub fn set_destination(&mut self, destination: Destination, cx: &mut Context<Self>) {
        self.destination = destination;
        let remembered = match destination {
            Destination::Agent => Some("agent"),
            Destination::Review => Some("review"),
            Destination::Now => None,
        };
        if let Some(remembered) = remembered.filter(|_| !self.notes_only) {
            crate::client_settings::set(
                crate::client_settings::COMMENT_DESTINATION,
                Some(serde_json::json!(remembered)),
                cx,
            )
            .log_err();
        }
        cx.notify();
    }

    fn height(&self, cx: &App) -> u32 {
        let text = self.editor.read(cx).text(cx);
        let media = (crate::media::split_media(&text).1.len() as u32).div_ceil(3) * 10;
        let error = u32::from(self.error.is_some());
        if is_reply(&self.target) {
            let lines = (text.lines().count() as u32).clamp(1, 14);
            return 3 + lines + media + error;
        }
        let lines = (text.lines().count() as u32).clamp(3, 14);
        4 + lines + media + error
    }

    /// A reply's box: one growing line inside the thread's card, with Cancel and Reply under it.
    fn render_reply(&self, cx: &mut Context<Self>) -> AnyElement {
        let empty = self.editor.read(cx).text(cx).trim().is_empty();
        let (_, media) = crate::media::split_media(&self.editor.read(cx).text(cx));
        let media_strip =
            crate::media::render_media_strip("review-reply-media", &media, &media, cx);
        let colors = cx.theme().colors();
        let drop_bg = colors.drop_target_background;
        v_flex()
            .key_context("IcarusReply")
            .on_action(
                cx.listener(|this, _: &SubmitReviewComment, window, cx| this.submit(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &CancelReviewComment, window, cx| this.cancel(window, cx)),
            )
            .capture_action(
                cx.listener(|this, _: &editor::actions::Paste, window, cx| this.paste(window, cx)),
            )
            .drag_over::<gpui::ExternalPaths>(move |style, _, _, _| style.bg(drop_bg))
            .on_drop(
                cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| {
                    let sources = paths
                        .paths()
                        .iter()
                        .cloned()
                        .map(UploadSource::Path)
                        .collect();
                    this.upload(sources, window, cx);
                }),
            )
            .mt_1()
            .p_1p5()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.editor_background)
            .child(self.editor.clone())
            .children(media_strip)
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
                        IconButton::new("review-reply-media", IconName::Image)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Attach an image or video"))
                            .disabled(self.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.pick_media(window, cx)),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("review-reply-cancel", "Cancel")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
                    )
                    .child(
                        Button::new(
                            "review-reply-send",
                            if self.busy { "Posting…" } else { "Reply" },
                        )
                        .style(ButtonStyle::Filled)
                        .label_size(LabelSize::Small)
                        .disabled(self.busy || empty)
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
            .into_any_element()
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

fn is_reply(target: &ComposerTarget) -> bool {
    matches!(
        target,
        ComposerTarget::Reply { .. } | ComposerTarget::NoteReply { .. }
    )
}

impl Render for CommentComposer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if is_reply(&self.target) {
            return self.render_reply(cx);
        }
        let header = match &self.target {
            ComposerTarget::Lines { start, end } => lines_label(*start, *end).to_uppercase(),
            ComposerTarget::EditDraft { .. } | ComposerTarget::EditPublished { .. } => {
                "EDIT COMMENT".to_owned()
            }
            ComposerTarget::Reply { .. } => "REPLY".to_owned(),
            ComposerTarget::NoteReply { .. } => "REPLY TO NOTE".to_owned(),
            ComposerTarget::EditNote { .. } => "EDIT NOTE".to_owned(),
        };
        let new_comment = matches!(self.target, ComposerTarget::Lines { .. });
        let submit_label = match (&self.target, self.destination) {
            (_, _) if self.busy => "Posting…",
            (ComposerTarget::Lines { .. }, Destination::Review | Destination::Now) => {
                "Add to review"
            }
            (ComposerTarget::Lines { .. }, Destination::Agent) => "Add note",
            (
                ComposerTarget::EditDraft { .. }
                | ComposerTarget::EditNote { .. }
                | ComposerTarget::EditPublished { .. },
                _,
            ) => "Save",
            (ComposerTarget::Reply { .. } | ComposerTarget::NoteReply { .. }, _) => "Reply",
        };
        let can_post_now =
            new_comment && !self.notes_only && self.destination != Destination::Agent;
        let empty = self.editor.read(cx).text(cx).trim().is_empty();
        let (_, media) = crate::media::split_media(&self.editor.read(cx).text(cx));
        let media_strip =
            crate::media::render_media_strip("review-composer-media", &media, &media, cx);
        let drop_bg = cx.theme().colors().drop_target_background;
        let composer = card_frame(cx)
            .key_context("IcarusReviewComposer")
            .on_action(
                cx.listener(|this, _: &SubmitReviewComment, window, cx| this.submit(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &CancelReviewComment, window, cx| this.cancel(window, cx)),
            )
            .capture_action(
                cx.listener(|this, _: &editor::actions::Paste, window, cx| this.paste(window, cx)),
            )
            .drag_over::<gpui::ExternalPaths>(move |style, _, _, _| style.bg(drop_bg))
            .on_drop(
                cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| {
                    let sources = paths
                        .paths()
                        .iter()
                        .cloned()
                        .map(UploadSource::Path)
                        .collect();
                    this.upload(sources, window, cx);
                }),
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
                    .when(new_comment && !self.notes_only, |row| {
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
                                Destination::Review | Destination::Now => 1,
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
            .children(media_strip)
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
                    .when(can_post_now, |row| {
                        row.child(
                            Button::new("review-comment-now", "Comment now")
                                .style(ButtonStyle::Outlined)
                                .label_size(LabelSize::Small)
                                .disabled(self.busy || empty)
                                .tooltip(Tooltip::text(
                                    "Post this one comment now, outside your pending review",
                                ))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.send_now(window, cx)),
                                ),
                        )
                    })
                    .child(
                        Button::new("review-submit", submit_label)
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .disabled(self.busy || empty)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            );
        composer.into_any_element()
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

    /// Requests to these methods are recorded but never answered, so a test can check what the
    /// view shows while the server is still working.
    pub(crate) struct HeldRpc(pub(crate) Vec<&'static str>);

    impl gpui::Global for HeldRpc {}

    pub(crate) fn hold_rpc(cx: &mut App, methods: &[&'static str]) {
        cx.set_global(HeldRpc(methods.to_vec()));
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

    #[gpui::test]
    async fn the_reviewer_picker_lists_suggestions_first_and_requests_the_pick(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let calls = cx.update(|cx| {
            stub_rpc(cx, |method, _| match method {
                "review.get" => Ok(review_view(0)),
                "review.reviewer_suggestions" => Ok(json!({
                    "suggested": [{ "login": "carol", "name": "Carol" }, { "login": "bob" }],
                    "everyone": [{ "login": "alice", "name": "Alice A" }, { "login": "dave" }],
                })),
                _ => Ok(json!({})),
            })
        });
        let window = cx.add_empty_window();
        let section = window
            .update(|window, cx| cx.new(|cx| ReviewSection::new("ws-1".into(), None, window, cx)));
        window.run_until_parked();
        assert_eq!(
            calls_to(&calls, "review.reviewer_suggestions"),
            [json!({ "workspace": "ws-1" })]
        );
        section.update(window, |section, _| {
            section.set_pull_request(&pull_request(json!({
                "number": 7, "title": "t", "url": "u", "state": "OPEN", "head_ref_name": "h",
                "base_ref_name": "main", "head_ref_oid": "abc", "author": "me",
                "requested_reviewers": [{ "kind": "user", "login": "bob" }],
            })));
        });
        let listed: Vec<(String, bool)> = section.read_with(window, |section, _| {
            section
                .picker_candidates()
                .into_iter()
                .map(|candidate| (candidate.login, candidate.suggested))
                .collect()
        });
        assert_eq!(
            listed,
            [
                ("carol".to_owned(), true),
                ("bob".to_owned(), true),
                ("alice".to_owned(), false),
                ("dave".to_owned(), false)
            ],
            "suggestions first; someone asked but not yet reviewing stays, to be taken back"
        );
        assert!(section.read_with(window, |section, _| section.is_requested("bob")));

        let picker = window.update(|window, cx| {
            let section = section.downgrade();
            cx.new(|cx| reviewer_picker(section, window, cx))
        });
        let pick = |query: &str, window: &mut gpui::VisualTestContext| {
            picker.update_in(window, |picker, window, cx| {
                picker.set_query(query, window, cx)
            });
            window.run_until_parked();
            picker.update_in(window, |picker, window, cx| {
                PickerDelegate::confirm(&mut picker.delegate, false, window, cx)
            });
            window.run_until_parked();
        };
        pick("ali", window);
        assert_eq!(
            calls_to(&calls, "review.request_reviewers")[0]["reviewers"],
            json!(["alice"])
        );
        pick("bob", window);
        assert_eq!(
            calls_to(&calls, "review.remove_reviewers")[0]["reviewers"],
            json!(["bob"]),
            "picking someone already asked takes the request back"
        );
        pick("@acme/backend", window);
        assert_eq!(
            calls_to(&calls, "review.request_reviewers")[1]["reviewers"],
            json!(["acme/backend"]),
            "text that matches nobody is requested as typed, teams too"
        );
    }

    #[test]
    fn typed_reviewers_split_like_orca() {
        assert_eq!(
            typed_reviewers(" @alice, acme/backend  bob,"),
            ["alice", "acme/backend", "bob"]
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
        assert_eq!(thread_height(&thread, false), 1 + 3 + 3 + 3);
        thread.is_resolved = true;
        assert_eq!(thread_height(&thread, false), 2);
        let draft = ReviewDraft {
            body: "look ![shot](https://x/y.png)".into(),
            ..Default::default()
        };
        assert_eq!(markdown_images(&draft.body), ["https://x/y.png"]);
        assert_eq!(draft_height(&draft), 3 + 1 + 10);
    }

    #[gpui::test]
    async fn note_bodies_and_answers_render_as_markdown(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let note: WorkspaceNote = serde_json::from_value(json!({
            "id": "n1", "path": "src/a.rs", "line": 1, "end_line": 2, "author": "agent",
            "body": "Migration is **safe**.",
            "replies": [
                { "id": "r1", "author": "user", "body": "is that `good`?" },
                { "id": "r2", "author": "reply-agent", "status": "done",
                  "body": "Yes.\n\n- **The migration** is safe.\n- Rollout is fine." },
            ],
        }))
        .expect("note");
        let (card, cx) = cx.add_window_view(|_, _| {
            ReviewCard::new(
                AnyCard::Note(note, true),
                WeakEntity::new_invalid(),
                MarkdownCache::default(),
            )
        });
        cx.run_until_parked();
        card.read_with(cx, |card, cx| {
            let sources: HashSet<String> = card
                .markdown
                .0
                .borrow()
                .values()
                .map(|markdown| markdown.read(cx).source().to_string())
                .collect();
            assert!(sources.contains("Migration is **safe**."));
            assert!(sources.contains("is that `good`?"));
            assert!(sources.contains("Yes.\n\n- **The migration** is safe.\n- Rollout is fine."));
        });
    }
}
