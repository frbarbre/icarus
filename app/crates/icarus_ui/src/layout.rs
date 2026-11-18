use std::path::{Path, PathBuf};
use std::sync::Arc;

use collections::HashSet;
use editor::Editor;
use gpui::{App, Context, Entity, EntityId, Focusable as _, Task, Window};
use icarus_client::Client;
use icarus_protocol::layout::{LayoutNode, SplitAxis, Tab};
use project::ProjectPath;
use terminal_view::TerminalView;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{Member, Pane, SaveIntent, SplitDirection, Workspace};

/// A layout the server resolved for a workspace (`open_layout` event, `workspace.take_layout`).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct PendingLayout {
    pub workspace_id: String,
    pub path: PathBuf,
    #[serde(default)]
    pub name: String,
    pub layout: LayoutNode,
}

/// Claims the workspace's pending layout; `None` when another window already applied it.
pub async fn take_layout(
    client: Arc<Client>,
    workspace_id: String,
) -> anyhow::Result<Option<PendingLayout>> {
    let value = client
        .request(
            "workspace.take_layout",
            serde_json::json!({ "id": workspace_id }),
        )
        .await?;
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_value(value)?))
}

pub async fn pending_layouts(client: Arc<Client>) -> anyhow::Result<Vec<PendingLayout>> {
    let value = client
        .request("workspace.pending_layouts", serde_json::json!({}))
        .await?;
    Ok(serde_json::from_value(value)?)
}

/// Reveals the sidebar panel a layout entry names: Source Control in the entry's scope, or the Explorer.
fn show_sidebar_panel(
    request: &Tab,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match request {
        Tab::Diff { scope, .. } => {
            let scope = diff_scope(scope.as_deref());
            if let Some(panel) = crate::changes_tab::reveal_source_control(workspace, window, cx) {
                panel.update(cx, |panel, cx| panel.set_scope(scope, cx));
            }
        }
        Tab::Explorer => {
            let dock = workspace.right_dock().clone();
            dock.update(cx, |dock, cx| {
                if let Some(index) =
                    dock.panel_index_for_persistent_name(crate::sidebar::EXPLORER_PANEL, cx)
                {
                    dock.activate_panel(index, window, cx);
                    dock.set_open(true, window, cx);
                }
            });
        }
        _ => {}
    }
}

/// A diff entry's `scope` as Source Control understands it: base (default), uncommitted, turn[:name], commit:sha.
fn diff_scope(scope: Option<&str>) -> crate::changes_tab::ChangesScope {
    use crate::changes_tab::ChangesScope;
    match scope.map(str::trim) {
        Some("uncommitted") => ChangesScope::Uncommitted,
        Some("turn" | "turn:latest" | "latest-turn") => ChangesScope::LatestTurn,
        Some(other) => match other.split_once(':') {
            Some(("turn", name)) if !name.is_empty() => ChangesScope::Turn(name.to_string()),
            Some(("commit", sha)) if !sha.is_empty() => ChangesScope::Commit(sha.to_string()),
            _ => ChangesScope::Base,
        },
        None => ChangesScope::Base,
    }
}

/// Explorer entries and diff entries without a file show sidebar panels rather than tabs (plan D31);
/// a diff entry naming a file is that file's diff tab.
fn is_sidebar_entry(tab: &Tab) -> bool {
    matches!(tab, Tab::Explorer | Tab::Diff { path: None, .. })
}

/// The sidebar panel a layout asks for: its first explorer or diff entry, in layout order.
fn sidebar_request(layout: &LayoutNode) -> Option<Tab> {
    layout
        .tabs()
        .into_iter()
        .find(|tab| is_sidebar_entry(tab))
        .cloned()
}

/// Drops sidebar entries, empty panes and splits left with one child, so the tree never builds an empty pane.
fn prune(node: &LayoutNode) -> Option<LayoutNode> {
    match node {
        LayoutNode::Pane { tabs, active } => {
            let kept: Vec<Tab> = tabs
                .iter()
                .filter(|tab| !is_sidebar_entry(tab))
                .cloned()
                .collect();
            if kept.is_empty() {
                return None;
            }
            let active = active
                .filter(|active| tabs.get(*active).is_some_and(|tab| !is_sidebar_entry(tab)))
                .map(|active| {
                    tabs[..active]
                        .iter()
                        .filter(|tab| !is_sidebar_entry(tab))
                        .count()
                });
            Some(LayoutNode::Pane { tabs: kept, active })
        }
        LayoutNode::Split {
            split,
            children,
            sizes,
        } => {
            let mut kept_children = Vec::new();
            let mut kept_sizes = Vec::new();
            for (index, child) in children.iter().enumerate() {
                if let Some(child) = prune(child) {
                    kept_children.push(child);
                    if let Some(size) = sizes.as_ref().and_then(|sizes| sizes.get(index)) {
                        kept_sizes.push(*size);
                    }
                }
            }
            match kept_children.len() {
                0 => None,
                1 => kept_children.pop(),
                count => Some(LayoutNode::Split {
                    split: *split,
                    sizes: (kept_sizes.len() == count).then_some(kept_sizes),
                    children: kept_children,
                }),
            }
        }
    }
}

/// Split flexes for `sizes`, scaled so they sum to the member count as Zed's pane axes expect.
fn flexes(sizes: &[f32]) -> Option<Vec<f32>> {
    let total: f32 = sizes.iter().sum();
    if sizes.is_empty() || total <= 0.0 || sizes.iter().any(|size| *size <= 0.0) {
        return None;
    }
    let count = sizes.len() as f32;
    Some(sizes.iter().map(|size| size / total * count).collect())
}

fn find_axis<'a>(member: &'a Member, panes: &[Entity<Pane>]) -> Option<&'a workspace::PaneAxis> {
    let Member::Axis(axis) = member else {
        return None;
    };
    let matches = axis.members.len() == panes.len()
        && axis
            .members
            .iter()
            .zip(panes)
            .all(|(member, pane)| matches!(member, Member::Pane(found) if found == pane));
    if matches {
        return Some(axis);
    }
    axis.members
        .iter()
        .find_map(|member| find_axis(member, panes))
}

/// Replaces the workspace's center arrangement with `layout`. Items the layout doesn't place are closed,
/// except unsaved ones, which stay in the first pane.
pub fn apply_layout(
    layout: &LayoutNode,
    worktree_root: &Path,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(request) = sidebar_request(layout) {
        show_sidebar_panel(&request, workspace, window, cx);
    }
    let Some(layout) = prune(layout) else {
        return;
    };
    if workspace.panes().len() > 1 {
        workspace.join_all_panes(window, cx);
    }
    // Saved tabs other than agents close first, so the layout's files and diff open fresh. Agent tabs stay
    // until the layout has claimed the ones it shows, so their sessions aren't reloaded.
    let agent_type = cx
        .try_global::<AgentItemType>()
        .map(|agent_type| agent_type.0);
    let mut closing = Vec::new();
    for pane in workspace.panes().to_vec() {
        let to_close: Vec<EntityId> = pane
            .read(cx)
            .items()
            .filter(|item| {
                Some(item.to_any_view().entity_type()) != agent_type && !item.is_dirty(cx)
            })
            .map(|item| item.item_id())
            .collect();
        pane.update(cx, |pane, cx| {
            for item_id in to_close {
                closing.push(pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx));
            }
        });
    }
    let worktree_root = worktree_root.to_path_buf();
    // Emptied panes leave the split tree on a later effect cycle, so building waits for the closes to land.
    cx.spawn_in(window, async move |workspace, cx| {
        for close in closing {
            close.await.log_err();
        }
        workspace.update_in(cx, |workspace, window, cx| {
            build_layout(&layout, &worktree_root, workspace, window, cx)
        })
    })
    .detach_and_log_err(cx);
}

fn build_layout(
    layout: &LayoutNode,
    worktree_root: &Path,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let root = workspace.center_pane_group().first_pane();
    if workspace.panes().len() > 1 {
        log::error!(
            "layout: the center still has {} panes after joining",
            workspace.panes().len()
        );
    }
    let mut build = Build {
        worktree_root,
        placed: HashSet::default(),
        pending: Vec::new(),
        actives: Vec::new(),
    };
    build.node(layout, root, workspace, window, cx);
    // Agent tabs the layout doesn't show stay open: closing one would end its agent.
    let Build {
        pending, actives, ..
    } = build;

    let first = workspace.center_pane_group().first_pane();
    window.focus(&first.focus_handle(cx), cx);
    cx.spawn_in(window, async move |_, cx| {
        for task in pending {
            task.await.log_err();
        }
        // Files and terminals arrive late and take focus in their pane; hand it back to the layout's choice.
        cx.update(|window, cx| {
            for (pane, active) in actives {
                pane.update(cx, |pane, cx| {
                    if active < pane.items_len() {
                        pane.activate_item(active, false, false, window, cx);
                    }
                });
            }
        })
        .log_err();
    })
    .detach();
}

struct Build<'a> {
    worktree_root: &'a Path,
    placed: HashSet<EntityId>,
    pending: Vec<Task<anyhow::Result<()>>>,
    actives: Vec<(Entity<Pane>, usize)>,
}

/// The item type of agent tabs, registered by the app crate that owns them.
pub struct AgentItemType(pub std::any::TypeId);

impl gpui::Global for AgentItemType {}

impl Build<'_> {
    fn node(
        &mut self,
        node: &LayoutNode,
        pane: Entity<Pane>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        match node {
            LayoutNode::Split {
                split,
                children,
                sizes,
            } => {
                let direction = match split {
                    SplitAxis::Row => SplitDirection::Right,
                    SplitAxis::Column => SplitDirection::Down,
                };
                let mut panes = vec![pane];
                for _ in 1..children.len() {
                    let Some(previous) = panes.last().cloned() else {
                        break;
                    };
                    panes.push(workspace.split_pane(previous, direction, window, cx));
                }
                if let Some(flexes) = sizes.as_deref().and_then(flexes)
                    && let Some(axis) = find_axis(&workspace.center_pane_group().root, &panes)
                {
                    *axis.flexes.lock() = flexes;
                }
                for (child, pane) in children.iter().zip(panes) {
                    self.node(child, pane, workspace, window, cx);
                }
            }
            LayoutNode::Pane { tabs, active } => {
                for (index, tab) in tabs.iter().enumerate() {
                    self.tab(tab, index, &pane, workspace, window, cx);
                }
                let active = active.unwrap_or(0);
                pane.update(cx, |pane, cx| {
                    if active < pane.items_len() {
                        pane.activate_item(active, false, false, window, cx);
                    }
                });
                self.actives.push((pane, active));
            }
        }
    }

    fn tab(
        &mut self,
        tab: &Tab,
        index: usize,
        pane: &Entity<Pane>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let before: HashSet<EntityId> = pane.read(cx).items().map(|item| item.item_id()).collect();
        let worktree_root = self.worktree_root;
        match tab {
            Tab::Agent { agent_id, .. } => {
                crate::open_agent_tab(agent_id.clone(), Some(pane.clone()), workspace, window, cx);
                // An agent tab that was already open is moved or activated rather than added.
                if let Some(active) = pane.read(cx).active_item() {
                    self.placed.insert(active.item_id());
                }
            }
            Tab::Browser { url } => {
                crate::browser::open_browser_tab(url.clone(), pane, false, workspace, window, cx);
            }
            Tab::Diff {
                path: Some(path),
                scope,
            } => {
                let task = open_diff(
                    worktree_root.join(path),
                    diff_tab_base(scope.as_deref()),
                    index,
                    pane,
                    workspace,
                    window,
                    cx,
                );
                self.pending.push(task);
            }
            // Pruned before building: these show sidebar panels, not tabs.
            Tab::Diff { path: None, .. } | Tab::Explorer => {}
            Tab::File { path, line } => {
                let path = if path.is_absolute() {
                    path.clone()
                } else {
                    worktree_root.join(path)
                };
                let task = open_file(path, *line, index, pane, workspace, window, cx);
                self.pending.push(task);
            }
            Tab::Terminal {
                command,
                terminal_id,
            } => {
                let task = open_terminal(
                    command.clone(),
                    terminal_id.clone(),
                    worktree_root,
                    index,
                    pane,
                    workspace,
                    window,
                    cx,
                );
                self.pending.push(task);
            }
        }
        self.placed.extend(
            pane.read(cx)
                .items()
                .map(|item| item.item_id())
                .filter(|id| !before.contains(id)),
        );
    }
}

/// Puts a tab that arrived late at the position its layout gave it.
fn place_at(
    pane: &Entity<Pane>,
    item_id: EntityId,
    index: usize,
    window: &mut Window,
    cx: &mut App,
) {
    let last = pane.read(cx).items_len().saturating_sub(1);
    workspace::move_item(pane, pane, item_id, index.min(last), false, window, cx);
}

fn open_file(
    path: PathBuf,
    line: Option<u32>,
    index: usize,
    pane: &Entity<Pane>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<anyhow::Result<()>> {
    let Some((worktree, relative)) = workspace.project().read(cx).find_worktree(&path, cx) else {
        return Task::ready(Err(anyhow::anyhow!(
            "{} is outside the workspace",
            path.display()
        )));
    };
    let project_path = ProjectPath {
        worktree_id: worktree.read(cx).id(),
        path: relative,
    };
    let open = workspace.open_path(project_path, Some(pane.downgrade()), false, window, cx);
    let pane = pane.clone();
    cx.spawn_in(window, async move |_, cx| {
        let item = open.await?;
        cx.update(|window, cx| place_at(&pane, item.item_id(), index, window, cx))?;
        if let Some(line) = line
            && let Some(editor) = item.downcast::<Editor>()
        {
            editor.update_in(cx, |editor, window, cx| {
                let point = language::Point::new(line.saturating_sub(1), 0);
                editor.go_to_singleton_buffer_point(point, window, cx);
            })?;
        }
        anyhow::Ok(())
    })
}

/// A diff tab's base from its entry's scope: `base:<ref>` (the server fills it in), else uncommitted changes.
fn diff_tab_base(scope: Option<&str>) -> crate::diff_tab::DiffBase {
    use crate::diff_tab::DiffBase;
    match scope.and_then(|scope| scope.strip_prefix("base:")) {
        Some(base) if !base.is_empty() => DiffBase::MergeBase(base.to_owned()),
        _ => DiffBase::Uncommitted,
    }
}

fn open_diff(
    path: PathBuf,
    base: crate::diff_tab::DiffBase,
    index: usize,
    pane: &Entity<Pane>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<anyhow::Result<()>> {
    let open = crate::diff_tab::open_diff_tab(
        path,
        base,
        crate::diff_tab::DiffTabMode::Diff,
        Some(pane.clone()),
        workspace,
        window,
        cx,
    );
    let pane = pane.clone();
    cx.spawn_in(window, async move |_, cx| {
        let tab = open.await?;
        cx.update(|window, cx| place_at(&pane, tab.entity_id(), index, window, cx))?;
        anyhow::Ok(())
    })
}

fn open_terminal(
    command: Option<String>,
    terminal_id: Option<String>,
    worktree_root: &Path,
    index: usize,
    pane: &Entity<Pane>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<anyhow::Result<()>> {
    let project = workspace.project().clone();
    let cwd = worktree_root.to_path_buf();
    let create = project.update(cx, |project, cx| match (command, terminal_id) {
        // An existing server terminal, e.g. the workspace's Setup: show it rather than start another.
        (_, Some(terminal_id)) => {
            project.create_terminal_shell_reattaching(Some(cwd), Some(terminal_id), cx)
        }
        (Some(command), None) => project.create_terminal_task(
            task::SpawnInTerminal {
                label: command.clone(),
                full_label: command.clone(),
                command_label: command.clone(),
                // Tasks take a program plus arguments; a layout command is a whole shell line.
                command: Some("sh".to_string()),
                args: vec!["-c".to_string(), command],
                cwd: Some(cwd),
                use_new_terminal: true,
                ..Default::default()
            },
            cx,
        ),
        (None, None) => project.create_terminal_shell(Some(cwd), cx),
    });
    let pane = pane.downgrade();
    cx.spawn_in(window, async move |workspace, cx| {
        let terminal = create.await?;
        workspace.update_in(cx, |workspace, window, cx| {
            let view = cx.new(|cx| {
                TerminalView::new(
                    terminal,
                    workspace.weak_handle(),
                    workspace.database_id(),
                    workspace.project().downgrade(),
                    window,
                    cx,
                )
            });
            let pane = pane
                .upgrade()
                .unwrap_or_else(|| workspace.active_pane().clone());
            let item_id = view.entity_id();
            workspace.add_item(pane.clone(), Box::new(view), None, false, false, window, cx);
            place_at(&pane, item_id, index, window, cx);
        })
    })
}

/// Applies `pending` to the window showing its workspace, if one is open; returns whether it was applied.
pub fn apply_to_open_window(pending: &PendingLayout, cx: &mut App) -> bool {
    let Some((window, workspace)) = crate::window_for_path(&pending.path, cx) else {
        return false;
    };
    let layout = pending.layout.clone();
    let root = pending.path.clone();
    window
        .update(cx, |multi_workspace, window, cx| {
            multi_workspace.activate(workspace.clone(), None, window, cx);
            workspace.update(cx, |workspace, cx| {
                apply_layout(&layout, &root, workspace, window, cx)
            });
        })
        .log_err()
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(value: serde_json::Value) -> LayoutNode {
        serde_json::from_value(value).expect("layout")
    }

    #[test]
    fn empty_panes_are_dropped_and_single_child_splits_collapse() {
        let layout = node(json!({ "split": "row", "sizes": [1.0, 3.0], "children": [
            { "tabs": [] },
            { "tabs": [{ "type": "explorer" }, { "type": "agent", "agent_id": "a" }], "active": 1 },
        ]}));
        assert_eq!(
            prune(&layout),
            Some(node(json!({
                "tabs": [{ "type": "agent", "agent_id": "a" }],
                "active": 0,
            }))),
            "the explorer is a sidebar panel, so it leaves the pane and the active index follows"
        );
        assert_eq!(prune(&node(json!({ "tabs": [] }))), None);
        assert_eq!(
            prune(&node(json!({ "tabs": [{ "type": "diff" }] }))),
            None,
            "a pane of sidebar entries builds nothing"
        );
    }

    #[test]
    fn the_first_explorer_or_diff_entry_picks_the_sidebar_panel() {
        let layout = node(json!({ "split": "row", "children": [
            { "tabs": [{ "type": "agent" }, { "type": "diff", "path": "a.rs" }] },
            { "tabs": [{ "type": "diff", "scope": "uncommitted" }, { "type": "explorer" }] },
        ]}));
        assert_eq!(
            sidebar_request(&layout),
            Some(Tab::Diff {
                scope: Some("uncommitted".into()),
                path: None,
            }),
            "a diff naming a file is a tab, not the panel"
        );
        assert_eq!(
            sidebar_request(&node(json!({ "tabs": [{ "type": "agent" }] }))),
            None
        );
        let pruned = prune(&layout).expect("tabs left");
        assert_eq!(
            pruned.tabs().len(),
            2,
            "the agent and the file's diff tab stay"
        );
    }

    #[test]
    fn a_diff_tab_entry_opens_against_the_base_the_server_named() {
        use crate::diff_tab::DiffBase;
        assert_eq!(
            diff_tab_base(Some("base:origin/main")),
            DiffBase::MergeBase("origin/main".into())
        );
        assert_eq!(diff_tab_base(None), DiffBase::Uncommitted);
        assert_eq!(diff_tab_base(Some("base:")), DiffBase::Uncommitted);
    }

    #[test]
    fn diff_tab_scopes_parse_like_the_server() {
        use crate::changes_tab::ChangesScope;
        assert_eq!(diff_scope(None), ChangesScope::Base);
        assert_eq!(diff_scope(Some("uncommitted")), ChangesScope::Uncommitted);
        assert_eq!(diff_scope(Some("turn")), ChangesScope::LatestTurn);
        assert_eq!(
            diff_scope(Some("turn:t-1")),
            ChangesScope::Turn("t-1".into())
        );
        assert_eq!(
            diff_scope(Some("commit:abc")),
            ChangesScope::Commit("abc".into())
        );
        assert_eq!(diff_scope(Some("nonsense")), ChangesScope::Base);
    }

    fn browser_urls(pane: &Entity<Pane>, cx: &App) -> Vec<String> {
        pane.read(cx)
            .items()
            .filter_map(|item| item.downcast::<crate::browser::BrowserTab>())
            .map(|tab| tab.read(cx).url().to_string())
            .collect()
    }

    fn count_of<T: 'static>(workspace: &Workspace, cx: &App) -> usize {
        workspace
            .panes()
            .iter()
            .flat_map(|pane| pane.read(cx).items().collect::<Vec<_>>())
            .filter(|item| item.downcast::<T>().is_some())
            .count()
    }

    #[gpui::test]
    async fn a_nested_layout_builds_the_same_split_tree(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "a.txt": "" })).await;
        let project = project::Project::test(fs, [Path::new("/project")], cx).await;
        let multi_workspace = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let cx = &mut gpui::VisualTestContext::from_window(multi_workspace.into(), cx);
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel =
                cx.new(|cx| crate::changes_tab::SourceControlPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        let layout = node(json!({ "split": "row", "sizes": [1.0, 3.0], "children": [
            { "tabs": [{ "type": "browser", "url": "" }, { "type": "browser", "url": "" }], "active": 1 },
            { "split": "column", "children": [
                { "tabs": [{ "type": "diff", "scope": "uncommitted" }] },
                { "tabs": [{ "type": "explorer" }] },
                { "tabs": [{ "type": "browser", "url": "" }] },
                { "tabs": [{ "type": "browser", "url": "" }] },
            ]},
        ]}));
        let apply = |cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                apply_layout(&layout, Path::new("/project"), workspace, window, cx)
            });
            cx.run_until_parked();
        };
        apply(cx);

        workspace.read_with(cx, |workspace, cx| {
            let Member::Axis(row) = &workspace.center_pane_group().root else {
                panic!("the root is a row split");
            };
            assert_eq!(row.axis, gpui::Axis::Horizontal);
            assert_eq!(*row.flexes.lock(), vec![0.5, 1.5]);
            let Member::Pane(left) = &row.members[0] else {
                panic!("the left side is one pane");
            };
            assert_eq!(browser_urls(left, cx).len(), 2);
            assert_eq!(left.read(cx).active_item_index(), 1);
            let Member::Axis(column) = &row.members[1] else {
                panic!("the right side is a column");
            };
            assert_eq!(column.axis, gpui::Axis::Vertical);
            assert_eq!(
                column.members.len(),
                2,
                "the diff and explorer entries are sidebar panels, not panes"
            );
            assert_eq!(workspace.panes().len(), 3);
            let dock = workspace.right_dock().read(cx);
            assert!(dock.is_open(), "the layout reveals Source Control");
            assert_eq!(
                dock.active_panel().map(|panel| panel.persistent_name()),
                Some(crate::changes_tab::SOURCE_CONTROL_PANEL)
            );
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.scope(),
                &crate::changes_tab::ChangesScope::Uncommitted
            )
        });

        apply(cx);
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.panes().len(),
                3,
                "applying again rebuilds, not nests"
            );
            let tabs: usize = workspace
                .panes()
                .iter()
                .map(|pane| browser_urls(pane, cx).len())
                .sum();
            assert_eq!(tabs, 4, "tabs the layout doesn't place are closed");
            assert_eq!(count_of::<crate::DiffTab>(workspace, cx), 0);
        });
    }

    #[test]
    fn sizes_become_flexes_that_sum_to_the_member_count() {
        assert_eq!(flexes(&[1.0, 3.0]), Some(vec![0.5, 1.5]));
        assert_eq!(flexes(&[0.25, 0.25, 0.5]), Some(vec![0.75, 0.75, 1.5]));
        assert_eq!(flexes(&[0.0, 1.0]), None);
        assert_eq!(flexes(&[]), None);
    }
}
