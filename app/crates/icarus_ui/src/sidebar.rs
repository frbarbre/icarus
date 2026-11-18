//! The right sidebar as Orca has it: Explorer, Agents, Source Control and Checks, one at a time,
//! switched by the icons on top, with the workspace's links and next step at the strip's right end.
//! It follows the active tab (FORK §29): Source Control for a diff, the Explorer for a file.

use std::sync::Arc;

use editor::Editor;
use gpui::{AnyElement, App, Context, Window};
use ui::{IconButton, Indicator, Tab, Tooltip, prelude::*};
use workspace::Workspace;
use workspace::dock::{Dock, RightDockHeader, SharedSideDockWidth};

use crate::changes_tab::SOURCE_CONTROL_PANEL;
use crate::diff_tab::DiffTab;
use crate::github_panel::{CheckState, GitHubPanel};

/// Zed's project panel, which is the sidebar's Explorer.
pub const EXPLORER_PANEL: &str = "Project Panel";
/// The Checks tab: the pull request's checks and comments.
pub const GITHUB_PANEL: &str = "IcarusGitHubPanel";
pub use crate::session_history::SESSION_HISTORY_PANEL;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SidebarTab {
    pub panel: &'static str,
    pub icon: IconName,
    pub label: &'static str,
    /// The action whose key the tooltip shows, by name, since the Explorer's lives in another crate.
    pub action: Option<&'static str>,
}

/// The sidebar's tabs in Orca's order, with Orca's icons and names.
pub const SIDEBAR_PANELS: [SidebarTab; 4] = [
    SidebarTab {
        panel: EXPLORER_PANEL,
        icon: IconName::Files,
        label: "Explorer",
        action: Some("project_panel::ToggleFocus"),
    },
    SidebarTab {
        panel: SESSION_HISTORY_PANEL,
        icon: IconName::AgentSessionHistory,
        label: "Agents",
        action: Some("icarus::ToggleSessionHistory"),
    },
    SidebarTab {
        panel: SOURCE_CONTROL_PANEL,
        icon: IconName::GitBranch,
        label: "Source Control",
        action: Some("icarus::ToggleSourceControlPanel"),
    },
    SidebarTab {
        panel: GITHUB_PANEL,
        icon: IconName::ListChecks,
        label: "Checks",
        action: Some("icarus::ToggleGitHubPanel"),
    },
];

/// Puts the switcher above the right dock, whose width stays put as it switches panels.
pub fn install_switcher(cx: &mut App) {
    cx.set_global(RightDockHeader(Arc::new(render_switcher)));
    if !cx.has_global::<SharedSideDockWidth>() {
        cx.set_global(SharedSideDockWidth::default());
    }
}

pub fn register(cx: &mut App) {
    install_switcher(cx);
    cx.observe_new(|_: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let this = cx.entity();
        cx.subscribe_in(
            &this,
            window,
            |workspace, _, event: &workspace::Event, window, cx| {
                if matches!(event, workspace::Event::ActiveItemChanged) {
                    follow_active_item(workspace, window, cx);
                }
            },
        )
        .detach();
    })
    .detach();
}

/// The tabs the dock holds, in strip order, with each one's panel index.
pub fn visible_tabs(dock: &Dock, cx: &App) -> Vec<(usize, SidebarTab)> {
    SIDEBAR_PANELS
        .iter()
        .filter_map(|tab| {
            dock.panel_index_for_persistent_name(tab.panel, cx)
                .map(|index| (index, *tab))
        })
        .collect()
}

fn check_dot_color(state: CheckState) -> Option<Color> {
    match state {
        CheckState::Success => Some(Color::Success),
        CheckState::Failure => Some(Color::Error),
        CheckState::Pending => Some(Color::Warning),
        CheckState::Neutral | CheckState::Unknown => None,
    }
}

/// The strip above the right dock, shown only while one of the sidebar's panels is the active one.
fn render_switcher(dock: &Dock, window: &mut Window, cx: &mut Context<Dock>) -> Option<AnyElement> {
    let active = dock.active_panel_index()?;
    let tabs = visible_tabs(dock, cx);
    if !tabs.iter().any(|(index, _)| *index == active) {
        return None;
    }
    let checks = dock
        .panel::<GitHubPanel>()
        .and_then(|panel| panel.read(cx).checks_state())
        .and_then(check_dot_color);
    let links = crate::links::links_for_window(window, cx);
    let next_action = crate::NextActionButton::for_window(window, cx);
    Some(
        h_flex()
            .h(Tab::container_height(cx))
            .flex_none()
            .pl_1()
            .pr_1p5()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .children(tabs.into_iter().map(|(index, tab)| {
                        let selected = index == active;
                        let dot = (tab.panel == GITHUB_PANEL).then_some(checks).flatten();
                        div()
                            .relative()
                            .h_full()
                            .flex()
                            .items_center()
                            .child(
                                IconButton::new(("icarus-sidebar", index), tab.icon)
                                    .icon_size(IconSize::Small)
                                    .icon_color(if selected {
                                        Color::Default
                                    } else {
                                        Color::Muted
                                    })
                                    .tooltip(move |_window, cx| {
                                        match tab
                                            .action
                                            .and_then(|name| cx.build_action(name, None).ok())
                                        {
                                            Some(action) => {
                                                Tooltip::for_action(tab.label, action.as_ref(), cx)
                                            }
                                            None => Tooltip::simple(tab.label, cx),
                                        }
                                    })
                                    .on_click(cx.listener(move |dock, _, window, cx| {
                                        dock.activate_panel(index, window, cx);
                                    })),
                            )
                            .when_some(dot, |slot, color| {
                                slot.child(
                                    div()
                                        .absolute()
                                        .top(px(6.))
                                        .right(px(2.))
                                        .child(Indicator::dot().color(color)),
                                )
                            })
                            .when(selected, |slot| {
                                slot.child(
                                    div()
                                        .absolute()
                                        .bottom_0()
                                        .left(relative(0.25))
                                        .right(relative(0.25))
                                        .h(px(2.))
                                        .rounded_t_sm()
                                        .bg(cx.theme().colors().text),
                                )
                            })
                    })),
            )
            .children(links)
            .children(next_action)
            .into_any_element(),
    )
}

/// The sidebar panel the active tab asks for: Source Control for a diff tab, the Explorer for a file.
pub fn panel_for_active_item(workspace: &Workspace, cx: &App) -> Option<&'static str> {
    let item = workspace.active_item(cx)?;
    if item.downcast::<DiffTab>().is_some() {
        Some(SOURCE_CONTROL_PANEL)
    } else if item.downcast::<Editor>().is_some() {
        Some(EXPLORER_PANEL)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff_tab::{DiffBase, DiffTabMode, open_diff_tab};
    use gpui::{Entity, EventEmitter, FocusHandle, Focusable, Pixels, Render, px};
    use std::path::PathBuf;
    use workspace::dock::{DockPosition, Panel, PanelEvent};

    struct StandInExplorer {
        focus_handle: FocusHandle,
    }

    impl EventEmitter<PanelEvent> for StandInExplorer {}

    impl Focusable for StandInExplorer {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for StandInExplorer {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().track_focus(&self.focus_handle)
        }
    }

    impl Panel for StandInExplorer {
        fn persistent_name() -> &'static str {
            EXPLORER_PANEL
        }

        fn panel_key() -> &'static str {
            EXPLORER_PANEL
        }

        fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
            DockPosition::Right
        }

        fn position_is_valid(&self, position: DockPosition) -> bool {
            position == DockPosition::Right
        }

        fn set_position(
            &mut self,
            _position: DockPosition,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
        }

        fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
            px(240.)
        }

        fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
            Some(IconName::FileTree)
        }

        fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
            Some("Explorer")
        }

        fn toggle_action(&self) -> Box<dyn gpui::Action> {
            Box::new(workspace::ToggleRightDock)
        }

        fn activation_priority(&self) -> u32 {
            1
        }
    }

    fn active_panel(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
    ) -> Option<&'static str> {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .right_dock()
                .read(cx)
                .active_panel()
                .map(|panel| panel.persistent_name())
        })
    }

    #[test]
    fn the_strip_lists_orcas_tabs_in_orcas_order() {
        let tabs: Vec<(&str, IconName)> = SIDEBAR_PANELS
            .iter()
            .map(|tab| (tab.label, tab.icon))
            .collect();
        assert_eq!(
            tabs,
            [
                ("Explorer", IconName::Files),
                ("Agents", IconName::AgentSessionHistory),
                ("Source Control", IconName::GitBranch),
                ("Checks", IconName::ListChecks),
            ]
        );
    }

    #[test]
    fn each_tab_key_reaches_its_panel_from_editors_terminals_and_the_composer() {
        let mac = include_str!("../../../assets/keymaps/default-macos.json");
        let linux = include_str!("../../../assets/keymaps/default-linux.json");
        for (keymap, key, action) in [
            (mac, "cmd-shift-e", "project_panel::ToggleFocus"),
            (mac, "cmd-shift-g", "icarus::ToggleSourceControlPanel"),
            (mac, "cmd-shift-k", "icarus::ToggleGitHubPanel"),
            (linux, "ctrl-shift-e", "project_panel::ToggleFocus"),
            (linux, "ctrl-shift-g", "icarus::ToggleSourceControlPanel"),
            (linux, "ctrl-shift-k", "icarus::ToggleGitHubPanel"),
        ] {
            assert!(
                SIDEBAR_PANELS.iter().any(|tab| tab.action == Some(action)),
                "{action} isn't a tab's action"
            );
            // The settings window is a window of its own, with no sidebar to reach.
            let bindings: Vec<_> = crate::links::tests::reaching(keymap, key)
                .into_iter()
                .filter(|(context, _)| context != "SettingsWindow")
                .collect();
            assert!(
                bindings
                    .iter()
                    .any(|(context, bound)| context == "Workspace" && bound == action),
                "{key} isn't bound to {action}: {bindings:?}"
            );
            let shadows: Vec<_> = bindings
                .iter()
                .filter(|(_, bound)| bound != action)
                .collect();
            assert!(shadows.is_empty(), "{key} is taken by {shadows:?}");
        }
    }

    #[gpui::test]
    async fn the_strip_shows_the_docks_tabs_in_orcas_order(cx: &mut gpui::TestAppContext) {
        let (workspace, mut cx) = crate::diff_tab::tests::git_workspace(cx).await;
        let cx = &mut cx;
        workspace.update_in(cx, |workspace, window, cx| {
            let checks = cx
                .new(|cx| GitHubPanel::preview(workspace.weak_handle(), serde_json::json!({}), cx));
            workspace.add_panel(checks, window, cx);
            let source_control =
                cx.new(|cx| crate::changes_tab::SourceControlPanel::new(workspace, window, cx));
            workspace.add_panel(source_control, window, cx);
            let explorer = cx.new(|cx| StandInExplorer {
                focus_handle: cx.focus_handle(),
            });
            workspace.add_panel(explorer, window, cx);
        });
        let labels: Vec<&str> = workspace.read_with(cx, |workspace, cx| {
            visible_tabs(workspace.right_dock().read(cx), cx)
                .into_iter()
                .map(|(_, tab)| tab.label)
                .collect()
        });
        assert_eq!(
            labels,
            ["Explorer", "Source Control", "Checks"],
            "a tab shows only when its panel is in the dock"
        );
    }

    #[gpui::test]
    async fn a_new_workspace_opens_its_sidebar_on_source_control(cx: &mut gpui::TestAppContext) {
        let (workspace, mut cx) = crate::diff_tab::tests::git_workspace(cx).await;
        let cx = &mut cx;
        cx.update(|_, cx| crate::enable_for_tests(cx));
        workspace.update_in(cx, |workspace, window, cx| {
            let source_control =
                cx.new(|cx| crate::changes_tab::SourceControlPanel::new(workspace, window, cx));
            workspace.add_panel(source_control, window, cx);
            let explorer = cx.new(|cx| StandInExplorer {
                focus_handle: cx.focus_handle(),
            });
            workspace.add_panel(explorer, window, cx);
            // Zed's Explorer opens itself on startup; Source Control still comes first.
            workspace.open_panel::<StandInExplorer>(window, cx);
            crate::open_source_control_first(workspace, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(active_panel(&workspace, cx), Some(SOURCE_CONTROL_PANEL));
        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace.right_dock().read(cx).is_open()
        }));
    }

    #[gpui::test]
    async fn the_sidebar_follows_the_active_tab_between_explorer_and_source_control(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, mut cx) = crate::diff_tab::tests::git_workspace(cx).await;
        let cx = &mut cx;
        workspace.update_in(cx, |workspace, window, cx| {
            let explorer = cx.new(|cx| StandInExplorer {
                focus_handle: cx.focus_handle(),
            });
            workspace.add_panel(explorer, window, cx);
            let source_control =
                cx.new(|cx| crate::changes_tab::SourceControlPanel::new(workspace, window, cx));
            workspace.add_panel(source_control, window, cx);
            workspace.open_panel::<StandInExplorer>(window, cx);
        });
        assert_eq!(active_panel(&workspace, cx), Some(EXPLORER_PANEL));

        let diff = workspace.update_in(cx, |workspace, window, cx| {
            open_diff_tab(
                PathBuf::from("/project/src/app.rs"),
                DiffBase::Uncommitted,
                DiffTabMode::Diff,
                None,
                workspace,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        diff.await.expect("diff tab");
        cx.run_until_parked();
        assert_eq!(active_panel(&workspace, cx), Some(SOURCE_CONTROL_PANEL));

        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
        let buffer = project.update(cx, |project, cx| {
            project.open_local_buffer("/project/src/new.rs", cx)
        });
        cx.run_until_parked();
        let buffer = buffer.await.expect("buffer");
        workspace.update_in(cx, |workspace, window, cx| {
            let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
            workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(active_panel(&workspace, cx), Some(EXPLORER_PANEL));
    }
}

fn follow_active_item(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let dock = workspace.right_dock().clone();
    let current = {
        let dock = dock.read(cx);
        if !dock.is_open() {
            return;
        }
        dock.active_panel().map(|panel| panel.persistent_name())
    };
    // Only switches between Explorer and Source Control; GitHub stays put while it's being read.
    if !matches!(current, Some(EXPLORER_PANEL | SOURCE_CONTROL_PANEL)) {
        return;
    }
    let Some(target) = panel_for_active_item(workspace, cx) else {
        return;
    };
    if current == Some(target) {
        return;
    }
    dock.update(cx, |dock, cx| {
        if let Some(index) = dock.panel_index_for_persistent_name(target, cx) {
            dock.activate_panel(index, window, cx);
        }
    });
}
