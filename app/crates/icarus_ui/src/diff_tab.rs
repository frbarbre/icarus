//! A diff tab: one changed file against the workspace's base, shown as a diff or edited in place (source-control §5.2).

use std::any::{Any, TypeId};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use editor::{Editor, EditorEvent, HiddenDiffHunkRenderer};
use git::status::{DiffTreeType, TreeDiffStatus};
use gpui::{
    AnyEntity, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Subscription, Task, WeakEntity, Window, actions,
};
use language::Buffer;
use multi_buffer::{Anchor, MultiBuffer};
use project::{Project, ProjectPath};
use serde::{Deserialize, Serialize};
use ui::{ToggleButtonGroup, ToggleButtonSimple, prelude::*};
use util::ResultExt as _;
use workspace::item::{Item, ItemEvent, SaveOptions, SerializableItem};
use workspace::searchable::SearchableItemHandle;
use workspace::{ItemHandle as _, ItemId, ItemNavHistory, Pane, Workspace, WorkspaceId};

use crate::review::{AddReviewComment, DiffReview};

actions!(
    icarus,
    [
        /// Shows the active diff tab as an editable file (⌥⌘E).
        ShowEdit,
        /// Shows the active file's changes against the base (⌥⌘D).
        ShowDiff,
    ]
);

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
    _review_changes: Option<Subscription>,
    _editor_events: Subscription,
}

impl DiffTab {
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
            cx.new(|cx| DiffReview::new(editor, buffer, root, relative, cx))
        });
        let review_changes = review
            .as_ref()
            .map(|review| cx.observe(review, |_, _, cx| cx.notify()));
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
        let review = cx.new(|cx| DiffReview::new(editor, buffer, workspace_key, path, cx));
        self._review_changes = Some(cx.observe(&review, |_, _, cx| cx.notify()));
        self.review = Some(review);
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
    })
    .detach();
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
        v_flex()
            .key_context("IcarusDiffTab")
            .size_full()
            .on_action(
                cx.listener(|this, _: &ShowEdit, _, cx| this.set_mode(DiffTabMode::Edit, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ShowDiff, _, cx| this.set_mode(DiffTabMode::Diff, cx)),
            )
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
            review.toggle_reaction("T1-c1", false, cx);
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

        // A note for the agent goes to the composer as a text attachment.
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
        let notes = calls_to(&calls, "attach.text");
        assert_eq!(notes[0]["title"], "src/app.rs:lines 1-2");
        assert!(
            notes[0]["text"]
                .as_str()
                .is_some_and(|text| text.ends_with("Check both lines")),
            "{notes:?}"
        );
    }
}
