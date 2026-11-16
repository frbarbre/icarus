//! PR review threads outside the diff: the Changes list's per-file count, the PR panel's thread and draft
//! rows, and opening a thread's line in the file's diff tab.

use std::path::PathBuf;

use collections::HashMap;
use editor::{Editor, SelectionEffects, scroll::Autoscroll};
use gpui::{AnyElement, App, Context, Entity, Task, Window};
use language::Point;
use serde_json::Value;
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use crate::diff_tab::{DiffBase, DiffTab, DiffTabMode, open_diff_tab};
use crate::review::{ReviewDraft, ReviewThread};

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

/// Orca's path badge: `export.rs:L42`, or the bare file name for a file or outdated thread.
pub fn thread_location(thread: &ReviewThread) -> String {
    match thread_line(thread) {
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
            Self::All => IconName::Chat,
            Self::Humans => IconName::Person,
            Self::Bots => IconName::ZedAgent,
            Self::Unresolved => IconName::Circle,
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

    fn includes(self, thread: &ReviewThread, comment: &crate::review::ThreadComment) -> bool {
        match self {
            Self::All => true,
            Self::Humans => !comment.is_bot,
            Self::Bots => comment.is_bot,
            Self::Unresolved => !thread.is_resolved,
        }
    }
}

/// Comments in the tab, counted per comment as Orca counts them.
pub fn audience_count(threads: &[ReviewThread], audience: Audience) -> usize {
    threads
        .iter()
        .flat_map(|thread| {
            thread
                .comments
                .iter()
                .filter(move |comment| audience.includes(thread, comment))
        })
        .count()
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
}
