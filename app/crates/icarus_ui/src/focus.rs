//! Tells the app's server which workspace this app shows (`client.focus`), so the server only marks
//! workspaces unread that nobody is looking at, and clears the mark when one comes into view.

use std::path::PathBuf;

use gpui::{App, Global, TaskExt as _};
use serde_json::json;
use workspace::{MultiWorkspace, MultiWorkspaceEvent};

#[derive(Default)]
struct FocusState {
    /// What the server last heard; `None` until the first report on this connection.
    sent: Option<Option<PathBuf>>,
}

impl Global for FocusState {}

pub fn register(cx: &mut App) {
    cx.observe_new(|_: &mut MultiWorkspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        cx.observe_window_activation(window, |_, _, cx| cx.defer(report))
            .detach();
        cx.subscribe_self(|_, event: &MultiWorkspaceEvent, cx| {
            if matches!(event, MultiWorkspaceEvent::ActiveWorkspaceChanged { .. }) {
                cx.defer(report);
            }
        })
        .detach();
    })
    .detach();
}

/// A new connection knows nothing of this app's focus yet.
pub fn connected(cx: &mut App) {
    cx.default_global::<FocusState>().sent = None;
    report(cx);
}

/// The first folder of the active window's workspace, or none while the app isn't frontmost.
fn focused_path(cx: &mut App) -> Option<PathBuf> {
    let active = cx.active_window()?.downcast::<MultiWorkspace>()?;
    let workspace = active.read(cx).ok()?.workspace().clone();
    let root = workspace.read(cx).root_paths(cx).into_iter().next()?;
    Some(root.to_path_buf())
}

pub fn report(cx: &mut App) {
    let path = focused_path(cx);
    let state = cx.default_global::<FocusState>();
    if state.sent.as_ref() == Some(&path) {
        return;
    }
    let Some(task) = crate::review::request(
        crate::app_client(cx),
        "client.focus",
        json!({ "path": path }),
        cx,
    ) else {
        return;
    };
    cx.default_global::<FocusState>().sent = Some(path);
    task.detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::tests::{calls_to, stub_rpc};
    use gpui::{TestAppContext, VisualTestContext};
    use project::Project;
    use serde_json::Value;
    use std::path::Path;

    #[gpui::test]
    async fn the_server_hears_which_workspace_the_frontmost_window_shows(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            register(cx);
        });
        let calls = cx.update(|cx| stub_rpc(cx, |_, _| Ok(json!({}))));
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "a.txt": "a" })).await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        // Outside any window update, as the app's deferred reports run.
        let app = |cx: &mut VisualTestContext, f: fn(&mut App)| {
            let app: &mut TestAppContext = cx;
            app.update(f);
        };
        app(&mut cx, connected);
        cx.run_until_parked();
        let paths = |calls: &std::sync::Mutex<Vec<(String, Value)>>| {
            calls_to(calls, "client.focus")
                .into_iter()
                .map(|params| params["path"].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(&calls).last(), Some(&json!("/project")));
        let sent = paths(&calls).len();
        app(&mut cx, report);
        assert_eq!(
            paths(&calls).len(),
            sent,
            "an unchanged focus is not resent"
        );

        cx.deactivate_window();
        app(&mut cx, report);
        cx.run_until_parked();
        assert_eq!(
            paths(&calls).last(),
            Some(&Value::Null),
            "a window in the background shows nothing"
        );
    }
}
