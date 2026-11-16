//! Terminals in Icarus workspaces run on the Icarus server and are shown with `icarus terminal attach`,
//! so quitting, crashing or updating the app never stops them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use collections::HashSet;
use gpui::{App, AppContext as _, Context, Entity, WeakEntity, Window, actions};
use gpui_tokio::Tokio;
use icarus_client::Client;
use project::terminals::{RoutedShell, ShellRequest, ShellRouter, TaskRequest};
use terminal_view::{
    ExternalKeyContext, ExternalTerminalCloser, ExternalTerminalRetitled, TerminalView,
};
use util::ResultExt as _;
use util::shell::Shell;
use workspace::Workspace;

actions!(
    icarus,
    [
        /// Names the focused terminal pane (Orca's "Set Title…"); the server keeps the name for every window and the CLI.
        SetPaneTitle,
        /// Clears the focused terminal pane's name, so it shows its program's title again.
        ClearPaneTitle,
    ]
);

use crate::routing::{self, ServerRef};
use crate::{AppConnection, server_home, worktree_trust};

/// The `icarus` CLI shipped next to the app in a release bundle, else the one on `PATH`.
pub(crate) fn icarus_program() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("icarus")))
        .filter(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "icarus".to_owned())
}

fn connection(cx: &App) -> Option<Arc<Client>> {
    cx.try_global::<AppConnection>()
        .and_then(|connection| connection.client.clone())
}

fn in_icarus_workspace(cwd: &Path, cx: &App) -> bool {
    cx.try_global::<AppConnection>().is_some_and(|connection| {
        connection
            .workspace_paths
            .iter()
            .any(|root| cwd.starts_with(root))
    })
}

/// A labelled terminal (the workspace's "Setup", a quick command, a pane title) is named by its label.
fn label(info: &serde_json::Value) -> Option<String> {
    info["label"]
        .as_str()
        .filter(|label| !label.trim().is_empty())
        .map(str::to_owned)
}

/// "folder — zsh" as Zed titles its own shells, never the attach command line; shown while the
/// program has set no title of its own.
fn fallback_title(info: &serde_json::Value) -> Option<String> {
    let folder = Path::new(info["cwd"].as_str()?)
        .file_name()?
        .to_string_lossy()
        .into_owned();
    let program = info["command"]
        .as_array()
        .and_then(|command| command.first())
        .and_then(|program| program.as_str())
        .and_then(|program| Path::new(program).file_name())
        .map(|name| name.to_string_lossy().trim_start_matches('-').to_owned());
    Some(match program {
        Some(program) => format!("{folder} — {program}"),
        None => folder,
    })
}

/// The server terminal a tab shows: the one it showed before if that still runs, else a new one.
/// A restored task tab whose task has ended fails instead, so it doesn't come back as a stray shell.
async fn server_terminal(
    client: Arc<Client>,
    cwd: Option<PathBuf>,
    reattach: Option<String>,
    task: Option<TaskRequest>,
) -> Result<serde_json::Value> {
    if let Some(id) = reattach {
        let info = client
            .request("terminal.info", serde_json::json!({ "id": id }))
            .await;
        match info {
            Ok(info)
                if info["running"].as_bool() == Some(true)
                    && info["lost"].as_bool() != Some(true) =>
            {
                return Ok(info);
            }
            Ok(info) if info["task"].as_bool() == Some(true) => {
                anyhow::bail!("task terminal {id} has finished")
            }
            _ => {}
        }
    }
    let workspace = match &cwd {
        Some(cwd) => worktree_trust::list_workspace_paths(client.clone())
            .await?
            .into_iter()
            .filter(|(_, root)| cwd.starts_with(root))
            .max_by_key(|(_, root)| root.components().count())
            .map(|(id, _)| id),
        None => None,
    };
    client
        .request("terminal.create", create_params(cwd, workspace, task))
        .await
}

fn create_params(
    cwd: Option<PathBuf>,
    workspace: Option<String>,
    task: Option<TaskRequest>,
) -> serde_json::Value {
    match task {
        Some(task) => serde_json::json!({
            "cwd": cwd,
            "workspace": workspace,
            "command": task.command,
            "env": task.env,
            "label": task.label,
            "task": true,
        }),
        None => serde_json::json!({ "cwd": cwd, "workspace": workspace }),
    }
}

/// A tab's terminal id names its server when that's a saved one (`term-1@desk`), since ids are per server.
fn qualified_id(id: &str, server: Option<&str>) -> String {
    match server {
        Some(server) => format!("{id}@{server}"),
        None => id.to_owned(),
    }
}

fn split_id(external: &str) -> (String, Option<String>) {
    match external.rsplit_once('@') {
        Some((id, server)) if !server.is_empty() => (id.to_owned(), Some(server.to_owned())),
        _ => (external.to_owned(), None),
    }
}

/// `icarus [--server NAME | --home HOME] terminal attach ID`: a saved server is reached by name,
/// this machine's by its state directory.
fn attach_args(server: Option<&str>, home: Option<&Path>, id: &str) -> Vec<String> {
    let mut args = match (server, home) {
        (Some(server), _) => vec!["--server".to_owned(), server.to_owned()],
        (None, Some(home)) => vec!["--home".to_owned(), home.to_string_lossy().into_owned()],
        (None, None) => Vec::new(),
    };
    args.extend(["terminal".to_owned(), "attach".to_owned(), id.to_owned()]);
    args
}

/// The saved server a request goes to (`None`: this machine), or no routing at all.
fn server_for(request: &ShellRequest, cx: &App) -> Option<Option<String>> {
    if let Some(reattach) = &request.reattach {
        return Some(split_id(reattach).1.or_else(|| routing::primary(cx)));
    }
    let cwd = request.cwd.as_deref()?;
    let server = if in_icarus_workspace(cwd, cx) {
        ServerRef::Primary
    } else {
        routing::server_for_path(cwd, cx)?
    };
    Some(match server {
        ServerRef::Primary => routing::primary(cx),
        ServerRef::Saved(name) => Some(name),
    })
}

fn client_for(server: Option<&str>, cx: &App) -> Option<Arc<Client>> {
    match server {
        Some(name) if routing::primary(cx).as_deref() != Some(name) => {
            routing::client_for(&ServerRef::Saved(name.to_owned()), cx)
        }
        _ => connection(cx),
    }
}

fn route(request: ShellRequest, cx: &mut App) -> Option<gpui::Task<Result<RoutedShell>>> {
    let server = server_for(&request, cx)?;
    let client = client_for(server.as_deref(), cx)?;
    let home = server_home();
    let reattach = request
        .reattach
        .as_deref()
        .map(|external| split_id(external).0);
    let task = Tokio::spawn_result(cx, async move {
        let info = server_terminal(client, request.cwd, reattach, request.task).await?;
        let id = info["id"]
            .as_str()
            .context("the server returned no terminal id")?
            .to_owned();
        let routed = RoutedShell {
            shell: Shell::WithArguments {
                program: icarus_program(),
                args: attach_args(server.as_deref(), home.as_deref(), &id),
                title_override: label(&info),
            },
            external_id: qualified_id(&id, server.as_deref()),
            fallback_title: fallback_title(&info),
        };
        anyhow::Ok((routed, info["kitty_keyboard_flags"].as_u64().unwrap_or(0)))
    });
    Some(cx.spawn(async move |cx| {
        let (routed, kitty_flags) = task.await?;
        let external_id = routed.external_id.clone();
        cx.update(|cx| set_kitty_keyboard(&external_id, kitty_flags > 0, cx));
        Ok(routed)
    }))
}

/// Server terminals whose program turned on the kitty keyboard protocol, by tab terminal id.
#[derive(Default)]
struct KittyKeyboardTerminals(HashSet<String>);

impl gpui::Global for KittyKeyboardTerminals {}

/// Terminal tabs, so a server's terminal events reach the tabs showing that terminal.
#[derive(Default)]
struct TerminalViews(Vec<WeakEntity<TerminalView>>);

impl gpui::Global for TerminalViews {}

fn views_showing(external_id: &str, cx: &mut App) -> Vec<Entity<TerminalView>> {
    let views = cx.default_global::<TerminalViews>();
    views.0.retain(|view| view.upgrade().is_some());
    let views: Vec<_> = views.0.iter().filter_map(WeakEntity::upgrade).collect();
    views
        .into_iter()
        .filter(|view| view.read(cx).terminal().read(cx).external_id() == Some(external_id))
        .collect()
}

fn set_kitty_keyboard(external_id: &str, on: bool, cx: &mut App) {
    let set = &mut cx.default_global::<KittyKeyboardTerminals>().0;
    let changed = if on {
        set.insert(external_id.to_owned())
    } else {
        set.remove(external_id)
    };
    if changed {
        for view in views_showing(external_id, cx) {
            view.update(cx, |_, cx| cx.notify());
        }
    }
}

/// A server's `terminal.keyboard` / `terminal.label` event; `server` is the saved server it came from, if any.
pub(crate) fn terminal_changed(
    server: Option<&str>,
    topic: &str,
    payload: &serde_json::Value,
    cx: &mut App,
) {
    let Some(id) = payload["id"].as_str() else {
        return;
    };
    let external_id = qualified_id(id, server);
    match topic {
        "terminal.keyboard" => {
            let on = payload["flags"].as_u64().unwrap_or(0) > 0;
            set_kitty_keyboard(&external_id, on, cx);
        }
        "terminal.label" => {
            let label = payload["label"].as_str().map(str::to_owned);
            for view in views_showing(&external_id, cx) {
                view.update(cx, |view, cx| view.set_custom_title(label.clone(), cx));
            }
        }
        _ => {}
    }
}

fn retitle(external_id: String, label: Option<String>, cx: &mut App) {
    let (id, server) = split_id(&external_id);
    let Some(client) = client_for(server.as_deref(), cx) else {
        return;
    };
    let request = Tokio::spawn_result(cx, async move {
        client
            .request(
                "terminal.set_label",
                serde_json::json!({ "id": id, "label": label }),
            )
            .await
    });
    cx.background_spawn(async move { request.await.log_err() })
        .detach();
}

fn set_pane_title(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(view) = active_terminal(workspace, cx) else {
        return;
    };
    view.update(cx, |view, cx| {
        view.rename_terminal(&terminal_view::RenameTerminal, window, cx)
    });
}

fn clear_pane_title(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some(view) = active_terminal(workspace, cx) else {
        return;
    };
    if let Some(external_id) = view.read(cx).terminal().read(cx).external_id() {
        retitle(external_id.to_owned(), None, cx);
    }
    view.update(cx, |view, cx| view.set_custom_title(None, cx));
}

fn active_terminal(workspace: &Workspace, cx: &App) -> Option<Entity<TerminalView>> {
    workspace
        .active_pane()
        .read(cx)
        .active_item()?
        .downcast::<TerminalView>()
}

fn close(external: String, cx: &mut App) {
    let (id, server) = split_id(&external);
    let Some(client) = client_for(server.as_deref(), cx) else {
        return;
    };
    let kill = Tokio::spawn_result(cx, async move {
        client
            .request(
                "terminal.kill",
                serde_json::json!({ "id": id, "remove": true }),
            )
            .await
    });
    cx.background_spawn(async move { kill.await.log_err() })
        .detach();
}

pub(crate) fn register(cx: &mut App) {
    cx.set_global(ShellRouter(Arc::new(route)));
    cx.set_global(ExternalTerminalCloser(Arc::new(close)));
    cx.set_global(ExternalTerminalRetitled(Arc::new(retitle)));
    cx.set_global(ExternalKeyContext(Arc::new(|external_id, context, cx| {
        if cx
            .try_global::<KittyKeyboardTerminals>()
            .is_some_and(|terminals| terminals.0.contains(external_id))
        {
            context.add("kitty_keyboard");
        }
    })));
    register_views(cx);
}

/// The parts that need no server: tracking tabs and the pane-title actions.
pub(crate) fn register_views(cx: &mut App) {
    cx.observe_new(|_: &mut TerminalView, _, cx| {
        let view = cx.entity().downgrade();
        cx.defer(move |cx| cx.default_global::<TerminalViews>().0.push(view));
    })
    .detach();
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &SetPaneTitle, window, cx| {
            set_pane_title(workspace, window, cx)
        });
        workspace.register_action(|workspace, _: &ClearPaneTitle, _, cx| {
            clear_pane_title(workspace, cx)
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_cli_wins_over_path_only_when_it_exists() {
        let program = icarus_program();
        assert!(program == "icarus" || Path::new(&program).is_file());
    }

    #[test]
    fn tabs_are_titled_like_zeds_own_shells() {
        let info = serde_json::json!({ "cwd": "/w/demo-1", "command": ["-zsh", "-l"] });
        assert_eq!(fallback_title(&info).as_deref(), Some("demo-1 — zsh"));
        let info = serde_json::json!({ "cwd": "/w/demo-1", "command": ["/bin/bash"] });
        assert_eq!(fallback_title(&info).as_deref(), Some("demo-1 — bash"));
        let info = serde_json::json!({ "cwd": "/w/demo-1", "command": [] });
        assert_eq!(fallback_title(&info).as_deref(), Some("demo-1"));
        let info =
            serde_json::json!({ "cwd": "/w/demo-1", "command": ["/bin/zsh"], "label": "Setup" });
        assert_eq!(label(&info).as_deref(), Some("Setup"));
    }

    #[gpui::test]
    fn shells_outside_icarus_workspaces_stay_local(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_global(AppConnection {
                workspace_paths: vec![PathBuf::from("/w/icarus")],
                ..AppConnection::default()
            });
            assert!(in_icarus_workspace(Path::new("/w/icarus/src"), cx));
            assert!(!in_icarus_workspace(Path::new("/w/other"), cx));
            let request = ShellRequest {
                cwd: Some(PathBuf::from("/w/icarus")),
                reattach: None,
                task: None,
            };
            assert!(
                route(request, cx).is_none(),
                "without a server connection the shell stays local"
            );
        });
    }

    #[test]
    fn terminal_ids_name_their_saved_server() {
        assert_eq!(qualified_id("term-1", Some("desk")), "term-1@desk");
        assert_eq!(qualified_id("term-1", None), "term-1");
        assert_eq!(
            split_id("term-1@desk"),
            ("term-1".into(), Some("desk".into()))
        );
        assert_eq!(split_id("term-1"), ("term-1".into(), None));
    }

    #[test]
    fn remote_terminals_attach_through_the_named_server() {
        assert_eq!(
            attach_args(Some("desk"), Some(Path::new("/h")), "term-1"),
            ["--server", "desk", "terminal", "attach", "term-1"]
        );
        assert_eq!(
            attach_args(None, Some(Path::new("/h")), "term-1"),
            ["--home", "/h", "terminal", "attach", "term-1"]
        );
    }

    #[gpui::test]
    async fn shells_in_a_saved_servers_workspace_route_to_that_server(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_, panel, mut cx) =
            crate::tests::panel_window(vec![crate::tests::summary("main", "/w/main", None)], cx)
                .await;
        let store = panel.read_with(&cx, |panel, _| panel.remote_servers().clone());
        store.update(&mut cx, |store, cx| {
            store.set_saved(
                &[crate::servers::tests::saved("desk", "100.64.0.2:4000")],
                cx,
            );
            store.set_workspaces(
                "desk",
                vec![crate::tests::summary("remote", "/srv/ws/remote", None)],
                cx,
            );
        });
        cx.update(|_, cx| {
            cx.default_global::<AppConnection>().workspace_paths = vec![PathBuf::from("/w/main")];
            let request = |cwd: &str| ShellRequest {
                cwd: Some(PathBuf::from(cwd)),
                reattach: None,
                task: None,
            };
            assert_eq!(
                server_for(&request("/srv/ws/remote/src"), cx),
                Some(Some("desk".into()))
            );
            assert_eq!(server_for(&request("/w/main"), cx), Some(None));
            assert_eq!(server_for(&request("/elsewhere"), cx), None);
            let reattach = ShellRequest {
                cwd: None,
                reattach: Some("term-9@desk".into()),
                task: None,
            };
            assert_eq!(server_for(&reattach, cx), Some(Some("desk".into())));
        });
    }

    #[test]
    fn a_task_is_created_as_a_labelled_task_terminal_with_its_command_and_env() {
        let params = create_params(
            Some(PathBuf::from("/w/demo")),
            Some("ws-1".into()),
            Some(TaskRequest {
                label: "cargo test".into(),
                command: Some(vec!["/bin/zsh".into(), "-c".into(), "cargo test".into()]),
                env: [("RUST_LOG".to_string(), "info".to_string())]
                    .into_iter()
                    .collect(),
            }),
        );
        assert_eq!(
            params,
            serde_json::json!({
                "cwd": "/w/demo",
                "workspace": "ws-1",
                "command": ["/bin/zsh", "-c", "cargo test"],
                "env": { "RUST_LOG": "info" },
                "label": "cargo test",
                "task": true,
            })
        );
        assert_eq!(
            create_params(None, None, None),
            serde_json::json!({ "cwd": null, "workspace": null })
        );
    }

    #[test]
    fn unlabelled_terminals_fall_back_to_folder_and_shell_but_labels_win() {
        let info = serde_json::json!({ "cwd": "/w/demo-1", "command": ["/bin/zsh"] });
        assert_eq!(label(&info), None);
        assert_eq!(fallback_title(&info).as_deref(), Some("demo-1 — zsh"));
        let info = serde_json::json!({ "cwd": "/w/demo-1", "command": ["/bin/zsh"], "label": " " });
        assert_eq!(label(&info), None, "a blank label is no label");
    }

    fn routed_terminal_view(
        external_id: &str,
        cx: &mut gpui::VisualTestContext,
        workspace: &Entity<Workspace>,
    ) -> Entity<TerminalView> {
        let external_id = external_id.to_owned();
        workspace.update_in(cx, |workspace, window, cx| {
            let terminal = cx.new(|cx| {
                let mut terminal = terminal::TerminalBuilder::new_display_only(
                    terminal::terminal_settings::CursorShape::default(),
                    terminal::terminal_settings::AlternateScroll::On,
                    None,
                    0,
                    cx.background_executor(),
                    util::paths::PathStyle::local(),
                )
                .subscribe(cx);
                terminal.set_external_id(Some(external_id));
                terminal
            });
            let project = workspace.project().downgrade();
            let weak_workspace = cx.entity().downgrade();
            let view =
                cx.new(|cx| TerminalView::new(terminal, weak_workspace, None, project, window, cx));
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
            view
        })
    }

    #[gpui::test]
    async fn server_label_and_keyboard_events_reach_the_tabs_showing_that_terminal(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, _panel, mut cx) =
            crate::tests::panel_window(vec![crate::tests::summary("main", "/w/main", None)], cx)
                .await;
        cx.update(|_, cx| register_views(cx));
        let local = routed_terminal_view("term-1", &mut cx, &workspace);
        let remote = routed_terminal_view("term-1@desk", &mut cx, &workspace);
        cx.run_until_parked();

        cx.update(|_, cx| {
            terminal_changed(
                None,
                "terminal.label",
                &serde_json::json!({ "id": "term-1", "label": "Logs" }),
                cx,
            );
            terminal_changed(
                Some("desk"),
                "terminal.keyboard",
                &serde_json::json!({ "id": "term-1", "flags": 1 }),
                cx,
            );
        });
        local.read_with(&cx, |view, _| assert_eq!(view.custom_title(), Some("Logs")));
        remote.read_with(&cx, |view, _| assert_eq!(view.custom_title(), None));
        cx.update(|_, cx| {
            let kitty = &cx.global::<KittyKeyboardTerminals>().0;
            assert!(kitty.contains("term-1@desk") && !kitty.contains("term-1"));
            terminal_changed(
                None,
                "terminal.label",
                &serde_json::json!({ "id": "term-1", "label": null }),
                cx,
            );
            terminal_changed(
                Some("desk"),
                "terminal.keyboard",
                &serde_json::json!({ "id": "term-1", "flags": 0 }),
                cx,
            );
            assert!(cx.global::<KittyKeyboardTerminals>().0.is_empty());
        });
        local.read_with(&cx, |view, _| assert_eq!(view.custom_title(), None));
    }

    #[gpui::test]
    async fn set_and_clear_pane_title_act_on_the_focused_terminal(cx: &mut gpui::TestAppContext) {
        let (workspace, _panel, mut cx) =
            crate::tests::panel_window(vec![crate::tests::summary("main", "/w/main", None)], cx)
                .await;
        cx.update(|_, cx| register_views(cx));
        let view = routed_terminal_view("term-1", &mut cx, &workspace);
        cx.run_until_parked();
        workspace.update_in(&mut cx, |workspace, window, cx| {
            set_pane_title(workspace, window, cx);
            assert!(
                view.read(cx).is_renaming(),
                "Set Title opens the tab's rename field"
            );
        });
        view.update(&mut cx, |view, cx| {
            view.set_custom_title(Some("Logs".into()), cx)
        });
        workspace.update(&mut cx, |workspace, cx| clear_pane_title(workspace, cx));
        view.read_with(&cx, |view, _| assert_eq!(view.custom_title(), None));
    }
}
