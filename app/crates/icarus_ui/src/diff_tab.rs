//! A diff tab: one changed file against the workspace's base, shown as a diff or edited in place (source-control §5.2).

use std::any::{Any, TypeId};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use editor::scroll::Autoscroll;
use editor::{Editor, EditorEvent, HiddenDiffHunkRenderer, SelectionEffects};
use git::status::{DiffTreeType, TreeDiffStatus};
use gpui::{
    AnyEntity, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Subscription, Task, WeakEntity, Window, actions,
};
use language::{Bias, Buffer, Point};
use multi_buffer::{Anchor, MultiBuffer, MultiBufferRow};
use project::{Project, ProjectPath};
use serde::{Deserialize, Serialize};
use ui::{ToggleButtonGroup, ToggleButtonSimple, prelude::*};
use util::ResultExt as _;
use workspace::item::{Item, ItemEvent, SaveOptions, SerializableItem};
use workspace::searchable::SearchableItemHandle;
use workspace::{ItemHandle as _, ItemId, ItemNavHistory, Pane, Workspace, WorkspaceId};

use crate::review::{AddReviewComment, DiffReview, OpenChangedFile};

actions!(
    icarus,
    [
        /// Shows the active diff tab as an editable file (⌥⌘E).
        ShowEdit,
        /// Shows the active file's changes against the base (⌥⌘D).
        ShowDiff,
        /// Goes to the next change in the diff (F7); stops at the last one.
        NextChange,
        /// Goes to the previous change in the diff (⇧F7); stops at the first one.
        PreviousChange,
    ]
);

/// What one press of a change key does (source-control §5.3).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ChangeStep {
    /// Move the cursor to the start of the change at this index.
    Jump(usize),
    /// Page through a change taller than the viewport: the new top row.
    Scroll(f64),
    /// At the first or last change: Orca doesn't wrap.
    Stay,
}

/// One change in display rows: everything it covers (deleted rows shown above the new ones
/// included), and the row the cursor lands on, its first new line or, for a pure deletion, the line
/// after the gap.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChangeRows {
    pub rows: Range<u32>,
    pub target: u32,
}

/// Orca's change stepping over changes sorted by position; `top` and `visible` describe the
/// viewport in display rows.
pub(crate) fn step_change(
    changes: &[ChangeRows],
    cursor: u32,
    top: f64,
    visible: f64,
    forward: bool,
) -> ChangeStep {
    let contains = |change: &ChangeRows| {
        if change.rows.is_empty() {
            change.target == cursor
        } else {
            change.rows.contains(&cursor)
        }
    };
    let current = changes.iter().position(contains);
    let page = (visible * 0.85).floor().max(1.0);
    if forward {
        if let Some(change) = current.map(|index| &changes[index]) {
            let last_row = change.rows.end.saturating_sub(1).max(change.target);
            if f64::from(last_row) > top + visible - 1.0 {
                return ChangeStep::Scroll(top + page);
            }
        }
        changes
            .iter()
            .position(|change| change.target > cursor)
            .map_or(ChangeStep::Stay, ChangeStep::Jump)
    } else {
        if let Some(index) = current {
            let change = &changes[index];
            if f64::from(change.rows.start) < top {
                return ChangeStep::Scroll((top - page).max(0.0));
            }
            if cursor > change.target {
                return ChangeStep::Jump(index);
            }
        }
        changes
            .iter()
            .rposition(|change| change.target < cursor)
            .map_or(ChangeStep::Stay, ChangeStep::Jump)
    }
}

/// What the file is compared against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffBase {
    /// The merge base with this ref (`origin/main` and so on), against the file on disk.
    MergeBase(String),
    /// The last commit, so only uncommitted edits show.
    Uncommitted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffTabMode {
    Diff,
    Edit,
}

pub struct DiffTab {
    path: PathBuf,
    base: DiffBase,
    mode: DiffTabMode,
    project: Entity<Project>,
    project_path: ProjectPath,
    buffer: Entity<Buffer>,
    editor: Entity<Editor>,
    /// Review drafts, PR threads and the comment box; only when Icarus runs.
    review: Option<Entity<DiffReview>>,
    _review_changes: Option<Vec<Subscription>>,
    _editor_events: Subscription,
}

impl DiffTab {
    /// Redraws on review changes, and opens the diff of a file a note's answer changed.
    fn watch_review(
        review: &Entity<DiffReview>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        vec![
            cx.observe(review, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                review,
                window,
                |this, _, event: &OpenChangedFile, window, cx| {
                    this.open_changed_file(&event.0, window, cx)
                },
            ),
        ]
    }

    fn open_changed_file(&mut self, relative: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self
            .project
            .read(cx)
            .worktree_for_id(self.project_path.worktree_id, cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        else {
            return;
        };
        let Some(workspace) = Workspace::for_window(window, cx) else {
            return;
        };
        let path = root.join(relative);
        let base = self.base.clone();
        // Deferred: the click arrives while this tab is mid-update.
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                open_diff_tab(path, base, DiffTabMode::Diff, None, workspace, window, cx)
                    .detach_and_log_err(cx);
            });
        });
    }

    fn new(
        path: PathBuf,
        base: DiffBase,
        mode: DiffTabMode,
        project: Entity<Project>,
        project_path: ProjectPath,
        buffer: Entity<Buffer>,
        diff: Entity<BufferDiff>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::singleton(buffer.clone(), cx);
            multibuffer.add_diff(diff, cx);
            multibuffer
        });
        let editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx);
            // Any renderer stops the editor from replacing this base diff with its own HEAD diff.
            editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
            editor.register_addon(crate::review::ReviewAttached::default());
            editor
        });
        let editor_events = cx.subscribe(&editor, |_, _, event: &EditorEvent, cx| {
            cx.emit(event.clone())
        });
        let root = project
            .read(cx)
            .worktree_for_id(project_path.worktree_id, cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf());
        let review = root.filter(|_| crate::enabled(cx)).map(|root| {
            let relative = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let editor = editor.downgrade();
            let buffer = buffer.clone();
            let root = root.to_string_lossy().into_owned();
            let client = crate::routing::client_for_project(project.read(cx), cx);
            cx.new(|cx| DiffReview::new(editor, buffer, root, relative, client, cx))
        });
        let review_changes = review
            .as_ref()
            .map(|review| Self::watch_review(review, window, cx));
        let mut tab = Self {
            path,
            base,
            mode,
            project,
            project_path,
            buffer,
            editor,
            review,
            _review_changes: review_changes,
            _editor_events: editor_events,
        };
        tab.apply_mode(cx);
        tab
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn base(&self) -> &DiffBase {
        &self.base
    }

    pub fn mode(&self) -> DiffTabMode {
        self.mode
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn review(&self) -> Option<&Entity<DiffReview>> {
        self.review.as_ref()
    }

    /// The review layer for tests and previews, which run without `icarus_ui::init`.
    pub fn enable_review(&mut self, workspace_key: String, path: String, cx: &mut Context<Self>) {
        let editor = self.editor.downgrade();
        let buffer = self.buffer.clone();
        let client = crate::review::app_client(cx);
        let review = cx.new(|cx| DiffReview::new(editor, buffer, workspace_key, path, client, cx));
        self._review_changes = Some(vec![cx.observe(&review, |_, _, cx| cx.notify())]);
        self.review = Some(review);
    }

    /// One press of the next/previous change key (source-control §5.3).
    pub fn step_change(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.snapshot(window, cx);
            let buffer = snapshot.buffer_snapshot();
            let display_row =
                |point: Point, bias: Bias| snapshot.point_to_display_point(point, bias).row().0;
            let mut destinations = Vec::new();
            let mut changes = Vec::new();
            for hunk in buffer.diff_hunks_in_range(Point::zero()..buffer.max_point()) {
                let start_row = display_row(Point::new(hunk.row_range.start.0, 0), Bias::Left);
                let end_row = if hunk.row_range.is_empty() {
                    start_row
                } else {
                    let last = MultiBufferRow(hunk.row_range.end.0 - 1);
                    display_row(Point::new(last.0, buffer.line_len(last)), Bias::Right) + 1
                };
                // The new text's start: the deleted rows shown above it aren't part of the file.
                let first_new_row = buffer
                    .row_infos(hunk.row_range.start)
                    .zip(hunk.row_range.start.0..)
                    .take_while(|(_, row)| {
                        *row < hunk.row_range.end.0.max(hunk.row_range.start.0 + 1)
                    })
                    .find(|(info, _)| !info.diff_status.is_some_and(|status| status.is_deleted()))
                    .map_or(hunk.row_range.end.0, |(_, row)| row);
                let destination = Point::new(first_new_row, 0);
                destinations.push(destination);
                changes.push(ChangeRows {
                    rows: start_row..end_row,
                    target: display_row(destination, Bias::Left),
                });
            }
            let head = editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head();
            let cursor = display_row(head, Bias::Left);
            let top = editor.scroll_position(cx).y;
            let visible = editor.visible_line_count().unwrap_or(1.0);
            match step_change(&changes, cursor, top, visible, forward) {
                ChangeStep::Jump(index) => {
                    let Some(&destination) = destinations.get(index) else {
                        return;
                    };
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| selections.select_ranges([destination..destination]),
                    );
                }
                ChangeStep::Scroll(new_top) => {
                    let x = editor.scroll_position(cx).x;
                    editor.set_scroll_position(gpui::point(x, new_top), window, cx);
                }
                ChangeStep::Stay => {}
            }
        });
    }

    fn add_review_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(review) = &self.review {
            review.update(cx, |review, cx| review.open_on_selection(window, cx));
        }
    }

    pub fn set_mode(&mut self, mode: DiffTabMode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            self.apply_mode(cx);
            cx.emit(EditorEvent::TitleChanged);
            cx.notify();
        }
    }

    fn apply_mode(&mut self, cx: &mut Context<Self>) {
        let mode = self.mode;
        self.editor.update(cx, |editor, cx| {
            editor.buffer().update(cx, |buffer, cx| match mode {
                DiffTabMode::Diff => {
                    buffer.set_all_diff_hunks_expanded(cx);
                    buffer.expand_diff_hunks(vec![Anchor::Min..Anchor::Max], cx);
                }
                DiffTabMode::Edit => {
                    buffer.set_all_diff_hunks_collapsed(cx);
                    buffer.collapse_diff_hunks(vec![Anchor::Min..Anchor::Max], cx);
                }
            });
        });
    }

    /// Points the tab at another base, reloading its diff in place.
    fn set_base(&mut self, base: DiffBase, window: &mut Window, cx: &mut Context<Self>) {
        if self.base == base {
            return;
        }
        self.base = base.clone();
        let load = load_diff(&self.project, &self.buffer, &self.project_path, &base, cx);
        cx.spawn_in(window, async move |this, cx| {
            let diff = load.await?;
            this.update(cx, |this, cx| {
                this.editor.update(cx, |editor, cx| {
                    editor
                        .buffer()
                        .update(cx, |multibuffer, cx| multibuffer.add_diff(diff, cx));
                });
                this.apply_mode(cx);
                cx.emit(EditorEvent::TitleChanged);
            })
        })
        .detach_and_log_err(cx);
    }
}

/// The diff of `buffer` against `base`: the merge base's blob when the file changed since, else its last commit.
fn load_diff(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    project_path: &ProjectPath,
    base: &DiffBase,
    cx: &mut App,
) -> Task<Result<Entity<BufferDiff>>> {
    let uncommitted = |project: Entity<Project>, buffer: Entity<Buffer>, cx: &mut App| {
        project.update(cx, |project, cx| project.open_uncommitted_diff(buffer, cx))
    };
    let DiffBase::MergeBase(base_ref) = base else {
        return uncommitted(project.clone(), buffer.clone(), cx);
    };
    let git_store = project.read(cx).git_store().clone();
    let Some((repo, repo_path)) = git_store
        .read(cx)
        .repository_and_path_for_project_path(project_path, cx)
    else {
        return uncommitted(project.clone(), buffer.clone(), cx);
    };
    let tree = repo.update(cx, |repo, cx| {
        repo.diff_tree(
            DiffTreeType::MergeBaseWithWorktree {
                base: base_ref.clone().into(),
            },
            cx,
        )
    });
    let project = project.clone();
    let buffer = buffer.clone();
    cx.spawn(async move |cx| {
        let tree = tree
            .await
            .context("git stopped before the diff was read")??;
        // `Some(None)` is a file added since the base, which diffs against nothing.
        let old = match tree.entries.get(&repo_path) {
            Some(TreeDiffStatus::Added) => Some(None),
            Some(TreeDiffStatus::Modified { old } | TreeDiffStatus::Deleted { old }) => {
                Some(Some(*old))
            }
            None => None,
        };
        match old {
            Some(oid) => {
                git_store
                    .update(cx, |git_store, cx| {
                        git_store.open_diff_since(oid, buffer, repo, cx)
                    })
                    .await
            }
            None => cx.update(|cx| uncommitted(project, buffer, cx)).await,
        }
    })
}

/// Opens `path`'s diff tab in `pane` (the active pane by default), or brings back the one already open.
pub fn open_diff_tab(
    path: PathBuf,
    base: DiffBase,
    mode: DiffTabMode,
    pane: Option<Entity<Pane>>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<Entity<DiffTab>>> {
    let existing = workspace
        .items_of_type::<DiffTab>(cx)
        .find(|tab| tab.read(cx).path == path);
    if let Some(existing) = existing {
        existing.update(cx, |tab, cx| {
            tab.set_base(base, window, cx);
            tab.set_mode(mode, cx);
        });
        workspace.activate_item(&existing, true, true, window, cx);
        return Task::ready(Ok(existing));
    }
    let project = workspace.project().clone();
    let Some(project_path) = project.read(cx).find_project_path(&path, cx) else {
        return Task::ready(Err(anyhow::anyhow!(
            "{} is outside the workspace",
            path.display()
        )));
    };
    let pane = pane.unwrap_or_else(|| workspace.active_pane().clone());
    let open_buffer = project.update(cx, |project, cx| {
        project.open_buffer(project_path.clone(), cx)
    });
    cx.spawn_in(window, async move |workspace, cx| {
        let buffer = open_buffer.await?;
        let diff = cx
            .update(|_, cx| load_diff(&project, &buffer, &project_path, &base, cx))?
            .await?;
        workspace.update_in(cx, |workspace, window, cx| {
            let tab = cx.new(|cx| {
                DiffTab::new(
                    path,
                    base,
                    mode,
                    project,
                    project_path,
                    buffer,
                    diff,
                    window,
                    cx,
                )
            });
            workspace.add_item(pane, Box::new(tab.clone()), None, true, true, window, cx);
            tab
        })
    })
}

pub fn register(cx: &mut App) {
    workspace::register_serializable_item::<DiffTab>(cx);
    // From the palette or another app the tab isn't focused, so the workspace forwards the action.
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &AddReviewComment, window, cx| {
            let tab = workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<DiffTab>());
            if let Some(tab) = tab {
                window.defer(cx, move |window, cx| {
                    tab.update(cx, |tab, cx| tab.add_review_comment(window, cx));
                });
            }
        });
        workspace.register_action(|workspace, _: &NextChange, window, cx| {
            step_change_outside_diff(true, workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &PreviousChange, window, cx| {
            step_change_outside_diff(false, workspace, window, cx);
        });
    })
    .detach();
}

/// A change key pressed outside the diff editor focuses the diff and steps there (Orca's
/// diff-shortcuts-outside-editor): only while a diff is the active tab or Source Control is showing.
fn step_change_outside_diff(
    forward: bool,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let active = workspace
        .active_item(cx)
        .and_then(|item| item.downcast::<DiffTab>());
    let tab = active.or_else(|| {
        crate::changes_tab::source_control_showing(workspace, window, cx)
            .then(|| workspace.items_of_type::<DiffTab>(cx).next())
            .flatten()
    });
    let Some(tab) = tab else {
        return;
    };
    workspace.activate_item(&tab, true, true, window, cx);
    window.defer(cx, move |window, cx| {
        tab.update(cx, |tab, cx| tab.step_change(forward, window, cx));
    });
}

impl EventEmitter<EditorEvent> for DiffTab {}

impl Focusable for DiffTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for DiffTab {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
            .into()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        let against = match &self.base {
            DiffBase::MergeBase(base) => format!("vs {base}"),
            DiffBase::Uncommitted => "uncommitted".to_string(),
        };
        Some(format!("{} ({against})", self.path.display()).into())
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _cx: &'a App,
    ) -> Option<AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.clone().into())
        } else {
            None
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.for_each_project_item(cx, f)
    }

    fn active_project_path(&self, _cx: &App) -> Option<ProjectPath> {
        Some(self.project_path.clone())
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.buffer.read(cx).is_dirty()
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.buffer.read(cx).has_conflict()
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor
            .update(cx, |editor, _| editor.set_nav_history(Some(nav_history)));
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }

    fn can_save(&self, cx: &App) -> bool {
        self.editor.read(cx).can_save(cx)
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.save(options, project, window, cx)
    }
}

impl Render for DiffTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.mode;
        let against: SharedString = match &self.base {
            DiffBase::MergeBase(base) => format!("vs {base}").into(),
            DiffBase::Uncommitted => "Uncommitted".into(),
        };
        let weak = cx.entity().downgrade();
        let switch = move |target: DiffTabMode, weak: WeakEntity<DiffTab>| {
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                weak.update(cx, |tab, cx| tab.set_mode(target, cx))
                    .log_err();
            }
        };
        let mut key_context = gpui::KeyContext::default();
        key_context.add("IcarusDiffTab");
        // Remapped bare keys (Orca's ↑/↓ for changes) apply only while the diff shows, not while editing.
        if mode == DiffTabMode::Diff {
            key_context.add("diff_mode");
        }
        v_flex()
            .key_context(key_context)
            .size_full()
            .on_action(
                cx.listener(|this, _: &ShowEdit, _, cx| this.set_mode(DiffTabMode::Edit, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ShowDiff, _, cx| this.set_mode(DiffTabMode::Diff, cx)),
            )
            .on_action(
                cx.listener(|this, _: &NextChange, window, cx| this.step_change(true, window, cx)),
            )
            .on_action(cx.listener(|this, _: &PreviousChange, window, cx| {
                this.step_change(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &AddReviewComment, window, cx| {
                this.add_review_comment(window, cx)
            }))
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        ToggleButtonGroup::single_row(
                            "diff-tab-mode",
                            [
                                ToggleButtonSimple::new(
                                    "Edit",
                                    switch(DiffTabMode::Edit, weak.clone()),
                                ),
                                ToggleButtonSimple::new("Changes", switch(DiffTabMode::Diff, weak)),
                            ],
                        )
                        .selected_index(match mode {
                            DiffTabMode::Edit => 0,
                            DiffTabMode::Diff => 1,
                        })
                        .style(ui::ToggleButtonGroupStyle::Outlined)
                        .label_size(LabelSize::Small),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .when_some(
                                self.review
                                    .as_ref()
                                    .and_then(|review| review.read(cx).error().map(str::to_owned)),
                                |row, error| {
                                    row.child(
                                        Label::new(error)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Error)
                                            .truncate(),
                                    )
                                },
                            )
                            .when(self.review.is_some(), |row| {
                                row.child(
                                    Button::new("diff-tab-comment", "Comment")
                                        .style(ButtonStyle::Subtle)
                                        .label_size(LabelSize::Small)
                                        .start_icon(
                                            Icon::new(IconName::Chat).size(IconSize::XSmall),
                                        )
                                        .tooltip(|_window, cx| {
                                            ui::Tooltip::for_action(
                                                "Comment on the selected lines",
                                                &AddReviewComment,
                                                cx,
                                            )
                                        })
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.add_review_comment(window, cx)
                                        })),
                                )
                            })
                            .child(
                                Label::new(against)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    ),
            )
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }
}

#[derive(Serialize, Deserialize)]
struct SavedDiffTab {
    path: PathBuf,
    base: DiffBase,
    mode: DiffTabMode,
}

fn serialized_key(workspace_id: WorkspaceId, item_id: ItemId) -> String {
    format!("icarus-diff-tab:{}:{item_id}", i64::from(workspace_id))
}

impl SerializableItem for DiffTab {
    fn serialized_item_kind() -> &'static str {
        "IcarusDiffTab"
    }

    fn cleanup(
        _workspace_id: WorkspaceId,
        _alive_items: Vec<ItemId>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn deserialize(
        project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let saved =
            db::kvp::KeyValueStore::global(cx).read_kvp(&serialized_key(workspace_id, item_id));
        window.spawn(cx, async move |cx| {
            let saved: SavedDiffTab = serde_json::from_str(
                &saved?.ok_or_else(|| anyhow::anyhow!("no file recorded for this diff tab"))?,
            )?;
            let project_path = cx
                .update(|_, cx| project.read(cx).find_project_path(&saved.path, cx))?
                .ok_or_else(|| anyhow::anyhow!("{} is gone", saved.path.display()))?;
            let buffer = cx
                .update(|_, cx| {
                    project.update(cx, |project, cx| {
                        project.open_buffer(project_path.clone(), cx)
                    })
                })?
                .await?;
            let diff = cx
                .update(|_, cx| load_diff(&project, &buffer, &project_path, &saved.base, cx))?
                .await?;
            cx.update(|window, cx| {
                cx.new(|cx| {
                    DiffTab::new(
                        saved.path,
                        saved.base,
                        saved.mode,
                        project,
                        project_path,
                        buffer,
                        diff,
                        window,
                        cx,
                    )
                })
            })
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let saved = serde_json::to_string(&SavedDiffTab {
            path: self.path.clone(),
            base: self.base.clone(),
            mode: self.mode,
        })
        .log_err()?;
        let store = db::kvp::KeyValueStore::global(cx);
        Some(cx.background_spawn(async move {
            store
                .write_kvp(serialized_key(workspace_id, item_id), saved)
                .await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(
            event,
            EditorEvent::TitleChanged | EditorEvent::Saved | EditorEvent::BufferEdited
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A workspace on a git repo whose merge base had `src/app.rs` as "old" and lacked `src/new.rs`.
    pub(crate) async fn git_workspace(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<Workspace>, gpui::VisualTestContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            cx.set_global(db::AppDatabase::test_new());
            crate::changes_tab::register(cx);
            crate::sidebar::register(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({
                ".git": {},
                "src": { "app.rs": "fn main() {}\nnew line\n", "new.rs": "added\n" },
            }),
        )
        .await;
        let dot_git = Path::new("/project/.git");
        fs.set_head_and_index_for_repo(
            dot_git,
            &[("src/app.rs", "fn main() {}\nnew line\n".to_string())],
        );
        fs.set_merge_base_content_for_repo(
            dot_git,
            &[("src/app.rs", "fn main() {}\nold line\n".to_string())],
        );
        let project = project::Project::test(fs, [Path::new("/project")], cx).await;
        let multi_workspace = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let cx = gpui::VisualTestContext::from_window(multi_workspace.into(), cx);
        cx.run_until_parked();
        (workspace, cx)
    }

    fn open(
        workspace: &Entity<Workspace>,
        path: &str,
        base: DiffBase,
        cx: &mut gpui::VisualTestContext,
    ) -> Task<Result<Entity<DiffTab>>> {
        workspace.update_in(cx, |workspace, window, cx| {
            open_diff_tab(
                PathBuf::from(path),
                base,
                DiffTabMode::Diff,
                None,
                workspace,
                window,
                cx,
            )
        })
    }

    #[gpui::test]
    async fn a_diff_tab_shows_the_merge_base_and_toggles_to_editing(cx: &mut gpui::TestAppContext) {
        let (workspace, mut cx) = git_workspace(cx).await;
        let cx = &mut cx;
        let base = DiffBase::MergeBase("origin/main".into());
        let tab = open(&workspace, "/project/src/app.rs", base.clone(), cx);
        cx.run_until_parked();
        let tab = tab.await.expect("diff tab");
        cx.run_until_parked();

        let shown = |cx: &mut gpui::VisualTestContext| {
            tab.update(cx, |tab, cx| {
                tab.editor.update(cx, |editor, cx| editor.display_text(cx))
            })
        };
        let diff = shown(cx);
        assert!(
            diff.contains("old line") && diff.contains("new line"),
            "diff mode shows the base's line next to the new one: {diff:?}"
        );

        cx.dispatch_action(ShowEdit);
        cx.run_until_parked();
        assert_eq!(tab.read_with(cx, |tab, _| tab.mode()), DiffTabMode::Edit);
        let edit = shown(cx);
        assert!(
            !edit.contains("old line") && edit.contains("new line"),
            "edit mode is the file alone: {edit:?}"
        );

        cx.dispatch_action(ShowDiff);
        cx.run_until_parked();
        assert_eq!(tab.read_with(cx, |tab, _| tab.mode()), DiffTabMode::Diff);

        let again = open(&workspace, "/project/src/app.rs", base, cx);
        cx.run_until_parked();
        let again = again.await.expect("same tab");
        assert_eq!(again, tab, "reopening a file reuses its diff tab");
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<DiffTab>(cx).count(), 1);
        });
    }

    #[gpui::test]
    async fn an_added_file_diffs_against_nothing(cx: &mut gpui::TestAppContext) {
        let (workspace, mut cx) = git_workspace(cx).await;
        let cx = &mut cx;
        let tab = open(
            &workspace,
            "/project/src/new.rs",
            DiffBase::MergeBase("origin/main".into()),
            cx,
        );
        cx.run_until_parked();
        let tab = tab.await.expect("diff tab");
        cx.run_until_parked();
        let hunks = tab.update(cx, |tab, cx| {
            let snapshot = tab.editor.read(cx).buffer().read(cx).snapshot(cx);
            snapshot.diff_hunks().count()
        });
        assert_eq!(hunks, 1, "the whole file is one added hunk");
    }

    #[gpui::test]
    async fn a_diff_tab_restores_its_file_base_and_mode(cx: &mut gpui::TestAppContext) {
        let (workspace, mut cx) = git_workspace(cx).await;
        let cx = &mut cx;
        let tab = open(&workspace, "/project/src/app.rs", DiffBase::Uncommitted, cx);
        cx.run_until_parked();
        let tab = tab.await.expect("diff tab");
        tab.update(cx, |tab, cx| tab.set_mode(DiffTabMode::Edit, cx));
        let (project, workspace_id, item_id, save) = workspace.update_in(cx, |workspace, _, cx| {
            workspace.set_random_database_id();
            let item_id = tab.entity_id().as_u64() as ItemId;
            let save = tab.update(cx, |tab, cx| tab.serialize(workspace, item_id, false, cx));
            (
                workspace.project().clone(),
                workspace.database_id().expect("database id"),
                item_id,
                save,
            )
        });
        save.expect("saved").await.expect("save");
        let restore = workspace.update_in(cx, |workspace, window, cx| {
            DiffTab::deserialize(
                project,
                workspace.weak_handle(),
                workspace_id,
                item_id,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let restored = restore.await.expect("restore");
        restored.read_with(cx, |restored, _| {
            assert_eq!(restored.path(), Path::new("/project/src/app.rs"));
            assert_eq!(restored.base(), &DiffBase::Uncommitted);
            assert_eq!(restored.mode(), DiffTabMode::Edit);
        });
    }

    fn thread_json(id: &str, line: u32, resolved: bool) -> serde_json::Value {
        json!({
            "id": id, "path": "src/app.rs", "line": line, "is_resolved": resolved,
            "viewer_can_resolve": true, "viewer_can_unresolve": true, "viewer_can_reply": true,
            "comments": [{ "id": format!("{id}-c1"), "author": "alice", "body": "Why this?",
                "reactions": [{ "content": "+1", "count": 2, "viewer_has_reacted": false }] }],
            "placement": { "file": false, "outdated": false, "line": line, "label": format!("Line {line}") },
        })
    }

    #[gpui::test]
    async fn review_comments_queue_edit_reply_resolve_and_reach_the_agent(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::review::tests::{calls_to, stub_rpc};
        use crate::review::{ComposerTarget, Destination, SubmitReviewComment};

        let (workspace, mut cx) = git_workspace(cx).await;
        let cx = &mut cx;
        let drafts = std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        let stored = drafts.clone();
        let calls = cx.update(|_, cx| {
            stub_rpc(cx, move |method, params| {
                let mut drafts = stored.lock().unwrap_or_else(|p| p.into_inner());
                match method {
                    "review.threads" => Ok(json!({ "threads": [thread_json("T1", 1, false), thread_json("T2", 2, true)] })),
                    "review.comment.list" => Ok(json!({ "drafts": *drafts })),
                    "review.comment.add" if params["body"] == "fail" => {
                        Err(anyhow::anyhow!("Pull request review thread line must be part of the diff"))
                    }
                    "review.comment.add" => {
                        drafts.push(json!({ "id": "d1", "path": params["path"], "line": params["line"],
                            "subject": "line", "body": params["body"] }));
                        Ok(json!({}))
                    }
                    _ => Ok(json!({})),
                }
            })
        });
        let tab = open(&workspace, "/project/src/app.rs", DiffBase::Uncommitted, cx);
        cx.run_until_parked();
        let tab = tab.await.expect("diff tab");
        let review = tab.update(cx, |tab, cx| {
            tab.enable_review("/project".into(), "src/app.rs".into(), cx);
            tab.review().cloned().expect("review")
        });
        review.update(cx, |review, cx| review.reload(cx));
        cx.run_until_parked();
        review.read_with(cx, |review, _| {
            assert_eq!(review.threads().len(), 2);
            assert!(review.drafts().is_empty());
        });
        assert_eq!(
            calls_to(&calls, "review.threads")[0],
            json!({ "workspace": "/project", "path": "src/app.rs" })
        );

        // ⌘⇧A opens the box on the cursor's line.
        let focus = tab.read_with(cx, |tab, cx| tab.editor().focus_handle(cx));
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.dispatch_action(crate::review::AddReviewComment);
        cx.run_until_parked();
        let composer = review.read_with(cx, |review, _| review.composer().cloned().expect("open"));
        assert_eq!(
            composer.read_with(cx, |composer, _| composer.target().clone()),
            ComposerTarget::Lines { start: 1, end: 1 }
        );

        // A failed add keeps the box and shows GitHub's words; a good one queues a draft.
        let type_text = |text: &str, cx: &mut gpui::VisualTestContext| {
            let editor = composer.read_with(cx, |composer, _| composer.editor().clone());
            editor.update_in(cx, |editor, window, cx| editor.set_text(text, window, cx));
        };
        type_text("fail", cx);
        review.update_in(cx, |review, window, cx| {
            review.submit_composer(Destination::Review, window, cx)
        });
        cx.run_until_parked();
        assert!(review.read_with(cx, |review, _| review.composer().is_some()));
        assert_eq!(
            composer.read_with(cx, |composer, _| composer.error_text()),
            Some("Pull request review thread line must be part of the diff".to_owned())
        );
        type_text("Rename this", cx);
        let focus = composer.read_with(cx, |composer, cx| composer.editor().focus_handle(cx));
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.dispatch_action(SubmitReviewComment);
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "review.comment.add").last().cloned(),
            Some(
                json!({ "workspace": "/project", "path": "src/app.rs", "line": 1,
                "start_line": null, "body": "Rename this" })
            )
        );
        review.read_with(cx, |review, _| {
            assert!(review.composer().is_none(), "the box closes after queueing");
            assert_eq!(review.drafts().len(), 1, "the draft shows inline");
        });

        // Editing a draft, replying, resolving, reacting and handing a thread to the agent.
        review.update_in(cx, |review, window, cx| {
            review.open_composer(
                ComposerTarget::EditDraft { id: "d1".into() },
                "Rename this, please".into(),
                window,
                cx,
            );
            review.submit_composer(Destination::Review, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "review.comment.edit"),
            [json!({ "workspace": "/project", "id": "d1", "body": "Rename this, please" })]
        );
        review.update_in(cx, |review, window, cx| {
            review.open_composer(
                ComposerTarget::Reply {
                    thread: "T1".into(),
                },
                "Because".into(),
                window,
                cx,
            );
            review.submit_composer(Destination::Review, window, cx);
            review.set_resolved("T1", true, cx);
            review.toggle_reaction("T1-c1", "+1", false, cx);
            review.add_to_agent("T2", cx);
            review.remove_draft("d1", cx);
        });
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "review.reply"),
            [json!({ "workspace": "/project", "thread": "T1", "body": "Because" })]
        );
        assert_eq!(
            calls_to(&calls, "review.resolve"),
            [json!({ "workspace": "/project", "thread": "T1" })]
        );
        assert_eq!(calls_to(&calls, "review.react")[0]["reaction"], "+1");
        assert_eq!(
            calls_to(&calls, "attach.pr_comments"),
            [json!({ "workspace": "/project", "ids": ["T2"] })]
        );
        assert_eq!(calls_to(&calls, "review.comment.remove")[0]["id"], "d1");

        // The Agent side of the box writes a note on those lines.
        review.update_in(cx, |review, window, cx| {
            review.open_composer(
                ComposerTarget::Lines { start: 1, end: 2 },
                "Check both lines".into(),
                window,
                cx,
            );
            review.submit_composer(Destination::Agent, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "note.add"),
            [
                json!({ "cwd": "/project", "path": "src/app.rs", "line": 1, "end_line": 2,
                "body": "Check both lines" })
            ]
        );
    }

    #[gpui::test]
    async fn note_replies_think_then_answer_and_their_changed_files_open(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::review::tests::{calls_to, stub_rpc};
        use crate::review::{ComposerTarget, Destination, OpenChangedFile, review_changed};

        let (workspace, mut cx) = git_workspace(cx).await;
        let cx = &mut cx;
        cx.update(|_, cx| cx.set_global(crate::IcarusEnabled));
        let notes = std::sync::Arc::new(std::sync::Mutex::new(vec![json!({
            "id": "n1", "path": "src/app.rs", "line": 1, "end_line": 1, "author": "user",
            "body": "Why main?", "resolved": false, "replies": [],
        })]));
        let stored = notes.clone();
        let calls = cx.update(|_, cx| {
            stub_rpc(cx, move |method, params| {
                let mut notes = stored.lock().unwrap_or_else(|p| p.into_inner());
                match method {
                    "note.list" => Ok(json!(*notes)),
                    "note.reply" => {
                        let replies = notes[0]["replies"].as_array_mut().expect("replies");
                        replies
                            .push(json!({ "id": "r1", "author": "user", "body": params["body"] }));
                        replies.push(json!({ "id": "r2", "author": "reply-agent", "body": "",
                            "status": "running" }));
                        Ok(json!({}))
                    }
                    _ => Ok(json!({})),
                }
            })
        });
        let tab = open(&workspace, "/project/src/app.rs", DiffBase::Uncommitted, cx);
        cx.run_until_parked();
        let tab = tab.await.expect("diff tab");
        let review = tab.read_with(cx, |tab, _| tab.review().cloned().expect("notes layer"));
        review.update(cx, |review, cx| review.reload(cx));
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "note.list")[0],
            json!({ "cwd": "/project", "path": "src/app.rs", "include_resolved": true })
        );
        review.read_with(cx, |review, _| {
            assert_eq!(review.notes().len(), 1);
            assert!(!review.notes()[0].by_agent());
        });

        // A reply goes to note.reply; the throwaway session shows as thinking.
        review.update_in(cx, |review, window, cx| {
            review.open_composer(
                ComposerTarget::NoteReply { note: "n1".into() },
                "What does line 1 do?".into(),
                window,
                cx,
            );
            review.submit_composer(Destination::Review, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "note.reply"),
            [json!({ "id": "n1", "body": "What does line 1 do?" })]
        );
        review.read_with(cx, |review, _| {
            let replies = &review.notes()[0].replies;
            assert_eq!(replies.len(), 2);
            assert!(replies[1].is_thinking(), "{replies:?}");
        });

        // The server finishes the answer and announces it; the thread shows it with its files.
        {
            let mut notes = notes.lock().unwrap_or_else(|p| p.into_inner());
            notes[0]["replies"][1] = json!({ "id": "r2", "author": "reply-agent", "status": "done",
                "body": "It is the entry point.", "changed_files": ["src/new.rs"] });
        }
        cx.update(|_, cx| review_changed(cx));
        cx.run_until_parked();
        review.read_with(cx, |review, _| {
            let answer = &review.notes()[0].replies[1];
            assert!(!answer.is_thinking());
            assert_eq!(answer.body, "It is the entry point.");
            assert_eq!(answer.changed_files, ["src/new.rs"]);
        });

        // Clicking a changed file opens its diff tab.
        review.update(cx, |_, cx| cx.emit(OpenChangedFile("src/new.rs".into())));
        cx.run_until_parked();
        let opened: Vec<PathBuf> = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<DiffTab>(cx)
                .map(|tab| tab.read(cx).path().to_path_buf())
                .collect()
        });
        assert!(
            opened.contains(&PathBuf::from("/project/src/new.rs")),
            "{opened:?}"
        );

        // A failed answer retries its question; edit, resolve, delete and add to agent.
        {
            let mut notes = notes.lock().unwrap_or_else(|p| p.into_inner());
            notes[0]["replies"][1] = json!({ "id": "r2", "author": "reply-agent",
                "status": "failed", "error": "timed out" });
        }
        cx.update(|_, cx| review_changed(cx));
        cx.run_until_parked();
        review.update_in(cx, |review, window, cx| {
            review.retry_note_reply("n1", "r2", cx);
            review.open_composer(
                ComposerTarget::EditNote {
                    note: "n1".into(),
                    reply: None,
                },
                "Why is this main?".into(),
                window,
                cx,
            );
            review.submit_composer(Destination::Review, window, cx);
            review.set_note_resolved("n1", true, cx);
            review.add_note_to_agent("n1", cx);
            review.remove_note("n1", Some("r1"), cx);
        });
        cx.run_until_parked();
        let removed = calls_to(&calls, "note.remove");
        assert_eq!(removed[0], json!({ "id": "n1", "reply_id": "r2" }));
        assert_eq!(removed[1], json!({ "id": "n1", "reply_id": "r1" }));
        assert_eq!(
            calls_to(&calls, "note.reply").last().cloned(),
            Some(json!({ "id": "n1", "body": "What does line 1 do?" })),
            "retry asks the same question again"
        );
        assert_eq!(
            calls_to(&calls, "note.edit"),
            [json!({ "id": "n1", "reply_id": null, "body": "Why is this main?" })]
        );
        assert_eq!(calls_to(&calls, "note.resolve"), [json!({ "id": "n1" })]);
        assert_eq!(calls_to(&calls, "attach.note"), [json!({ "id": "n1" })]);
    }

    #[test]
    fn change_steps_stop_at_the_ends_and_page_through_tall_changes() {
        // Rows 2..4, a deletion at 10, and a 50-row change at 20; the viewport shows 20 rows.
        let changes = [2..4, 10..10, 20..70].map(|rows| ChangeRows {
            target: rows.start,
            rows,
        });
        let step = |cursor, top, forward| step_change(&changes, cursor, top, 20.0, forward);
        assert_eq!(step(0, 0.0, true), ChangeStep::Jump(0));
        assert_eq!(step(2, 0.0, true), ChangeStep::Jump(1));
        assert_eq!(
            step(10, 0.0, true),
            ChangeStep::Jump(2),
            "a deletion sits on the row after its gap"
        );
        assert_eq!(
            step(20, 10.0, true),
            ChangeStep::Scroll(27.0),
            "a tall change pages by 85% of the viewport first"
        );
        assert_eq!(
            step(20, 51.0, true),
            ChangeStep::Stay,
            "once its end shows there is no next change, and no wrap"
        );
        assert_eq!(step(0, 0.0, false), ChangeStep::Stay, "no wrap backwards");
        assert_eq!(
            step(20, 27.0, false),
            ChangeStep::Scroll(10.0),
            "paging back up a tall change"
        );
        assert_eq!(step(20, 15.0, false), ChangeStep::Jump(1));
        assert_eq!(
            step(3, 0.0, false),
            ChangeStep::Jump(0),
            "inside a change, back goes to its start first"
        );
        assert_eq!(
            step_change(&[], 0, 0.0, 20.0, true),
            ChangeStep::Stay,
            "no changes, nothing to do"
        );
    }

    #[test]
    fn file_steps_wrap_on_a_press_but_not_while_held() {
        use crate::changes_tab::next_file;
        assert_eq!(next_file(None, 3, true, false), Some(0));
        assert_eq!(next_file(None, 3, false, false), Some(2));
        assert_eq!(next_file(Some(2), 3, true, false), Some(0));
        assert_eq!(next_file(Some(2), 3, true, true), None);
        assert_eq!(next_file(Some(0), 3, false, true), None);
        assert_eq!(next_file(Some(1), 3, true, true), Some(2));
        assert_eq!(next_file(Some(0), 0, true, false), None);
    }

    /// A workspace whose `src/app.rs` changed one line, lost another and gained a long block since the base.
    async fn stepping_workspace(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<Workspace>, gpui::VisualTestContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            cx.set_global(db::AppDatabase::test_new());
            crate::changes_tab::register(cx);
            crate::sidebar::register(cx);
            register(cx);
            cx.bind_keys([
                gpui::KeyBinding::new("f7", NextChange, Some("IcarusDiffTab > Editor")),
                gpui::KeyBinding::new("shift-f7", PreviousChange, Some("IcarusDiffTab > Editor")),
            ]);
        });
        let base = "one\ntwo\nthree\nfour\nfive\nsix\ntail\n";
        let added: String = (1..=60).map(|n| format!("added {n}\n")).collect();
        let current = format!("one\nTWO\nthree\nfive\n{added}six\ntail\n");
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({ ".git": {}, "src": { "app.rs": current.clone() } }),
        )
        .await;
        let dot_git = Path::new("/project/.git");
        fs.set_head_and_index_for_repo(dot_git, &[("src/app.rs", current)]);
        fs.set_merge_base_content_for_repo(dot_git, &[("src/app.rs", base.to_string())]);
        let project = project::Project::test(fs, [Path::new("/project")], cx).await;
        let multi_workspace = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let cx = gpui::VisualTestContext::from_window(multi_workspace.into(), cx);
        cx.run_until_parked();
        (workspace, cx)
    }

    fn cursor_line(tab: &Entity<DiffTab>, cx: &mut gpui::VisualTestContext) -> String {
        tab.update(cx, |tab, cx| {
            tab.editor.update(cx, |editor, cx| {
                let head = editor
                    .selections
                    .newest::<Point>(&editor.display_snapshot(cx))
                    .head();
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let row = MultiBufferRow(head.row);
                snapshot
                    .text_for_range(
                        Point::new(head.row, 0)..Point::new(head.row, snapshot.line_len(row)),
                    )
                    .collect()
            })
        })
    }

    #[gpui::test]
    async fn change_keys_step_through_a_diff_and_stop_at_the_last_change(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, mut cx) = stepping_workspace(cx).await;
        let cx = &mut cx;
        let tab = open(
            &workspace,
            "/project/src/app.rs",
            DiffBase::MergeBase("origin/main".into()),
            cx,
        );
        cx.run_until_parked();
        let tab = tab.await.expect("diff tab");
        cx.run_until_parked();
        tab.update_in(cx, |tab, window, cx| {
            window.focus(&tab.editor.focus_handle(cx), cx);
        });

        cx.simulate_keystrokes("f7");
        assert_eq!(cursor_line(&tab, cx), "TWO", "first the modified line");
        cx.simulate_keystrokes("f7");
        assert_eq!(
            cursor_line(&tab, cx),
            "five",
            "a deletion counts as the line after its gap"
        );
        cx.simulate_keystrokes("f7");
        assert_eq!(cursor_line(&tab, cx), "added 1");
        let scroll_top = |cx: &mut gpui::VisualTestContext| {
            tab.update(cx, |tab, cx| {
                tab.editor
                    .update(cx, |editor, cx| editor.scroll_position(cx).y)
            })
        };
        let before = scroll_top(cx);
        for _ in 0..10 {
            cx.simulate_keystrokes("f7");
        }
        assert!(
            scroll_top(cx) > before,
            "the 60-line change pages down instead of jumping past it"
        );
        let at_end = cursor_line(&tab, cx);
        assert!(
            at_end.starts_with("added"),
            "no wrap back to the first change after the last: {at_end:?}"
        );
        cx.simulate_keystrokes("shift-f7");
        cx.simulate_keystrokes("shift-f7");
        cx.simulate_keystrokes("shift-f7");
        cx.simulate_keystrokes("shift-f7");
        for _ in 0..10 {
            cx.simulate_keystrokes("shift-f7");
        }
        assert_eq!(
            cursor_line(&tab, cx),
            "TWO",
            "backwards stops at the first change, no wrap"
        );
    }

    #[gpui::test]
    async fn change_keys_from_outside_the_diff_reach_the_active_diff_tab(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, mut cx) = stepping_workspace(cx).await;
        let cx = &mut cx;
        let tab = open(
            &workspace,
            "/project/src/app.rs",
            DiffBase::MergeBase("origin/main".into()),
            cx,
        );
        cx.run_until_parked();
        let tab = tab.await.expect("diff tab");
        cx.run_until_parked();
        // Focus the workspace itself rather than the diff editor, as from the Source Control list.
        workspace.update_in(cx, |workspace, window, cx| {
            window.focus(&workspace.focus_handle(cx), cx);
        });
        cx.dispatch_action(NextChange);
        cx.run_until_parked();
        assert_eq!(cursor_line(&tab, cx), "TWO");
        assert!(
            cx.update(|window, cx| tab.read(cx).editor.focus_handle(cx).is_focused(window)),
            "the diff takes focus, as Orca's outside-editor keys do"
        );
    }

    fn editable_thread(id: &str, line: u32, resolved: bool) -> serde_json::Value {
        let mut thread = thread_json(id, line, resolved);
        thread["comments"][0]["viewer_can_update"] = json!(true);
        thread["comments"][0]["viewer_can_delete"] = json!(true);
        thread
    }

    async fn review_tab(
        cx: &mut gpui::TestAppContext,
        answer: impl Fn(&str, &serde_json::Value) -> Result<serde_json::Value> + Send + Sync + 'static,
    ) -> (
        Entity<DiffTab>,
        Entity<crate::review::DiffReview>,
        std::sync::Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
        gpui::VisualTestContext,
    ) {
        let (workspace, mut cx) = git_workspace(cx).await;
        let calls = cx.update(|_, cx| crate::review::tests::stub_rpc(cx, answer));
        let tab = open(
            &workspace,
            "/project/src/app.rs",
            DiffBase::Uncommitted,
            &mut cx,
        );
        cx.run_until_parked();
        let tab = tab.await.expect("diff tab");
        let review = tab.update(&mut cx, |tab, cx| {
            tab.enable_review("/project".into(), "src/app.rs".into(), cx);
            tab.review().cloned().expect("review")
        });
        review.update(&mut cx, |review, cx| review.reload(cx));
        cx.run_until_parked();
        (tab, review, calls, cx)
    }

    #[gpui::test]
    async fn gutter_plus_markers_comment_now_and_published_comment_actions(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::review::tests::calls_to;
        use crate::review::{ComposerTarget, Destination, MarkerKind};

        let (tab, review, calls, mut cx) = review_tab(cx, |method, _| match method {
            "review.threads" => Ok(json!({ "threads": [
                editable_thread("T1", 1, false), editable_thread("T2", 2, true),
            ] })),
            "review.comment.list" => Ok(json!({ "drafts": [] })),
            "review.mentionables" => Ok(json!({ "users": [{ "login": "alice" }] })),
            "review.comment.post" => Ok(json!({ "comment": { "id": 9 } })),
            _ => Ok(json!({})),
        })
        .await;
        let cx = &mut cx;

        // Rows with a thread show its marker instead of "+"; clicking a resolved one expands it.
        let editor = tab.read_with(cx, |tab, _| tab.editor().clone());
        let marked: Vec<(u32, MarkerKind, String)> = review.read_with(cx, |review, _| {
            (0..12)
                .filter_map(|row| review.marker_at(row).map(|(kind, id)| (row, kind, id)))
                .collect()
        });
        assert_eq!(
            marked
                .iter()
                .map(|(_, kind, id)| (*kind, id.as_str()))
                .collect::<Vec<_>>(),
            [(MarkerKind::Thread, "T1"), (MarkerKind::Thread, "T2")]
        );
        assert!(editor.read_with(cx, |editor, _| editor.gutter_comment_hook().is_some()));
        let resolved_row = marked[1].0;
        review.read_with(cx, |review, _| assert!(!review.is_expanded("T2")));
        let hook = editor.read_with(cx, |editor, _| editor.gutter_comment_hook().cloned());
        let hook = hook.expect("the review owns the gutter button");
        cx.update(|window, cx| {
            (hook.on_marker)(multi_buffer::MultiBufferRow(resolved_row), window, cx)
        });
        cx.run_until_parked();
        review.read_with(cx, |review, _| assert!(review.is_expanded("T2")));

        // "+" (click or drag) opens the box on the selected rows, the same as ⌘⇧A.
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([Point::new(0, 0)..Point::new(1, 3)])
            });
        });
        cx.update(|window, cx| (hook.on_rows)(window, cx));
        cx.run_until_parked();
        let composer = review.read_with(cx, |review, _| review.composer().cloned().expect("box"));
        assert_eq!(
            composer.read_with(cx, |composer, _| composer.target().clone()),
            ComposerTarget::Lines { start: 1, end: 2 }
        );
        assert_eq!(
            calls_to(&calls, "review.mentionables"),
            [json!({ "workspace": "/project" })],
            "the people to mention load when the first box opens"
        );

        // "Comment now" posts straight away instead of queueing.
        composer
            .read_with(cx, |composer, _| composer.editor().clone())
            .update_in(cx, |editor, window, cx| {
                editor.set_text("Ship it", window, cx)
            });
        review.update_in(cx, |review, window, cx| {
            review.submit_composer(Destination::Now, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "review.comment.post"),
            [
                json!({ "workspace": "/project", "path": "src/app.rs", "line": 2,
                "start_line": 1, "body": "Ship it" })
            ]
        );
        assert!(calls_to(&calls, "review.comment.add").is_empty());

        // Your published comments can be edited, deleted (after asking) and reacted to.
        review.update_in(cx, |review, window, cx| {
            review.open_composer(
                ComposerTarget::EditPublished {
                    comment: "T1-c1".into(),
                },
                "Why this, exactly?".into(),
                window,
                cx,
            );
            review.submit_composer(Destination::Review, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "review.comment.edit_published"),
            [
                json!({ "workspace": "/project", "kind": "review_comment", "node_id": "T1-c1",
                "body": "Why this, exactly?" })
            ]
        );
        review.update_in(cx, |review, window, cx| {
            review.delete_published("T1-c1", window, cx)
        });
        cx.run_until_parked();
        assert!(
            calls_to(&calls, "review.comment.delete_published").is_empty(),
            "nothing is deleted before the answer"
        );
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "review.comment.delete_published"),
            [json!({ "workspace": "/project", "kind": "review_comment", "node_id": "T1-c1" })]
        );
        review.update(cx, |review, cx| {
            review.toggle_reaction("T1-c1", "heart", false, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "review.react")[0],
            json!({ "workspace": "/project", "subject": "T1-c1", "reaction": "heart",
                "remove": false })
        );
    }

    #[gpui::test]
    async fn pasted_images_upload_in_place_and_mentions_complete(cx: &mut gpui::TestAppContext) {
        use crate::review::ComposerTarget;
        use crate::review::tests::calls_to;

        let (_tab, review, calls, mut cx) = review_tab(cx, |method, params| match method {
            "review.threads" => Ok(json!({ "threads": [] })),
            "review.comment.list" => Ok(json!({ "drafts": [] })),
            "review.mentionables" => Ok(json!({ "author": "bob",
                "users": [{ "login": "alice", "name": "Alice A" }] })),
            "review.upload" if params["name"] == "pasted-image.png" => Ok(json!({
                "markdown": "![pasted-image.png](https://example.com/a.png)" })),
            "review.upload" => Err(anyhow::anyhow!("Only images and videos can be attached.")),
            _ => Ok(json!({})),
        })
        .await;
        let cx = &mut cx;
        review.update_in(cx, |review, window, cx| {
            review.open_composer(
                ComposerTarget::Lines { start: 1, end: 1 },
                String::new(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let composer = review.read_with(cx, |review, _| review.composer().cloned().expect("box"));
        let editor = composer.read_with(cx, |composer, _| composer.editor().clone());
        let focus = editor.read_with(cx, |editor, cx| editor.focus_handle(cx));
        cx.update(|window, cx| window.focus(&focus, cx));

        // A pasted image uploads from memory and its markdown replaces the placeholder.
        cx.write_to_clipboard(gpui::ClipboardItem::new_image(&gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            vec![1, 2, 3],
        )));
        cx.dispatch_action(editor::actions::Paste);
        let uploaded = calls_to(&calls, "review.upload");
        assert_eq!(uploaded.len(), 1);
        assert_eq!(uploaded[0]["name"], "pasted-image.png");
        assert_eq!(uploaded[0]["data_base64"], "AQID");
        cx.run_until_parked();
        assert_eq!(
            editor.read_with(cx, |editor, cx| editor.text(cx)),
            "![pasted-image.png](https://example.com/a.png)"
        );

        // A failed upload takes its placeholder back out and shows GitHub's words.
        review.update_in(cx, |review, window, cx| {
            review.upload_to_composer(
                crate::comment_input::UploadSource::Path("/tmp/notes.txt".into()),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            editor.read_with(cx, |editor, cx| editor.text(cx)),
            "![pasted-image.png](https://example.com/a.png)"
        );
        assert_eq!(
            composer.read_with(cx, |composer, _| composer.error_text()),
            Some("Only images and videos can be attached.".to_owned())
        );

        // Typing "@al" offers the repository's people; confirming inserts the login.
        editor.update_in(cx, |editor, window, cx| editor.set_text("", window, cx));
        cx.simulate_input("cc @al");
        cx.run_until_parked();
        cx.dispatch_action(editor::actions::ConfirmCompletion::default());
        cx.run_until_parked();
        assert_eq!(
            editor.read_with(cx, |editor, cx| editor.text(cx)),
            "cc @alice "
        );
        cx.simulate_input(":tad");
        cx.run_until_parked();
        cx.dispatch_action(editor::actions::ConfirmCompletion::default());
        cx.run_until_parked();
        assert_eq!(
            editor.read_with(cx, |editor, cx| editor.text(cx)),
            "cc @alice 🎉"
        );
    }

    #[gpui::test]
    async fn notes_show_in_plain_file_editors_and_answers_link_their_transcript(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::review::tests::{calls_to, stub_rpc};

        let (workspace, mut cx) = git_workspace(cx).await;
        let cx = &mut cx;
        let calls = cx.update(|_, cx| {
            crate::enable_for_tests(cx);
            crate::worktree_trust::set_icarus_workspace_paths(vec!["/project".into()], cx);
            crate::review::attach_notes_to_file_editors(cx);
            stub_rpc(cx, |method, _| match method {
                "note.list" => Ok(json!([{
                    "id": "n1", "path": "src/new.rs", "line": 1, "end_line": 1,
                    "author": "user", "body": "Why added?",
                    "replies": [{ "id": "r1", "author": "reply-agent", "body": "For the test.",
                        "status": "done", "agent_id": "agent-7" }],
                }])),
                "review.threads" => {
                    let mut thread = thread_json("T1", 1, false);
                    thread["path"] = json!("src/new.rs");
                    Ok(json!({ "threads": [thread] }))
                }
                "review.comment.list" => Ok(json!({ "drafts": [
                    { "id": "d1", "path": "src/new.rs", "line": 1, "subject": "line", "body": "Queued" },
                    { "id": "d2", "path": "src/other.rs", "line": 1, "subject": "line", "body": "Elsewhere" },
                ] })),
                _ => Ok(json!({})),
            })
        });
        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
        let buffer = project.update(cx, |project, cx| {
            project.open_local_buffer("/project/src/new.rs", cx)
        });
        cx.run_until_parked();
        let buffer = buffer.await.expect("file opens");
        let editor = cx.new_window_entity(|window, cx| {
            Editor::for_buffer(buffer, Some(project.clone()), window, cx)
        });
        cx.run_until_parked();
        let review = editor.read_with(cx, |editor, _| {
            editor
                .addon::<crate::review::ReviewAttached>()
                .and_then(|attached| attached.0.clone())
        });
        let review = review.expect("plain file editors get notes too");
        // Without a server connection the first load waits to be asked, as in the diff tab tests.
        review.update(cx, |review, cx| review.reload(cx));
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "note.list")[0]["path"],
            "src/new.rs",
            "relative to the workspace root"
        );
        assert_eq!(
            calls_to(&calls, "review.threads")[0]["path"],
            "src/new.rs",
            "file editors show the PR's threads on this file too"
        );
        review.read_with(cx, |review, _| {
            assert_eq!(review.notes().len(), 1);
            assert_eq!(review.threads().len(), 1);
            assert_eq!(
                review
                    .drafts()
                    .iter()
                    .map(|draft| draft.id.as_str())
                    .collect::<Vec<_>>(),
                ["d1"],
                "only this file's drafts"
            );
            assert_eq!(
                review.marker_kinds_at(0),
                [
                    crate::review::MarkerKind::Thread,
                    crate::review::MarkerKind::Draft,
                    crate::review::MarkerKind::Note
                ],
                "a row with all three kinds shows each"
            );
        });
        let hook = editor.read_with(cx, |editor, _| editor.gutter_comment_hook().cloned());
        let hook = hook.expect("the review owns the gutter");
        let marker = cx
            .update(|_, cx| (hook.marker)(multi_buffer::MultiBufferRow(0), cx))
            .expect("a marker on the first row");
        assert_eq!(marker.icons.len(), 3);
        assert_eq!(
            marker.tooltip.as_ref(),
            "Show a PR thread, your pending comment and a note"
        );

        review.update(cx, |review, cx| review.open_transcript("agent-7", cx));
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "agent.resume"),
            [json!({ "id": "agent-7" })],
            "an answer's transcript wakes its session from history"
        );
    }
}
