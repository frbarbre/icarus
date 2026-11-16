//! Orca's keyboard movement between tab groups (panes) and between terminal panes.

use gpui::{
    App, Bounds, Context, Entity, Focusable as _, Pixels, TaskExt as _, Window, actions, px,
};
use terminal_view::TerminalView;
use workspace::{Pane, Workspace};

actions!(
    icarus,
    [
        /// Focuses the tab group to the left of the focused one (⌥⌘←); with a single group, goes back in history.
        FocusGroupLeft,
        /// Focuses the tab group to the right of the focused one (⌥⌘→); with a single group, goes forward in history.
        FocusGroupRight,
        /// Focuses the tab group above the focused one (⌥⌘↑); does nothing at the edge.
        FocusGroupUp,
        /// Focuses the tab group below the focused one (⌥⌘↓); does nothing at the edge.
        FocusGroupDown,
        /// Focuses the next terminal pane, in the order the panes were created (⌘] in a terminal).
        FocusNextTerminalPane,
        /// Focuses the previous terminal pane, in the order the panes were created (⌘[ in a terminal).
        FocusPreviousTerminalPane,
    ]
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Orca allows this much overlap before a group stops counting as being on that side.
const EDGE_TOLERANCE: f32 = 2.0;

/// The candidate Orca would focus from `current` in `direction`: the nearest group whose facing
/// edge shares some of the current group's span, preferring the larger shared span on a tie.
pub fn nearest_in_direction(
    current: Bounds<Pixels>,
    candidates: &[Bounds<Pixels>],
    direction: Direction,
) -> Option<usize> {
    let mut best: Option<(usize, f32, f32)> = None;
    for (index, candidate) in candidates.iter().enumerate() {
        if *candidate == current {
            continue;
        }
        let gap = f32::from(match direction {
            Direction::Left => current.left() - candidate.right(),
            Direction::Right => candidate.left() - current.right(),
            Direction::Up => current.top() - candidate.bottom(),
            Direction::Down => candidate.top() - current.bottom(),
        });
        if gap < -EDGE_TOLERANCE {
            continue;
        }
        let shared = f32::from(match direction {
            Direction::Left | Direction::Right => {
                current.bottom().min(candidate.bottom()) - current.top().max(candidate.top())
            }
            Direction::Up | Direction::Down => {
                current.right().min(candidate.right()) - current.left().max(candidate.left())
            }
        });
        if shared <= 0.0 {
            continue;
        }
        let better = match best {
            None => true,
            Some((_, best_gap, best_shared)) => {
                gap < best_gap - EDGE_TOLERANCE
                    || ((gap - best_gap).abs() <= EDGE_TOLERANCE && shared > best_shared)
            }
        };
        if better {
            best = Some((index, gap, shared));
        }
    }
    best.map(|(index, _, _)| index)
}

fn focus_group(
    workspace: &mut Workspace,
    direction: Direction,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let active = workspace.active_pane().clone();
    let panes: Vec<Entity<Pane>> = workspace
        .center_pane_group()
        .panes()
        .into_iter()
        .cloned()
        .collect();
    // As in Orca, a lone group gives ⌥⌘←/→ back to history: here the pane's back/forward navigation.
    if panes.len() == 1 {
        let pane = active.downgrade();
        let navigation = match direction {
            Direction::Left => workspace.go_back(pane, window, cx),
            Direction::Right => workspace.go_forward(pane, window, cx),
            Direction::Up | Direction::Down => return,
        };
        navigation.detach_and_log_err(cx);
        return;
    }
    let Some(current) = workspace.bounding_box_for_pane(&active) else {
        return;
    };
    let mut rects = Vec::new();
    let mut targets = Vec::new();
    for pane in panes {
        if let Some(bounds) = workspace.bounding_box_for_pane(&pane)
            && bounds.size.width > px(0.)
            && bounds.size.height > px(0.)
        {
            rects.push(bounds);
            targets.push(pane);
        }
    }
    let Some(index) = nearest_in_direction(current, &rects, direction) else {
        return;
    };
    let Some(target) = targets.get(index) else {
        return;
    };
    focus_pane(target, window, cx);
}

fn focus_pane(pane: &Entity<Pane>, window: &mut Window, cx: &mut App) {
    match pane.read(cx).active_item() {
        Some(item) => item.item_focus_handle(cx).focus(window, cx),
        None => pane.focus_handle(cx).focus(window, cx),
    }
}

/// Steps through the panes showing a terminal, in creation order, wrapping like Orca's ⌘] / ⌘[.
fn focus_terminal_pane(
    workspace: &mut Workspace,
    forward: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let terminal_panes: Vec<Entity<Pane>> = workspace
        .panes()
        .iter()
        .filter(|pane| {
            pane.read(cx)
                .active_item()
                .is_some_and(|item| item.downcast::<TerminalView>().is_some())
        })
        .cloned()
        .collect();
    if terminal_panes.len() < 2 {
        return;
    }
    let active = workspace.active_pane();
    let position = terminal_panes.iter().position(|pane| pane == active);
    let next = match (position, forward) {
        (Some(index), true) => (index + 1) % terminal_panes.len(),
        (Some(index), false) => (index + terminal_panes.len() - 1) % terminal_panes.len(),
        (None, true) => 0,
        (None, false) => terminal_panes.len() - 1,
    };
    if let Some(pane) = terminal_panes.get(next) {
        focus_pane(pane, window, cx);
    }
}

pub(crate) fn register(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &FocusGroupLeft, window, cx| {
            focus_group(workspace, Direction::Left, window, cx)
        });
        workspace.register_action(|workspace, _: &FocusGroupRight, window, cx| {
            focus_group(workspace, Direction::Right, window, cx)
        });
        workspace.register_action(|workspace, _: &FocusGroupUp, window, cx| {
            focus_group(workspace, Direction::Up, window, cx)
        });
        workspace.register_action(|workspace, _: &FocusGroupDown, window, cx| {
            focus_group(workspace, Direction::Down, window, cx)
        });
        workspace.register_action(|workspace, _: &FocusNextTerminalPane, window, cx| {
            focus_terminal_pane(workspace, true, window, cx)
        });
        workspace.register_action(|workspace, _: &FocusPreviousTerminalPane, window, cx| {
            focus_terminal_pane(workspace, false, window, cx)
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext, VisualTestContext, bounds, point, size};
    use project::Project;
    use std::path::Path;
    use workspace::{MultiWorkspace, SplitDirection};

    async fn grid_window(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, Entity<Project>, VisualTestContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            register(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            serde_json::json!({ "a.txt": "a", "b.txt": "b" }),
        )
        .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let cx = VisualTestContext::from_window(multi_workspace.into(), cx);
        (workspace, project, cx)
    }

    fn focus(workspace: &Entity<Workspace>, pane: &Entity<Pane>, cx: &mut VisualTestContext) {
        let pane = pane.clone();
        workspace.update_in(cx, |_, window, cx| focus_pane(&pane, window, cx));
        cx.run_until_parked();
    }

    fn active(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Pane> {
        workspace.read_with(cx, |workspace, _| workspace.active_pane().clone())
    }

    #[gpui::test]
    async fn group_focus_follows_the_grid_and_stops_at_its_edges(cx: &mut TestAppContext) {
        let (workspace, _project, mut cx) = grid_window(cx).await;
        let (top_left, top_right, bottom_left, bottom_right) =
            workspace.update_in(&mut cx, |workspace, window, cx| {
                let top_left = workspace.active_pane().clone();
                let top_right =
                    workspace.split_pane(top_left.clone(), SplitDirection::Right, window, cx);
                let bottom_left =
                    workspace.split_pane(top_left.clone(), SplitDirection::Down, window, cx);
                let bottom_right =
                    workspace.split_pane(top_right.clone(), SplitDirection::Down, window, cx);
                (top_left, top_right, bottom_left, bottom_right)
            });
        cx.run_until_parked();
        focus(&workspace, &top_left, &mut cx);

        cx.dispatch_action(FocusGroupRight);
        cx.run_until_parked();
        assert_eq!(active(&workspace, &mut cx), top_right);
        cx.dispatch_action(FocusGroupRight);
        cx.run_until_parked();
        assert_eq!(
            active(&workspace, &mut cx),
            top_right,
            "no wrap at the edge"
        );
        cx.dispatch_action(FocusGroupDown);
        cx.run_until_parked();
        assert_eq!(active(&workspace, &mut cx), bottom_right);
        cx.dispatch_action(FocusGroupLeft);
        cx.run_until_parked();
        assert_eq!(active(&workspace, &mut cx), bottom_left);
        cx.dispatch_action(FocusGroupDown);
        cx.run_until_parked();
        assert_eq!(
            active(&workspace, &mut cx),
            bottom_left,
            "no wrap at the edge"
        );
        cx.dispatch_action(FocusGroupUp);
        cx.run_until_parked();
        assert_eq!(active(&workspace, &mut cx), top_left);
    }

    #[gpui::test]
    async fn a_lone_group_moves_through_history_and_split_groups_only_move_focus(
        cx: &mut TestAppContext,
    ) {
        let (workspace, project, mut cx) = grid_window(cx).await;
        cx.update(|_, cx| editor::init(cx));
        let worktree_id = project.read_with(&cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("worktree")
                .read(cx)
                .id()
        });
        for name in ["a.txt", "b.txt"] {
            workspace
                .update_in(&mut cx, |workspace, window, cx| {
                    workspace.open_path(
                        (worktree_id, util::rel_path::rel_path(name)),
                        None,
                        true,
                        window,
                        cx,
                    )
                })
                .await
                .expect("open");
            cx.run_until_parked();
        }
        let active_title = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .active_item(cx)
                    .map(|item| item.tab_content_text(0, cx).to_string())
            })
        };
        assert_eq!(active_title(&mut cx).as_deref(), Some("b.txt"));

        cx.dispatch_action(FocusGroupLeft);
        cx.run_until_parked();
        assert_eq!(active_title(&mut cx).as_deref(), Some("a.txt"), "back");
        cx.dispatch_action(FocusGroupRight);
        cx.run_until_parked();
        assert_eq!(active_title(&mut cx).as_deref(), Some("b.txt"), "forward");

        let left = active(&workspace, &mut cx);
        let right = workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.split_pane(left.clone(), SplitDirection::Right, window, cx)
        });
        cx.run_until_parked();
        focus(&workspace, &left, &mut cx);
        cx.dispatch_action(FocusGroupLeft);
        cx.run_until_parked();
        assert_eq!(active(&workspace, &mut cx), left, "nothing at the edge");
        assert_eq!(
            active_title(&mut cx).as_deref(),
            Some("b.txt"),
            "no history step with two groups"
        );
        cx.dispatch_action(FocusGroupRight);
        cx.run_until_parked();
        assert_eq!(active(&workspace, &mut cx), right);
    }

    fn add_terminal(
        workspace: &Entity<Workspace>,
        project: &Entity<Project>,
        pane: &Entity<Pane>,
        cx: &mut VisualTestContext,
    ) {
        let pane = pane.clone();
        let project = project.downgrade();
        workspace.update_in(cx, |workspace, window, cx| {
            let terminal = cx.new(|cx| {
                terminal::TerminalBuilder::new_display_only(
                    terminal::terminal_settings::CursorShape::default(),
                    terminal::terminal_settings::AlternateScroll::On,
                    None,
                    0,
                    cx.background_executor(),
                    util::paths::PathStyle::local(),
                )
                .subscribe(cx)
            });
            let weak_workspace = cx.entity().downgrade();
            let database_id = workspace.database_id();
            let view = cx.new(|cx| {
                TerminalView::new(terminal, weak_workspace, database_id, project, window, cx)
            });
            pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(view), true, true, None, window, cx)
            });
        });
    }

    #[gpui::test]
    async fn terminal_pane_cycling_skips_other_panes_and_wraps(cx: &mut TestAppContext) {
        let (workspace, project, mut cx) = grid_window(cx).await;
        let (first, other, second, third) =
            workspace.update_in(&mut cx, |workspace, window, cx| {
                let first = workspace.active_pane().clone();
                let other = workspace.split_pane(first.clone(), SplitDirection::Right, window, cx);
                let second = workspace.split_pane(other.clone(), SplitDirection::Right, window, cx);
                let third = workspace.split_pane(second.clone(), SplitDirection::Down, window, cx);
                (first, other, second, third)
            });
        for pane in [&first, &second, &third] {
            add_terminal(&workspace, &project, pane, &mut cx);
        }
        cx.run_until_parked();
        focus(&workspace, &first, &mut cx);

        cx.dispatch_action(FocusNextTerminalPane);
        cx.run_until_parked();
        assert_eq!(
            active(&workspace, &mut cx),
            second,
            "the non-terminal pane is skipped"
        );
        assert_ne!(active(&workspace, &mut cx), other);
        cx.dispatch_action(FocusNextTerminalPane);
        cx.run_until_parked();
        assert_eq!(active(&workspace, &mut cx), third);
        cx.dispatch_action(FocusNextTerminalPane);
        cx.run_until_parked();
        assert_eq!(
            active(&workspace, &mut cx),
            first,
            "it wraps like Orca's ⌘]"
        );
        cx.dispatch_action(FocusPreviousTerminalPane);
        cx.run_until_parked();
        assert_eq!(active(&workspace, &mut cx), third);
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        bounds(point(px(x), px(y)), size(px(w), px(h)))
    }

    #[test]
    fn a_two_by_two_grid_moves_to_the_neighbour_and_never_wraps() {
        let top_left = rect(0., 0., 100., 100.);
        let top_right = rect(100., 0., 100., 100.);
        let bottom_left = rect(0., 100., 100., 100.);
        let bottom_right = rect(100., 100., 100., 100.);
        let grid = [top_left, top_right, bottom_left, bottom_right];

        assert_eq!(
            nearest_in_direction(top_left, &grid, Direction::Right),
            Some(1)
        );
        assert_eq!(
            nearest_in_direction(top_left, &grid, Direction::Down),
            Some(2)
        );
        assert_eq!(
            nearest_in_direction(bottom_right, &grid, Direction::Left),
            Some(2)
        );
        assert_eq!(
            nearest_in_direction(bottom_right, &grid, Direction::Up),
            Some(1)
        );
        assert_eq!(nearest_in_direction(top_left, &grid, Direction::Left), None);
        assert_eq!(nearest_in_direction(top_left, &grid, Direction::Up), None);
        assert_eq!(
            nearest_in_direction(bottom_right, &grid, Direction::Right),
            None
        );
        assert_eq!(
            nearest_in_direction(bottom_right, &grid, Direction::Down),
            None
        );
    }

    #[test]
    fn diagonal_neighbours_are_never_chosen() {
        let current = rect(0., 0., 100., 100.);
        let diagonal = rect(100., 100., 100., 100.);
        assert_eq!(
            nearest_in_direction(current, &[current, diagonal], Direction::Right),
            None
        );
    }

    #[test]
    fn the_largest_shared_edge_wins_among_equally_near_groups() {
        // Left column spans the full height; the right side holds a short pane on top and a tall one below.
        let left = rect(0., 0., 100., 300.);
        let short = rect(100., 0., 100., 50.);
        let tall = rect(100., 50., 100., 250.);
        assert_eq!(
            nearest_in_direction(left, &[left, short, tall], Direction::Right),
            Some(2)
        );
    }

    #[test]
    fn a_nearer_group_beats_a_larger_shared_edge() {
        let current = rect(0., 0., 100., 300.);
        let near = rect(104., 0., 100., 50.);
        let far = rect(300., 0., 100., 300.);
        assert_eq!(
            nearest_in_direction(current, &[current, near, far], Direction::Right),
            Some(1)
        );
    }

    #[test]
    fn overlap_within_the_tolerance_still_counts_as_beside() {
        let current = rect(0., 0., 100., 100.);
        let overlapping = rect(99., 0., 100., 100.);
        let far_overlap = rect(90., 0., 100., 100.);
        assert_eq!(
            nearest_in_direction(current, &[current, overlapping], Direction::Right),
            Some(1)
        );
        assert_eq!(
            nearest_in_direction(current, &[current, far_overlap], Direction::Right),
            None
        );
    }
}
