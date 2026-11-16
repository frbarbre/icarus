//! Terminals in Icarus workspaces run on the Icarus server and are shown with `icarus terminal attach`,
//! so quitting, crashing or updating the app never stops them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _};
use gpui_tokio::Tokio;
use icarus_client::Client;
use project::terminals::{RoutedShell, ShellRequest, ShellRouter};
use terminal_view::ExternalTerminalCloser;
use util::ResultExt as _;
use util::shell::Shell;

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

/// "folder — zsh", as Zed titles its own shells, instead of the attach command line.
fn tab_title(info: &serde_json::Value) -> Option<String> {
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
async fn server_terminal(
    client: Arc<Client>,
    cwd: Option<PathBuf>,
    reattach: Option<String>,
) -> Result<serde_json::Value> {
    if let Some(id) = reattach {
        let info = client
            .request("terminal.info", serde_json::json!({ "id": id }))
            .await;
        if let Ok(info) = info
            && info["running"].as_bool() == Some(true)
            && info["lost"].as_bool() != Some(true)
        {
            return Ok(info);
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
        .request(
            "terminal.create",
            serde_json::json!({ "cwd": cwd, "workspace": workspace }),
        )
        .await
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
    Some(Tokio::spawn_result(cx, async move {
        let info = server_terminal(client, request.cwd, reattach).await?;
        let id = info["id"]
            .as_str()
            .context("the server returned no terminal id")?
            .to_owned();
        Ok(RoutedShell {
            shell: Shell::WithArguments {
                program: icarus_program(),
                args: attach_args(server.as_deref(), home.as_deref(), &id),
                title_override: tab_title(&info),
            },
            external_id: qualified_id(&id, server.as_deref()),
        })
    }))
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
        assert_eq!(tab_title(&info).as_deref(), Some("demo-1 — zsh"));
        let info = serde_json::json!({ "cwd": "/w/demo-1", "command": ["/bin/bash"] });
        assert_eq!(tab_title(&info).as_deref(), Some("demo-1 — bash"));
        let info = serde_json::json!({ "cwd": "/w/demo-1", "command": [] });
        assert_eq!(tab_title(&info).as_deref(), Some("demo-1"));
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
            };
            assert_eq!(server_for(&reattach, cx), Some(Some("desk".into())));
        });
    }
}
