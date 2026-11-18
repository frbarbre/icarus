//! PR review threads outside the diff: the Changes list's per-file count, the PR panel's thread and draft
//! rows, and opening a thread's line in the file's diff tab.

use std::path::PathBuf;

use collections::HashMap;
use editor::{Editor, SelectionEffects, scroll::Autoscroll};
use gpui::{AnyElement, App, Context, Entity, Task, Window};
use language::Point;
use serde::Deserialize;
use serde_json::Value;
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use crate::diff_tab::{DiffBase, DiffTab, DiffTabMode, open_diff_tab};
use crate::github_panel::Tone;
use crate::review::{ReactionSummary, ReviewDraft, ReviewThread, ThreadComment};

/// Unresolved threads per file, from a `review.threads` reply.
pub fn open_thread_counts(reply: &Value) -> HashMap<String, usize> {
    let mut counts = HashMap::default();
    for thread in reply["threads"].as_array().into_iter().flatten() {
        if thread["is_resolved"].as_bool() == Some(true) {
            continue;
        }
        if let Some(path) = thread["path"].as_str() {
            *counts.entry(path.to_owned()).or_default() += 1;
        }
    }
    counts
}

/// A Changes row's GitHub badge: the file's unresolved PR threads.
pub fn open_thread_badge(index: usize, count: Option<usize>) -> Option<AnyElement> {
    let count = count.filter(|count| *count > 0)?;
    Some(
        h_flex()
            .id(("changes-row-pr-threads", index))
            .gap_0p5()
            .tooltip(Tooltip::text(if count == 1 {
                "1 unresolved PR thread".to_owned()
            } else {
                format!("{count} unresolved PR threads")
            }))
            .child(
                Icon::new(IconName::Chat)
                    .size(IconSize::XSmall)
                    .color(Color::Info),
            )
            .child(
                Label::new(count.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Info),
            )
            .into_any_element(),
    )
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The line a thread opens at, or `None` for a file thread or one whose line left the diff.
pub fn thread_line(thread: &ReviewThread) -> Option<u32> {
    if thread.placement.outdated || thread.placement.file {
        return None;
    }
    thread.placement.line.or(thread.line)
}

/// Orca's path badge: `export.rs:L42`; an outdated thread keeps the line it was left on, and a
/// file thread shows the bare file name.
pub fn thread_location(thread: &ReviewThread) -> String {
    let line = if thread.placement.file {
        None
    } else {
        thread_line(thread).or(thread.line).or(thread.original_line)
    };
    match line {
        Some(line) => format!("{}:L{line}", file_name(&thread.path)),
        None => file_name(&thread.path).to_owned(),
    }
}

pub fn draft_location(draft: &ReviewDraft) -> String {
    match (draft.start_line, draft.line) {
        (Some(start), Some(end)) if start != end => {
            format!("{}:L{start}-L{end}", file_name(&draft.path))
        }
        (_, Some(line)) => format!("{}:L{line}", file_name(&draft.path)),
        _ => file_name(&draft.path).to_owned(),
    }
}

/// The first line of text in a comment, without markdown images.
pub fn excerpt(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("![") && !line.starts_with("```"))
        .unwrap_or_default()
        .to_owned()
}

/// The Comments section's tabs (github-review.md §9.1); Humans is the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Audience {
    All,
    #[default]
    Humans,
    Bots,
    Unresolved,
}

impl Audience {
    pub const ALL: [Audience; 4] = [Self::All, Self::Humans, Self::Bots, Self::Unresolved];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Humans => "Humans",
            Self::Bots => "Bots",
            Self::Unresolved => "Unresolved",
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            Self::All => IconName::MessagesSquare,
            Self::Humans => IconName::Person,
            Self::Bots => IconName::Bot,
            Self::Unresolved => IconName::MessageSquareDot,
        }
    }

    pub fn empty_text(self) -> &'static str {
        match self {
            Self::All => "No comments yet.",
            Self::Humans => "No human comments.",
            Self::Bots => "No bot comments.",
            Self::Unresolved => "No unresolved threads.",
        }
    }

    fn includes(self, thread: &ReviewThread, comment: &ThreadComment) -> bool {
        match self {
            Self::All => true,
            Self::Humans => !is_bot(comment.is_bot, comment.author.as_deref()),
            Self::Bots => is_bot(comment.is_bot, comment.author.as_deref()),
            Self::Unresolved => !thread.is_resolved,
        }
    }

    fn includes_conversation(self, comment: &ConversationComment) -> bool {
        match self {
            Self::All => true,
            Self::Humans => !comment.is_automated(),
            Self::Bots => comment.is_automated(),
            Self::Unresolved => false,
        }
    }
}

/// Comments in the tab, counted per comment as Orca counts them; `threads` are the open ones,
/// since resolved threads have their own section.
pub fn audience_count(
    threads: &[ReviewThread],
    conversation: &[ConversationComment],
    audience: Audience,
) -> usize {
    let in_threads = threads
        .iter()
        .flat_map(|thread| {
            thread
                .comments
                .iter()
                .filter(move |comment| audience.includes(thread, comment))
        })
        .count();
    in_threads
        + conversation
            .iter()
            .filter(|comment| audience.includes_conversation(comment))
            .count()
}

// GitHub reports some review services as ordinary users, so Orca also goes by the login.
const AUTOMATION_LOGINS: [&str; 16] = [
    "chatgpt-codex-connector",
    "codex-connector",
    "qodo",
    "coderabbit",
    "codium",
    "sonarcloud",
    "sonarqube",
    "sourcery-ai",
    "deepsource",
    "snyk",
    "codecov",
    "greptile",
    "ellipsis",
    "graphite-app",
    "reviewer-gpt",
    "-reviewer",
];

/// Orca's bot test: GitHub's own flag, a `[bot]` login, or a login that reads as automation.
pub fn is_bot(flagged: bool, author: Option<&str>) -> bool {
    if flagged {
        return true;
    }
    let Some(login) = author.map(|login| login.trim().to_lowercase()) else {
        return false;
    };
    let has_word_bot = login
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .any(|word| word == "bot");
    login.ends_with("bot")
        || login.ends_with("[bot]")
        || has_word_bot
        || ["automation", "actions", "renovate", "dependabot"]
            .iter()
            .chain(AUTOMATION_LOGINS.iter())
            .any(|needle| login.contains(needle))
}

/// A submitted review's verdict, shown on its summary like Orca's review badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

impl ReviewState {
    pub fn parse(state: &str) -> Option<Self> {
        match state {
            "APPROVED" => Some(Self::Approved),
            "CHANGES_REQUESTED" => Some(Self::ChangesRequested),
            "COMMENTED" => Some(Self::Commented),
            "DISMISSED" => Some(Self::Dismissed),
            "PENDING" => Some(Self::Pending),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Approved => "Approved",
            Self::ChangesRequested => "Changes requested",
            Self::Commented => "Commented",
            Self::Dismissed => "Dismissed",
            Self::Pending => "Pending",
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            Self::Approved => IconName::Check,
            Self::ChangesRequested => IconName::Close,
            Self::Commented => IconName::Chat,
            Self::Dismissed => IconName::Undo,
            Self::Pending => IconName::Clock,
        }
    }

    /// Orca's colours: green for an approval, red for changes requested, muted for plain comments
    /// and dismissals. The panel's own pill palette, since some themes' diff colours are too dim on a tint.
    pub fn tone(self) -> Tone {
        match self {
            Self::Approved => Tone::Ready,
            Self::ChangesRequested => Tone::Blocked,
            Self::Pending => Tone::Pending,
            Self::Commented | Self::Dismissed => Tone::Neutral,
        }
    }

    pub fn color(self, cx: &App) -> Option<gpui::Hsla> {
        crate::github_panel::tone_color(self.tone(), cx)
    }
}

pub fn render_review_state_badge(state: ReviewState, cx: &App) -> AnyElement {
    let (text, background) = match state.color(cx) {
        Some(color) => (Color::Custom(color), color.opacity(0.15)),
        None => (Color::Muted, cx.theme().colors().element_background),
    };
    h_flex()
        .flex_none()
        .gap_0p5()
        .px_1p5()
        .rounded_full()
        .bg(background)
        .child(Icon::new(state.icon()).size(IconSize::XSmall).color(text))
        .child(
            Label::new(state.label())
                .size(LabelSize::XSmall)
                .weight(gpui::FontWeight::MEDIUM)
                .color(text)
                .single_line(),
        )
        .into_any_element()
}

/// An issue comment or a review summary from `review.threads`' `conversation`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ConversationComment {
    pub id: String,
    /// `comment` or `review`.
    pub kind: String,
    pub author: Option<String>,
    pub author_avatar_url: Option<String>,
    pub is_bot: bool,
    pub body: String,
    pub url: Option<String>,
    pub created_at: Option<String>,
    pub review_state: Option<String>,
    pub reactions: Vec<ReactionSummary>,
    /// The REST id issue comment edits and deletes address.
    pub database_id: Option<u64>,
    pub viewer_can_update: bool,
    pub viewer_can_delete: bool,
}

impl ConversationComment {
    pub fn is_automated(&self) -> bool {
        is_bot(self.is_bot, self.author.as_deref())
    }

    pub fn is_review(&self) -> bool {
        self.kind == "review"
    }

    /// Not yet confirmed by GitHub, so there is nothing to act on.
    pub fn is_pending(&self) -> bool {
        self.id.starts_with("pending-")
    }

    pub fn can_edit(&self) -> bool {
        self.viewer_can_update && !self.is_pending()
    }

    /// GitHub can't delete a submitted review, only edit its summary.
    pub fn can_delete(&self) -> bool {
        self.viewer_can_delete && !self.is_review() && !self.is_pending()
    }

    /// The `kind` and id `review.comment.edit_published` and `delete_published` address it by.
    pub fn published_ref(&self) -> Value {
        if self.is_review() {
            serde_json::json!({ "kind": "review_summary", "node_id": self.id })
        } else {
            serde_json::json!({ "kind": "issue_comment", "id": self.database_id })
        }
    }

    pub fn review_state(&self) -> Option<ReviewState> {
        self.review_state.as_deref().and_then(ReviewState::parse)
    }

    /// The same comment as a thread comment, so the card's header, body and reactions draw alike.
    pub fn as_thread_comment(&self) -> ThreadComment {
        ThreadComment {
            id: self.id.clone(),
            author: self.author.clone(),
            author_avatar_url: self.author_avatar_url.clone(),
            is_bot: self.is_automated(),
            body: self.body.clone(),
            url: self.url.clone(),
            created_at: self.created_at.clone(),
            reactions: self.reactions.clone(),
            viewer_can_update: self.viewer_can_update,
            viewer_can_delete: self.viewer_can_delete,
        }
    }
}

/// One card in the Comments section.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CommentItem<'a> {
    Thread(&'a ReviewThread),
    Conversation(&'a ConversationComment),
}

impl CommentItem<'_> {
    fn latest_activity(&self) -> Option<i64> {
        match self {
            Self::Thread(thread) => latest_activity(thread),
            Self::Conversation(comment) => comment
                .created_at
                .as_deref()
                .and_then(crate::github_panel::parse_timestamp),
        }
    }
}

/// The open threads and conversation comments under a tab, newest activity first.
pub fn comment_items<'a>(
    threads: &'a [ReviewThread],
    conversation: &'a [ConversationComment],
    audience: Audience,
) -> Vec<CommentItem<'a>> {
    let mut items: Vec<CommentItem<'a>> = threads
        .iter()
        .filter(|thread| thread_in_audience(thread, audience))
        .map(CommentItem::Thread)
        .chain(
            conversation
                .iter()
                .filter(|comment| audience.includes_conversation(comment))
                .map(CommentItem::Conversation),
        )
        .collect();
    items.sort_by_key(|item| std::cmp::Reverse(item.latest_activity()));
    items
}

/// A thread shows under a tab when any of its comments counts there.
pub fn thread_in_audience(thread: &ReviewThread, audience: Audience) -> bool {
    thread
        .comments
        .iter()
        .any(|comment| audience.includes(thread, comment))
}

fn latest_activity(thread: &ReviewThread) -> Option<i64> {
    thread
        .comments
        .iter()
        .filter_map(|comment| comment.created_at.as_deref())
        .filter_map(crate::github_panel::parse_timestamp)
        .max()
}

/// Open or resolved threads, newest activity first (Orca's triage order).
pub fn threads_by_activity(threads: &[ReviewThread], resolved: bool) -> Vec<&ReviewThread> {
    let mut picked: Vec<&ReviewThread> = threads
        .iter()
        .filter(|thread| thread.is_resolved == resolved)
        .collect();
    picked.sort_by_key(|thread| std::cmp::Reverse(latest_activity(thread)));
    picked
}

/// Shows `file` in its diff tab (the open one keeps its base) and puts the cursor on 1-based `line`.
pub fn open_location(
    workspace: &mut Workspace,
    file: PathBuf,
    line: Option<u32>,
    base: DiffBase,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<DiffTab>(cx)
        .find(|tab| tab.read(cx).path() == file.as_path());
    let tab: Task<anyhow::Result<Entity<DiffTab>>> = match existing {
        Some(tab) => {
            workspace.activate_item(&tab, true, true, window, cx);
            Task::ready(Ok(tab))
        }
        None => open_diff_tab(file, base, DiffTabMode::Diff, None, workspace, window, cx),
    };
    cx.spawn_in(window, async move |_, cx| {
        let tab = tab.await?;
        if let Some(line) = line {
            tab.update_in(cx, |tab, window, cx| {
                reveal_line(&tab.editor().clone(), line, window, cx)
            })?;
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn reveal_line(editor: &Entity<Editor>, line: u32, window: &mut Window, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
            return;
        };
        let text_anchor = {
            let buffer = buffer.read(cx);
            let row = line.saturating_sub(1).min(buffer.max_point().row);
            buffer.anchor_after(Point::new(row, 0))
        };
        let Some(anchor) = editor
            .buffer()
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(text_anchor)
        else {
            return;
        };
        editor.change_selections(
            SelectionEffects::scroll(Autoscroll::center()),
            window,
            cx,
            |selections| selections.select_anchor_ranges([anchor..anchor]),
        );
    });
}

/// `open_location` for the workspace that owns `weak`, logging when it has gone away.
pub fn open_in_workspace(
    workspace: &gpui::WeakEntity<Workspace>,
    file: PathBuf,
    line: Option<u32>,
    base: DiffBase,
    window: &mut Window,
    cx: &mut App,
) {
    workspace
        .update(cx, |workspace, cx| {
            open_location(workspace, file, line, base, window, cx)
        })
        .log_err();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn thread(value: Value) -> ReviewThread {
        serde_json::from_value(value).expect("valid thread")
    }

    #[test]
    fn counts_badges_and_order_follow_open_threads() {
        let reply = json!({ "threads": [
            { "id": "a", "path": "src/export.rs", "is_resolved": false },
            { "id": "b", "path": "src/export.rs", "is_resolved": false },
            { "id": "c", "path": "src/export.rs", "is_resolved": true },
            { "id": "d", "path": "README.md" },
        ]});
        let counts = open_thread_counts(&reply);
        assert_eq!(counts.get("src/export.rs"), Some(&2));
        assert_eq!(counts.get("README.md"), Some(&1));
        assert!(open_thread_badge(0, Some(0)).is_none());
        assert!(open_thread_badge(0, None).is_none());

        let threads = vec![
            thread(
                json!({ "id": "old", "path": "src/a.rs", "placement": { "line": 4 },
                "comments": [{ "body": "x", "created_at": "2026-11-14T10:00:00Z" }] }),
            ),
            thread(
                json!({ "id": "new", "path": "src/b.rs", "placement": { "outdated": true, "line": 9 },
                "comments": [{ "body": "y", "created_at": "2026-11-13T10:00:00Z" },
                             { "body": "z", "created_at": "2026-11-15T10:00:00Z" }] }),
            ),
            thread(json!({ "id": "done", "path": "src/c.rs", "is_resolved": true })),
        ];
        let open: Vec<&str> = threads_by_activity(&threads, false)
            .iter()
            .map(|thread| thread.id.as_str())
            .collect();
        assert_eq!(open, ["new", "old"]);
        assert_eq!(threads_by_activity(&threads, true).len(), 1);
        assert_eq!(thread_location(&threads[0]), "a.rs:L4");
        assert_eq!(
            thread_line(&threads[1]),
            None,
            "an outdated thread has no line to open"
        );
        assert_eq!(thread_location(&threads[1]), "b.rs");
        assert_eq!(
            excerpt("\n![shot](x.png)\n  Round before summing?  \nmore"),
            "Round before summing?"
        );
        let draft = ReviewDraft {
            path: "src/export.rs".into(),
            start_line: Some(3),
            line: Some(5),
            ..Default::default()
        };
        assert_eq!(draft_location(&draft), "export.rs:L3-L5");
    }

    #[test]
    fn tabs_count_threads_issue_comments_and_reviews_like_orca() {
        let threads = vec![
            thread(
                json!({ "id": "t1", "path": "ci.yml", "line": null, "original_line": 206,
                "placement": { "outdated": true },
                "comments": [
                    { "id": "c1", "author": "farm-bot", "is_bot": true, "created_at": "2026-11-15T09:00:00Z" },
                    { "id": "c2", "author": "maria", "created_at": "2026-11-15T09:30:00Z" },
                ] }),
            ),
            thread(json!({ "id": "t2", "path": "a.rs", "comments": [
                { "id": "c3", "author": "coderabbitai", "created_at": "2026-11-15T08:00:00Z" } ] })),
        ];
        let conversation: Vec<ConversationComment> = serde_json::from_value(json!([
            { "id": "i1", "kind": "comment", "author": "deploy-bot", "created_at": "2026-11-15T11:00:00Z" },
            { "id": "i2", "kind": "comment", "author": "github-actions", "created_at": "2026-11-15T07:00:00Z" },
            { "id": "r1", "kind": "review", "author": "devon", "review_state": "CHANGES_REQUESTED",
              "created_at": "2026-11-15T10:00:00Z" },
        ]))
        .expect("conversation");

        let counts =
            Audience::ALL.map(|audience| audience_count(&threads, &conversation, audience));
        assert_eq!(
            counts,
            [6, 2, 4, 3],
            "all, humans, bots, comments in open threads"
        );

        let ids = |audience| -> Vec<String> {
            comment_items(&threads, &conversation, audience)
                .into_iter()
                .map(|item| match item {
                    CommentItem::Thread(thread) => thread.id.clone(),
                    CommentItem::Conversation(comment) => comment.id.clone(),
                })
                .collect()
        };
        assert_eq!(ids(Audience::All), ["i1", "r1", "t1", "t2", "i2"]);
        assert_eq!(ids(Audience::Humans), ["r1", "t1"]);
        assert_eq!(ids(Audience::Unresolved), ["t1", "t2"]);
        assert_eq!(thread_location(&threads[0]), "ci.yml:L206");

        assert!(is_bot(false, Some("renovate")));
        assert!(is_bot(false, Some("lint[bot]")));
        assert!(is_bot(false, Some("my-bot-account")));
        assert!(!is_bot(false, Some("abbott")));
        assert!(!is_bot(false, Some("maria")));
    }

    #[test]
    fn review_states_map_to_orcas_badges() {
        let cases = [
            (
                "APPROVED",
                ReviewState::Approved,
                "Approved",
                IconName::Check,
            ),
            (
                "CHANGES_REQUESTED",
                ReviewState::ChangesRequested,
                "Changes requested",
                IconName::Close,
            ),
            (
                "COMMENTED",
                ReviewState::Commented,
                "Commented",
                IconName::Chat,
            ),
            (
                "DISMISSED",
                ReviewState::Dismissed,
                "Dismissed",
                IconName::Undo,
            ),
            ("PENDING", ReviewState::Pending, "Pending", IconName::Clock),
        ];
        for (raw, state, label, icon) in cases {
            assert_eq!(ReviewState::parse(raw), Some(state));
            assert_eq!(state.label(), label);
            assert_eq!(state.icon(), icon);
        }
        assert_eq!(ReviewState::parse("approved"), None);
        let comment = ConversationComment {
            review_state: Some("APPROVED".into()),
            ..Default::default()
        };
        assert_eq!(comment.review_state(), Some(ReviewState::Approved));
        assert_eq!(ConversationComment::default().review_state(), None);
    }

    #[gpui::test]
    fn badge_colors_follow_the_theme(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            let dark = !cx.theme().appearance().is_light();
            let green = if dark { 0x34d399 } else { 0x059669 };
            let red = if dark { 0xfb7185 } else { 0xe11d48 };
            assert_eq!(
                ReviewState::Approved.color(cx),
                Some(gpui::rgb(green).into())
            );
            assert_eq!(
                ReviewState::ChangesRequested.color(cx),
                Some(gpui::rgb(red).into())
            );
            assert_eq!(ReviewState::Commented.color(cx), None);
            assert_eq!(ReviewState::Dismissed.color(cx), None);
        });
    }
}
