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

use crate::{AppConnection, server_home, worktree_trust};

/// The `icarus` CLI shipped next to the app in a release bundle, else the one on `PATH`.
fn icarus_program() -> String {
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

fn route(request: ShellRequest, cx: &mut App) -> Option<gpui::Task<Result<RoutedShell>>> {
    let client = connection(cx)?;
    let routed = request.reattach.is_some()
        || request
            .cwd
            .as_deref()
            .is_some_and(|cwd| in_icarus_workspace(cwd, cx));
    if !routed {
        return None;
    }
    let home = server_home()?;
    Some(Tokio::spawn_result(cx, async move {
        let info = server_terminal(client, request.cwd, request.reattach).await?;
        let id = info["id"]
            .as_str()
            .context("the server returned no terminal id")?
            .to_owned();
        Ok(RoutedShell {
            shell: Shell::WithArguments {
                program: icarus_program(),
                args: vec![
                    "--home".to_owned(),
                    home.to_string_lossy().into_owned(),
                    "terminal".to_owned(),
                    "attach".to_owned(),
                    id.clone(),
                ],
                title_override: tab_title(&info),
            },
            external_id: id,
        })
    }))
}

fn close(id: String, cx: &mut App) {
    let Some(client) = connection(cx) else {
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
                client: None,
                workspace_paths: vec![PathBuf::from("/w/icarus")],
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
}
