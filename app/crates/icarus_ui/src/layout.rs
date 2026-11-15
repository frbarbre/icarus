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

/// A diff tab's `scope` as the Changes tab understands it: base (default), uncommitted, turn[:name], commit:sha.
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

/// Drops empty panes and splits left with one child, so the tree never builds an empty pane.
fn prune(node: &LayoutNode) -> Option<LayoutNode> {
    match node {
        LayoutNode::Pane { tabs, active } => {
            if tabs.is_empty() {
                return None;
            }
            let active = active.filter(|active| *active < tabs.len());
            Some(LayoutNode::Pane {
                tabs: tabs.clone(),
                active,
            })
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
    let agent_type = cx
        .try_global::<AgentItemType>()
        .map(|agent_type| agent_type.0);
    let mut leftover_agents: Vec<EntityId> = root
        .read(cx)
        .items()
        .filter(|item| Some(item.to_any_view().entity_type()) == agent_type)
        .map(|item| item.item_id())
        .collect();

    let mut build = Build {
        worktree_root,
        placed: HashSet::default(),
        pending: Vec::new(),
        actives: Vec::new(),
    };
    build.node(layout, root, workspace, window, cx);
    let Build {
        placed,
        pending,
        actives,
        ..
    } = build;

    leftover_agents.retain(|item_id| !placed.contains(item_id));
    for pane in workspace.panes().to_vec() {
        close_items(&pane, &leftover_agents, window, cx);
    }

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

fn close_items(pane: &Entity<Pane>, item_ids: &[EntityId], window: &mut Window, cx: &mut App) {
    pane.update(cx, |pane, cx| {
        for item_id in item_ids {
            if pane.items().any(|item| item.item_id() == *item_id) {
                pane.close_item_by_id(*item_id, SaveIntent::Skip, window, cx)
                    .detach_and_log_err(cx);
            }
        }
    });
}

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
            Tab::Diff { scope } => {
                let scope = diff_scope(scope.as_deref());
                if let Some(changes) =
                    crate::changes_tab::open_changes_tab(pane, false, workspace, window, cx)
                {
                    changes.update(cx, |changes, cx| changes.set_scope(scope, cx));
                }
            }
            Tab::File { path, line } => {
                let path = if path.is_absolute() {
                    path.clone()
                } else {
                    worktree_root.join(path)
                };
                let task = open_file(path, *line, index, pane, workspace, window, cx);
                self.pending.push(task);
            }
            Tab::Terminal { command } => {
                let task = open_terminal(
                    command.clone(),
                    worktree_root,
                    index,
                    pane,
                    workspace,
                    window,
                    cx,
                );
                self.pending.push(task);
            }
            Tab::Explorer => {
                crate::explorer_tab::open_explorer_tab(pane, false, workspace, window, cx);
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

fn open_terminal(
    command: Option<String>,
    worktree_root: &Path,
    index: usize,
    pane: &Entity<Pane>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<anyhow::Result<()>> {
    let project = workspace.project().clone();
    let cwd = worktree_root.to_path_buf();
    let create = project.update(cx, |project, cx| match command {
        Some(command) => project.create_terminal_task(
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
        None => project.create_terminal_shell(Some(cwd), cx),
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
                "tabs": [{ "type": "explorer" }, { "type": "agent", "agent_id": "a" }],
                "active": 1,
            })))
        );
        assert_eq!(prune(&node(json!({ "tabs": [] }))), None);
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

        let layout = node(json!({ "split": "row", "sizes": [1.0, 3.0], "children": [
            { "tabs": [{ "type": "browser", "url": "" }, { "type": "browser", "url": "" }], "active": 1 },
            { "split": "column", "children": [
                { "tabs": [{ "type": "diff", "scope": "uncommitted" }] },
                { "tabs": [{ "type": "explorer" }] },
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
            assert_eq!(column.members.len(), 3, "the explorer is a pane tab now");
            assert_eq!(workspace.panes().len(), 4);
            assert_eq!(
                count_of::<crate::explorer_tab::ExplorerTab>(workspace, cx),
                1
            );
            assert_eq!(count_of::<crate::changes_tab::ChangesTab>(workspace, cx), 1);
        });

        apply(cx);
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.panes().len(),
                4,
                "applying again rebuilds, not nests"
            );
            let tabs: usize = workspace
                .panes()
                .iter()
                .map(|pane| browser_urls(pane, cx).len())
                .sum();
            assert_eq!(tabs, 3, "tabs the layout doesn't place are closed");
            assert_eq!(
                count_of::<crate::explorer_tab::ExplorerTab>(workspace, cx),
                1
            );
            assert_eq!(count_of::<crate::changes_tab::ChangesTab>(workspace, cx), 1);
            let changes = workspace
                .items_of_type::<crate::changes_tab::ChangesTab>(cx)
                .next()
                .expect("changes tab");
            assert_eq!(
                changes.read(cx).scope(),
                &crate::changes_tab::ChangesScope::Uncommitted
            );
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
