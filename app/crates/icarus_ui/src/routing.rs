//! Which Icarus server a window, folder or agent belongs to: the app's own connection, or one of
//! the saved servers it watches (plan §2: one client, many servers).

use std::path::Path;
use std::sync::Arc;

use gpui::{App, WeakEntity};
use icarus_client::Client;
use project::Project;
use workspace::Workspace;

use crate::{AppConnection, app_client, servers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerRef {
    /// The server the app's own connection talks to (this machine unless the user picked another).
    Primary,
    /// A server saved with `icarus remote add`, by name.
    Saved(String),
}

impl ServerRef {
    pub fn saved_name(&self) -> Option<&str> {
        match self {
            ServerRef::Primary => None,
            ServerRef::Saved(name) => Some(name),
        }
    }
}

fn primary_paths(cx: &App) -> Vec<std::path::PathBuf> {
    cx.try_global::<AppConnection>()
        .map(|connection| connection.workspace_paths.clone())
        .unwrap_or_default()
}

/// The server whose workspace holds `path`. The primary server wins when both do (the same folder
/// layout on two machines), so a saved server only takes paths the primary doesn't know.
pub fn server_for_path(path: &Path, cx: &App) -> Option<ServerRef> {
    if primary_paths(cx).iter().any(|root| path.starts_with(root)) {
        return Some(ServerRef::Primary);
    }
    let store = servers::existing_store(cx)?;
    store
        .read(cx)
        .servers()
        .iter()
        .filter_map(|server| {
            let depth = server
                .workspace_paths()
                .into_iter()
                .filter(|root| path.starts_with(root))
                .map(|root| root.components().count())
                .max()?;
            Some((depth, server.name.clone()))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, name)| ServerRef::Saved(name))
}

/// A project opened over SSH belongs to the saved server on that host; a local one to the server
/// whose workspace holds its folder; anything else to the primary server.
pub fn server_for_project(project: &Project, cx: &App) -> ServerRef {
    if let Some(options) = project.remote_connection_options(cx) {
        let remote::RemoteConnectionOptions::Ssh(options) = options else {
            return ServerRef::Primary;
        };
        return servers::existing_store(cx)
            .and_then(|store| {
                store
                    .read(cx)
                    .servers()
                    .iter()
                    .find(|server| server.serves_ssh_host(options))
                    .map(|server| ServerRef::Saved(server.name.clone()))
            })
            .unwrap_or(ServerRef::Primary);
    }
    project
        .visible_worktrees(cx)
        .next()
        .and_then(|worktree| server_for_path(&worktree.read(cx).abs_path(), cx))
        .unwrap_or(ServerRef::Primary)
}

pub fn client_for(server: &ServerRef, cx: &App) -> Option<Arc<Client>> {
    match server {
        ServerRef::Primary => app_client(cx),
        ServerRef::Saved(name) => servers::existing_store(cx)?
            .read(cx)
            .servers()
            .iter()
            .find(|server| &server.name == name)?
            .client(),
    }
}

pub fn client_for_project(project: &Project, cx: &App) -> Option<Arc<Client>> {
    client_for(&server_for_project(project, cx), cx)
}

pub fn client_for_weak_project(project: &WeakEntity<Project>, cx: &App) -> Option<Arc<Client>> {
    client_for_project(project.upgrade()?.read(cx), cx)
}

pub fn client_for_workspace(workspace: &WeakEntity<Workspace>, cx: &App) -> Option<Arc<Client>> {
    let workspace = workspace.upgrade()?;
    client_for_project(workspace.read(cx).project().read(cx), cx)
}

/// A saved server's connection when the project's workspace lives there; `None` means the caller
/// keeps using its own connection to the primary server.
pub fn saved_client_for_project(project: &Project, cx: &App) -> Option<Arc<Client>> {
    match server_for_project(project, cx) {
        ServerRef::Primary => None,
        saved => client_for(&saved, cx),
    }
}

const PRIMARY_KEY: &str = "icarus_primary_server";

/// The saved server the app's own connection uses, remembered from last time (none: this machine).
pub fn load_primary(cx: &mut App) {
    let primary = db::kvp::KeyValueStore::global(cx)
        .read_kvp(PRIMARY_KEY)
        .ok()
        .flatten()
        .filter(|name| !name.is_empty());
    cx.default_global::<AppConnection>().primary = primary;
}

pub fn primary(cx: &App) -> Option<String> {
    cx.try_global::<AppConnection>()
        .and_then(|connection| connection.primary.clone())
}

/// Points the app's own connection (notifications, layouts, open-file requests) at `server`
/// (`None`: this machine) and reconnects.
pub fn set_primary(server: Option<String>, cx: &mut App) {
    let connection = cx.default_global::<AppConnection>();
    if connection.primary == server {
        return;
    }
    connection.primary = server.clone();
    connection.workspace_paths.clear();
    if let Some(reconnect) = connection.reconnect.take()
        && reconnect.send(()).is_err()
    {
        log::debug!("the app connection was already closed");
    }
    let store = db::kvp::KeyValueStore::global(cx);
    db::write_and_log(cx, move || async move {
        store
            .write_kvp(PRIMARY_KEY.to_string(), server.unwrap_or_default())
            .await
    });
}

/// The server running an agent session: a saved server listing it, else the primary one.
pub fn server_for_agent(agent_id: &str, cx: &App) -> ServerRef {
    servers::existing_store(cx)
        .and_then(|store| {
            store
                .read(cx)
                .servers()
                .iter()
                .find(|server| {
                    server
                        .workspaces
                        .iter()
                        .any(|workspace| workspace.agents.iter().any(|agent| agent.id == agent_id))
                })
                .map(|server| ServerRef::Saved(server.name.clone()))
        })
        .filter(|server| server.saved_name() != primary(cx).as_deref())
        .unwrap_or(ServerRef::Primary)
}

pub fn client_for_agent(agent_id: &str, cx: &App) -> Option<Arc<Client>> {
    client_for(&server_for_agent(agent_id, cx), cx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AgentSummary;

    #[gpui::test]
    async fn picking_another_server_reconnects_and_is_remembered(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let (reconnect, mut switched) = futures::channel::oneshot::channel();
        cx.update(|cx| {
            let connection = cx.default_global::<AppConnection>();
            connection.workspace_paths = vec![PathBuf::from("/w/main")];
            connection.reconnect = Some(reconnect);
            set_primary(Some("desk".into()), cx);
            assert_eq!(primary(cx).as_deref(), Some("desk"));
            assert!(
                cx.default_global::<AppConnection>()
                    .workspace_paths
                    .is_empty(),
                "the old server's folders no longer route to the app's connection"
            );
        });
        assert_eq!(
            switched.try_recv(),
            Ok(Some(())),
            "the connection is dropped"
        );
        cx.run_until_parked();
        cx.update(|cx| {
            cx.default_global::<AppConnection>().primary = None;
            load_primary(cx);
            assert_eq!(
                primary(cx).as_deref(),
                Some("desk"),
                "remembered across launches"
            );
            set_primary(None, cx);
        });
        cx.run_until_parked();
        cx.update(|cx| {
            load_primary(cx);
            assert_eq!(primary(cx), None);
        });
    }
    use crate::servers::tests::saved;
    use crate::tests::{panel_window, summary};
    use std::path::PathBuf;

    #[gpui::test]
    async fn folders_agents_and_windows_go_to_the_server_that_owns_them(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_, panel, mut cx) = panel_window(vec![summary("main", "/w/main", None)], cx).await;
        let store = panel.read_with(&cx, |panel, _| panel.remote_servers().clone());
        let mut remote = summary("remote", "/srv/ws/remote", None);
        remote.agents = vec![AgentSummary {
            id: "agent-remote".into(),
            title: None,
            state: "idle".into(),
            provider: "claude".into(),
            ..AgentSummary::default()
        }];
        let mut shared = summary("shared", "/w/main", None);
        shared.id = "ws-shared".into();
        store.update(&mut cx, |store, cx| {
            store.set_saved(&[saved("desk", "100.64.0.2:4000")], cx);
            store.set_workspaces("desk", vec![remote, shared], cx);
        });
        cx.update(|_, cx| {
            cx.default_global::<AppConnection>().workspace_paths = vec![PathBuf::from("/w/main")];
            assert_eq!(
                server_for_path(Path::new("/srv/ws/remote/src/lib.rs"), cx),
                Some(ServerRef::Saved("desk".into()))
            );
            assert_eq!(
                server_for_path(Path::new("/w/main/README.md"), cx),
                Some(ServerRef::Primary),
                "the primary server wins a folder both servers list"
            );
            assert_eq!(server_for_path(Path::new("/tmp/other"), cx), None);
            assert_eq!(
                server_for_agent("agent-remote", cx),
                ServerRef::Saved("desk".into())
            );
            assert_eq!(server_for_agent("agent-local", cx), ServerRef::Primary);

            cx.default_global::<AppConnection>().primary = Some("desk".into());
            assert_eq!(
                server_for_agent("agent-remote", cx),
                ServerRef::Primary,
                "the picked primary server is reached through the app's own connection"
            );
        });
    }
}
