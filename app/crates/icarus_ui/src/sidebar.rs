//! The right sidebar as Orca has it: Explorer, Source Control and GitHub, one at a time, switched by the
//! icons on top. It follows the active tab (FORK §29): Source Control for a diff, the Explorer for a file.

use std::sync::Arc;

use editor::Editor;
use gpui::{AnyElement, App, Context, Window};
use ui::{IconButton, Tab, Tooltip, prelude::*};
use workspace::Workspace;
use workspace::dock::{Dock, RightDockHeader};

use crate::changes_tab::SOURCE_CONTROL_PANEL;
use crate::diff_tab::DiffTab;

/// Zed's project panel, which is the sidebar's Explorer.
pub const EXPLORER_PANEL: &str = "Project Panel";
pub const GITHUB_PANEL: &str = "IcarusGitHubPanel";

/// The sidebar's panels in Orca's order, with the icon and name each shows in the switcher.
pub const SIDEBAR_PANELS: [(&str, IconName, &str); 3] = [
    (EXPLORER_PANEL, IconName::FileTree, "Explorer"),
    (SOURCE_CONTROL_PANEL, IconName::GitBranch, "Source Control"),
    (GITHUB_PANEL, IconName::PullRequest, "GitHub"),
];

/// Puts the switcher above the right dock.
pub fn install_switcher(cx: &mut App) {
    cx.set_global(RightDockHeader(Arc::new(render_switcher)));
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

/// The switcher above the right dock, shown only while one of the sidebar's panels is the active one.
fn render_switcher(
    dock: &Dock,
    _window: &mut Window,
    cx: &mut Context<Dock>,
) -> Option<AnyElement> {
    let active = dock.active_panel_index()?;
    let entries: Vec<(usize, IconName, &'static str)> = SIDEBAR_PANELS
        .iter()
        .filter_map(|(name, icon, label)| {
            dock.panel_index_for_persistent_name(name, cx)
                .map(|index| (index, *icon, *label))
        })
        .collect();
    if !entries.iter().any(|(index, _, _)| *index == active) {
        return None;
    }
    Some(
        h_flex()
            .h(Tab::container_height(cx))
            .flex_none()
            .px_1p5()
            .gap_0p5()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .children(entries.into_iter().map(|(index, icon, label)| {
                IconButton::new(("icarus-sidebar", index), icon)
                    .icon_size(IconSize::Small)
                    .toggle_state(index == active)
                    .tooltip(Tooltip::text(label))
                    .on_click(cx.listener(move |dock, _, window, cx| {
                        dock.activate_panel(index, window, cx);
                    }))
            }))
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
