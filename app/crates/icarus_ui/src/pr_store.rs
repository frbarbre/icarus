//! One client-side copy of each pull request's review state (threads, conversation, drafts) per
//! workspace, shared by the diff tabs' inline cards and the Checks panel. Mutations show at once and
//! reconcile with the server's answer; a failure rolls back and says why.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Task};
use icarus_client::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use util::ResultExt as _;

use crate::pr_comments::ConversationComment;
use crate::review::{ReviewDraft, ReviewThread, ThreadComment, request, toggle_reaction_locally};

/// Ids of comments, threads and drafts that exist only here until the server answers.
pub const PENDING_PREFIX: &str = "pending-";
/// How long a confirmed change is laid over loads that may predate it.
const SETTLED_TTL: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Viewer {
    pub login: String,
    pub avatar_url: Option<String>,
}

/// What both views draw for one workspace's pull request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrReviewData {
    pub threads: Vec<ReviewThread>,
    pub conversation: Vec<ConversationComment>,
    pub drafts: Vec<ReviewDraft>,
    pub viewer: Option<Viewer>,
    pub loaded: bool,
    /// Why the last load failed; a branch without a PR or a repo off GitHub is not an error.
    pub load_error: Option<String>,
}

impl PrReviewData {
    pub fn thread(&self, id: &str) -> Option<&ReviewThread> {
        self.threads.iter().find(|thread| thread.id == id)
    }

    fn comment_mut(&mut self, id: &str) -> Option<&mut ThreadComment> {
        self.threads
            .iter_mut()
            .flat_map(|thread| thread.comments.iter_mut())
            .find(|comment| comment.id == id)
    }

    fn has_comment(&self, id: &str) -> bool {
        self.threads
            .iter()
            .flat_map(|thread| &thread.comments)
            .any(|comment| comment.id == id)
            || self.conversation.iter().any(|comment| comment.id == id)
    }

    /// The signed-in user: the server's, else the author of a comment only they may delete.
    pub fn viewer(&self) -> Option<Viewer> {
        if let Some(viewer) = &self.viewer {
            return Some(viewer.clone());
        }
        self.threads
            .iter()
            .flat_map(|thread| &thread.comments)
            .find(|comment| comment.viewer_can_delete && comment.author.is_some())
            .map(|comment| (comment.author.clone(), comment.author_avatar_url.clone()))
            .or_else(|| {
                self.conversation
                    .iter()
                    .find(|comment| comment.viewer_can_delete && comment.author.is_some())
                    .map(|comment| (comment.author.clone(), comment.author_avatar_url.clone()))
            })
            .and_then(|(login, avatar_url)| {
                Some(Viewer {
                    login: login?,
                    avatar_url,
                })
            })
    }
}

/// One change the user made, drawn before the server confirms it.
#[derive(Debug, Clone, PartialEq)]
pub enum ReviewOp {
    Reply {
        thread: String,
        comment: ThreadComment,
    },
    /// A comment posted now, which opens a new thread.
    NewThread {
        thread: ReviewThread,
        start_line: Option<u32>,
    },
    NewDraft {
        draft: ReviewDraft,
    },
    EditDraft {
        id: String,
        body: String,
    },
    RemoveDraft {
        id: String,
    },
    Resolve {
        thread: String,
        resolved: bool,
    },
    React {
        subject: String,
        reaction: String,
        react: bool,
    },
    /// A published review comment, issue comment or review summary, addressed by `published`.
    EditComment {
        id: String,
        body: String,
        published: Value,
    },
    DeleteComment {
        id: String,
        published: Value,
    },
    /// A conversation comment; `reply_to` is the author it answers, whom the body already names.
    Conversation {
        comment: ConversationComment,
        body: String,
        reply_to: Option<String>,
    },
}

pub(crate) fn now_rfc3339() -> Option<String> {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

impl ReviewOp {
    fn call(&self, workspace: &str) -> (&'static str, Value) {
        match self {
            Self::Reply { thread, comment } => (
                "review.reply",
                json!({ "workspace": workspace, "thread": thread, "body": comment.body }),
            ),
            Self::NewThread { thread, start_line } => (
                "review.comment.post",
                json!({
                    "workspace": workspace, "path": thread.path, "line": thread.line,
                    "start_line": start_line,
                    "body": thread.comments.first().map(|comment| comment.body.clone()),
                }),
            ),
            Self::NewDraft { draft } => (
                "review.comment.add",
                json!({
                    "workspace": workspace, "path": draft.path, "line": draft.line,
                    "start_line": draft.start_line, "body": draft.body,
                }),
            ),
            Self::EditDraft { id, body } => (
                "review.comment.edit",
                json!({ "workspace": workspace, "id": id, "body": body }),
            ),
            Self::RemoveDraft { id } => (
                "review.comment.remove",
                json!({ "workspace": workspace, "id": id }),
            ),
            Self::Resolve { thread, resolved } => (
                if *resolved {
                    "review.resolve"
                } else {
                    "review.unresolve"
                },
                json!({ "workspace": workspace, "thread": thread }),
            ),
            Self::React {
                subject,
                reaction,
                react,
            } => (
                "review.react",
                json!({
                    "workspace": workspace, "subject": subject, "reaction": reaction,
                    "remove": !react,
                }),
            ),
            Self::EditComment {
                body, published, ..
            } => {
                let mut params = published.clone();
                params["workspace"] = workspace.into();
                params["body"] = body.clone().into();
                ("review.comment.edit_published", params)
            }
            Self::DeleteComment { published, .. } => {
                let mut params = published.clone();
                params["workspace"] = workspace.into();
                ("review.comment.delete_published", params)
            }
            Self::Conversation { body, reply_to, .. } => (
                "review.conversation.post",
                json!({ "workspace": workspace, "body": body, "reply_to": reply_to }),
            ),
        }
    }

    /// The change with the server's ids in place of the pending ones.
    fn confirmed(mut self, reply: &Value) -> Self {
        match &mut self {
            Self::Reply { comment, .. } => {
                if let Some(id) = reply["comment"]["id"].as_str() {
                    comment.id = id.to_owned();
                }
                if let Some(url) = reply["comment"]["url"].as_str() {
                    comment.url = Some(url.to_owned());
                }
                comment.viewer_can_update = true;
                comment.viewer_can_delete = true;
            }
            Self::NewThread { thread, .. } => {
                if let Ok(posted) = serde_json::from_value::<ReviewThread>(reply["thread"].clone())
                    && !posted.id.is_empty()
                {
                    *thread = posted;
                }
            }
            Self::NewDraft { draft } => {
                if let Ok(saved) = serde_json::from_value::<ReviewDraft>(reply["draft"].clone())
                    && !saved.id.is_empty()
                {
                    *draft = saved;
                }
            }
            Self::Conversation { comment, .. } => {
                if let Ok(posted) =
                    serde_json::from_value::<ConversationComment>(reply["comment"].clone())
                    && !posted.id.is_empty()
                {
                    let avatar = comment.author_avatar_url.take();
                    *comment = posted;
                    comment.author_avatar_url = comment.author_avatar_url.take().or(avatar);
                }
            }
            _ => {}
        }
        self
    }

    fn apply(&self, data: &mut PrReviewData) {
        match self {
            Self::Reply { thread, comment } => {
                if data.has_comment(&comment.id) {
                    return;
                }
                if let Some(found) = data.threads.iter_mut().find(|t| t.id == *thread) {
                    found.comments.push(comment.clone());
                }
            }
            Self::NewThread { thread, .. } => {
                let first = thread.comments.first().map(|comment| comment.id.as_str());
                let known = data.threads.iter().any(|existing| {
                    existing.id == thread.id
                        || first.is_some_and(|first| {
                            existing.comments.first().map(|c| c.id.as_str()) == Some(first)
                        })
                });
                if !known {
                    data.threads.push(thread.clone());
                }
            }
            Self::NewDraft { draft } => {
                if !data.drafts.iter().any(|existing| existing.id == draft.id) {
                    data.drafts.push(draft.clone());
                }
            }
            Self::EditDraft { id, body } => {
                if let Some(draft) = data.drafts.iter_mut().find(|draft| draft.id == *id) {
                    draft.body = body.clone();
                }
            }
            Self::RemoveDraft { id } => data.drafts.retain(|draft| draft.id != *id),
            Self::Resolve { thread, resolved } => {
                if let Some(found) = data.threads.iter_mut().find(|t| t.id == *thread) {
                    found.is_resolved = *resolved;
                    found.viewer_can_resolve = !resolved;
                    found.viewer_can_unresolve = *resolved;
                }
            }
            Self::React {
                subject,
                reaction,
                react,
            } => {
                let reactions = match data.comment_mut(subject) {
                    Some(comment) => Some(&mut comment.reactions),
                    None => data
                        .conversation
                        .iter_mut()
                        .find(|comment| comment.id == *subject)
                        .map(|comment| &mut comment.reactions),
                };
                if let Some(reactions) = reactions {
                    toggle_reaction_locally(reactions, reaction, *react);
                }
            }
            Self::EditComment { id, body, .. } => {
                let edited_at = now_rfc3339();
                if let Some(comment) = data.comment_mut(id) {
                    if comment.body != *body {
                        comment.body = body.clone();
                        comment.last_edited_at = comment.last_edited_at.take().or(edited_at);
                    }
                } else if let Some(comment) = data.conversation.iter_mut().find(|c| c.id == *id)
                    && comment.body != *body
                {
                    comment.body = body.clone();
                    comment.last_edited_at = comment.last_edited_at.take().or(edited_at);
                }
            }
            Self::DeleteComment { id, .. } => {
                for thread in &mut data.threads {
                    thread.comments.retain(|comment| comment.id != *id);
                }
                data.threads.retain(|thread| !thread.comments.is_empty());
                data.conversation.retain(|comment| comment.id != *id);
            }
            Self::Conversation { comment, .. } => {
                if !data.has_comment(&comment.id) {
                    data.conversation.push(comment.clone());
                }
            }
        }
    }

    /// Whether loaded data already shows this change, so it needn't be laid over it any more.
    fn absorbed(&self, data: &PrReviewData) -> bool {
        match self {
            Self::Reply { comment, .. } => data.has_comment(&comment.id),
            Self::NewThread { thread, .. } => thread
                .comments
                .first()
                .is_some_and(|first| data.has_comment(&first.id)),
            Self::NewDraft { draft } => data.drafts.iter().any(|d| d.id == draft.id),
            Self::EditDraft { id, body } => data
                .drafts
                .iter()
                .find(|draft| draft.id == *id)
                .is_none_or(|draft| draft.body == *body),
            Self::RemoveDraft { id } => !data.drafts.iter().any(|draft| draft.id == *id),
            Self::Resolve { thread, resolved } => data
                .thread(thread)
                .is_none_or(|found| found.is_resolved == *resolved),
            Self::React {
                subject,
                reaction,
                react,
            } => {
                let reactions = data
                    .threads
                    .iter()
                    .flat_map(|thread| &thread.comments)
                    .find(|comment| comment.id == *subject)
                    .map(|comment| &comment.reactions)
                    .or_else(|| {
                        data.conversation
                            .iter()
                            .find(|comment| comment.id == *subject)
                            .map(|comment| &comment.reactions)
                    });
                reactions.is_none_or(|reactions| {
                    reactions
                        .iter()
                        .any(|r| r.content == *reaction && r.viewer_has_reacted)
                        == *react
                })
            }
            Self::EditComment { id, body, .. } => {
                let thread_body = data
                    .threads
                    .iter()
                    .flat_map(|thread| &thread.comments)
                    .find(|comment| comment.id == *id)
                    .map(|comment| &comment.body);
                let body_now = thread_body.or_else(|| {
                    data.conversation
                        .iter()
                        .find(|comment| comment.id == *id)
                        .map(|comment| &comment.body)
                });
                body_now.is_none_or(|known| known == body)
            }
            Self::DeleteComment { id, .. } => !data.has_comment(id),
            Self::Conversation { comment, .. } => data.has_comment(&comment.id),
        }
    }
}

struct Change {
    id: u64,
    op: ReviewOp,
    settled_at: Option<Instant>,
}

#[derive(Default)]
struct PrState {
    base: PrReviewData,
    changes: Vec<Change>,
    visible: PrReviewData,
    loading: Option<Task<()>>,
}

impl PrState {
    /// Draws the server's copy with every change on top; true when that differs from before.
    fn recompute(&mut self) -> bool {
        let mut visible = self.base.clone();
        for change in &self.changes {
            change.op.apply(&mut visible);
        }
        let changed = visible != self.visible;
        if changed {
            self.visible = visible;
        }
        changed
    }
}

/// A failed change, already rolled back.
#[derive(Debug, Clone, PartialEq)]
pub struct PrReviewFailed {
    pub workspace: String,
    pub message: String,
}

#[derive(Default)]
pub struct PrReviewStore {
    prs: HashMap<String, PrState>,
    next_change: u64,
}

impl EventEmitter<PrReviewFailed> for PrReviewStore {}

struct GlobalPrReviewStore(Entity<PrReviewStore>);

impl Global for GlobalPrReviewStore {}

impl PrReviewStore {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalPrReviewStore>() {
            return global.0.clone();
        }
        let store = cx.new(|_| Self::default());
        cx.set_global(GlobalPrReviewStore(store.clone()));
        store
    }

    /// The pull request of the workspace at `workspace` (its root path) as both views draw it.
    pub fn data(&self, workspace: &str) -> Option<&PrReviewData> {
        self.prs.get(workspace).map(|state| &state.visible)
    }

    /// Reads the threads, conversation and drafts. A newer load replaces one still on its way: the
    /// server answers both from one cache, and data equal to what is shown changes nothing.
    pub fn load(
        &mut self,
        workspace: &str,
        client: Option<Arc<Client>>,
        refresh: bool,
        cx: &mut Context<Self>,
    ) {
        let (Some(threads), Some(drafts)) = (
            request(
                client.clone(),
                "review.threads",
                json!({ "workspace": workspace, "refresh": refresh, "conversation": true }),
                cx,
            ),
            request(
                client,
                "review.comment.list",
                json!({ "workspace": workspace }),
                cx,
            ),
        ) else {
            return;
        };
        let key = workspace.to_owned();
        let task = cx.spawn(async move |this, cx| {
            let threads = threads.await;
            let drafts = drafts.await;
            this.update(cx, |store, cx| {
                if let Some(state) = store.prs.get_mut(&key) {
                    state.loading = None;
                }
                match (threads, drafts) {
                    (Ok(threads), Ok(drafts)) => store.set_loaded(
                        &key,
                        Some(
                            serde_json::from_value(threads["threads"].clone()).unwrap_or_default(),
                        ),
                        serde_json::from_value(threads["conversation"].clone()).ok(),
                        Some(serde_json::from_value(drafts["drafts"].clone()).unwrap_or_default()),
                        serde_json::from_value(threads["viewer"].clone()).ok(),
                        cx,
                    ),
                    (Err(error), _) | (_, Err(error)) => {
                        store.set_load_failed(&key, format!("{error:#}"), cx)
                    }
                }
            })
            .log_err();
        });
        self.prs.entry(workspace.to_owned()).or_default().loading = Some(task);
    }

    /// Takes data the server sent; `None` keeps what is known. Pending and recent changes stay on top.
    pub fn set_loaded(
        &mut self,
        workspace: &str,
        threads: Option<Vec<ReviewThread>>,
        conversation: Option<Vec<ConversationComment>>,
        drafts: Option<Vec<ReviewDraft>>,
        viewer: Option<Viewer>,
        cx: &mut Context<Self>,
    ) {
        let state = self.prs.entry(workspace.to_owned()).or_default();
        let base = &mut state.base;
        if let Some(threads) = threads {
            base.threads = threads;
        }
        if let Some(conversation) = conversation {
            base.conversation = conversation;
        }
        if let Some(drafts) = drafts {
            base.drafts = drafts;
        }
        if viewer.is_some() {
            base.viewer = viewer;
        }
        base.loaded = true;
        base.load_error = None;
        let base = &state.base;
        state.changes.retain(|change| match change.settled_at {
            None => true,
            Some(at) => at.elapsed() < SETTLED_TTL && !change.op.absorbed(base),
        });
        if state.recompute() {
            cx.notify();
        }
    }

    fn set_load_failed(&mut self, workspace: &str, error: String, cx: &mut Context<Self>) {
        let state = self.prs.entry(workspace.to_owned()).or_default();
        let quiet = error.contains("no_pr") || error.contains("no_github");
        state.base.load_error = (!quiet).then_some(error);
        state.base.threads.clear();
        state.base.conversation.clear();
        state.base.drafts.clear();
        state.base.loaded = true;
        state.recompute();
        cx.notify();
    }

    pub fn pending_id(&mut self) -> String {
        self.next_change += 1;
        format!("{PENDING_PREFIX}{}", self.next_change)
    }

    /// Shows `op` at once, then sends it; the answer replaces pending ids, a failure undoes it.
    pub fn mutate(
        &mut self,
        workspace: &str,
        client: Option<Arc<Client>>,
        op: ReviewOp,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<Value>>> {
        let (method, params) = op.call(workspace);
        let call = request(client, method, params, cx)?;
        self.next_change += 1;
        let id = self.next_change;
        let state = self.prs.entry(workspace.to_owned()).or_default();
        state.changes.push(Change {
            id,
            op,
            settled_at: None,
        });
        if state.recompute() {
            cx.notify();
        }
        let key = workspace.to_owned();
        let task = cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |store, cx| store.settle(&key, id, &result, cx))?;
            result
        });
        Some(task)
    }

    fn settle(
        &mut self,
        workspace: &str,
        id: u64,
        result: &anyhow::Result<Value>,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.prs.get_mut(workspace) else {
            return;
        };
        let Some(index) = state.changes.iter().position(|change| change.id == id) else {
            return;
        };
        match result {
            Ok(reply) => {
                let change = &mut state.changes[index];
                change.op = change.op.clone().confirmed(reply);
                change.settled_at = Some(Instant::now());
            }
            Err(error) => {
                state.changes.remove(index);
                let message = format!("{error:#}");
                show_failure(workspace, &message, cx);
                cx.emit(PrReviewFailed {
                    workspace: workspace.to_owned(),
                    message,
                });
            }
        }
        if state.recompute() {
            cx.notify();
        }
    }
}

/// Adds a thread to the workspace's agent composer from what is shown, without asking anyone.
pub fn send_thread_to_agent(workspace: &str, thread: &str, cx: &mut App) -> Option<String> {
    let store = PrReviewStore::global(cx);
    let thread = store.read(cx).data(workspace)?.thread(thread)?.clone();
    let first = thread.comments.first()?;
    let title = format!(
        "@{} on {}",
        first.author.as_deref().unwrap_or("ghost"),
        thread_location(&thread)
    );
    let text = thread_text(&thread);
    Some(attach_local(workspace, "pr-comment", title, text, cx))
}

/// Adds a conversation comment or review summary to the workspace's agent composer.
pub fn send_conversation_to_agent(workspace: &str, comment: &str, cx: &mut App) -> Option<String> {
    let store = PrReviewStore::global(cx);
    let entry = store
        .read(cx)
        .data(workspace)?
        .conversation
        .iter()
        .find(|entry| entry.id == comment)?
        .clone();
    let author = entry.author.as_deref().unwrap_or("ghost");
    let title = match entry.review_state.as_deref() {
        Some(state) => format!(
            "@{author}'s review ({})",
            state.to_lowercase().replace('_', " ")
        ),
        None => format!("@{author} on the pull request"),
    };
    let mut text = format!("{title}:\n");
    if let Some(url) = &entry.url {
        text.push_str(url);
        text.push('\n');
    }
    text.push('\n');
    text.push_str(entry.body.trim());
    Some(attach_local(workspace, "pr-comment", title, text, cx))
}

fn thread_location(thread: &ReviewThread) -> String {
    let line = (!thread.placement.file)
        .then(|| thread.line.or(thread.original_line))
        .flatten();
    match line {
        Some(line) => format!("{}:{line}", thread.path),
        None => thread.path.clone(),
    }
}

/// The context block an agent reads, as the server's `attach.pr_comments` writes it.
pub fn thread_text(thread: &ReviewThread) -> String {
    let mut state = vec![if thread.is_resolved {
        "resolved"
    } else {
        "unresolved"
    }];
    if thread.placement.outdated {
        state.push("outdated");
    }
    let mut text = format!("{} ({})\n", thread_location(thread), state.join(", "));
    if let Some(url) = thread
        .comments
        .first()
        .and_then(|comment| comment.url.as_ref())
    {
        text.push_str(url);
        text.push('\n');
    }
    for comment in &thread.comments {
        let author = comment.author.as_deref().unwrap_or("ghost");
        text.push_str(&format!("\n@{author}:\n{}\n", comment.body.trim_end()));
    }
    text
}

/// Attachments made in this app, answered without the server when the prompt goes out.
#[derive(Default)]
pub(crate) struct LocalAttachments {
    texts: HashMap<String, String>,
    next: u64,
}

impl Global for LocalAttachments {}

pub(crate) fn local_attachment_text(id: &str, cx: &App) -> Option<String> {
    cx.try_global::<LocalAttachments>()?.texts.get(id).cloned()
}

#[cfg(test)]
pub(crate) fn local_attachment_texts(cx: &App) -> Vec<String> {
    cx.try_global::<LocalAttachments>()
        .map(|attachments| attachments.texts.values().cloned().collect())
        .unwrap_or_default()
}

fn attach_local(workspace: &str, kind: &str, title: String, text: String, cx: &mut App) -> String {
    let attachments = cx.default_global::<LocalAttachments>();
    attachments.next += 1;
    let id = format!("att-local-{}-{}", std::process::id(), attachments.next);
    attachments.texts.insert(id.clone(), text);
    crate::attach_to_composer(
        crate::ComposerAttachRequest {
            workspace_path: PathBuf::from(workspace),
            attachment: crate::ComposerAttachment {
                id: id.clone(),
                kind: kind.to_owned(),
                title,
            },
        },
        cx,
    );
    id
}

fn show_failure(workspace: &str, message: &str, cx: &mut App) {
    struct PrReviewToast;
    let Some((window, target)) = crate::workspace_for_path(std::path::Path::new(workspace), cx)
    else {
        log::warn!("review change failed: {message}");
        return;
    };
    let message = message.to_owned();
    window
        .update(cx, |_, _window, cx| {
            target.update(cx, |workspace, cx| {
                workspace.show_toast(
                    workspace::Toast::new(
                        workspace::notifications::NotificationId::unique::<PrReviewToast>(),
                        message,
                    )
                    .autohide(),
                    cx,
                );
            });
        })
        .log_err();
}
