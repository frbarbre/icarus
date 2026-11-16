//! Other Icarus servers this app watches (one client, many servers — plan §2): the ones saved with
//! `icarus remote add`, each with its own connection, listed in the Workspaces panel next to the
//! local server's workspaces.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AppContext as _, Context, Entity, Global, Task, TaskExt as _};
use gpui_tokio::Tokio;
use icarus_client::Client;
use icarus_client::servers::{SavedServer, SavedServers};
use serde_json::json;
use util::ResultExt as _;

use crate::model::{StatusColumn, WorkspaceSummary};

const REFRESH: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub enum RemoteState {
    Connecting,
    Connected { server_version: String },
    Unavailable(String),
}

#[derive(Clone)]
pub struct RemoteServer {
    pub name: String,
    pub address: String,
    pub state: RemoteState,
    pub workspaces: Vec<WorkspaceSummary>,
    pub statuses: Vec<StatusColumn>,
    saved: SavedServer,
    /// The workspace folders from the last listing, kept across launches for routing at startup.
    known_paths: Vec<PathBuf>,
    client: Option<Arc<Client>>,
    /// Forwards the server's refresh events (review, notes, git, ports) to the open views.
    _events: Option<Rc<Task<()>>>,
}

impl RemoteServer {
    fn new(saved: &SavedServer) -> Self {
        Self {
            name: saved.name.clone(),
            address: saved.address.clone(),
            state: RemoteState::Connecting,
            workspaces: Vec::new(),
            statuses: Vec::new(),
            saved: saved.clone(),
            known_paths: Vec::new(),
            client: None,
            _events: None,
        }
    }

    /// Folders of this server's workspaces: the current listing, else the last one seen.
    pub fn workspace_paths(&self) -> Vec<&Path> {
        if self.workspaces.is_empty() {
            self.known_paths.iter().map(PathBuf::as_path).collect()
        } else {
            self.workspaces
                .iter()
                .map(|workspace| workspace.path.as_path())
                .collect()
        }
    }

    pub fn client(&self) -> Option<Arc<Client>> {
        self.client.clone()
    }

    pub fn saved(&self) -> &SavedServer {
        &self.saved
    }

    /// Where this server's worktrees open over SSH, if anywhere.
    pub fn ssh_destination(&self) -> Option<String> {
        self.saved.ssh_destination()
    }

    /// Whether a project opened over SSH as `options` is on this server's machine.
    pub fn serves_ssh_host(&self, options: &remote::SshConnectionOptions) -> bool {
        let Some(destination) = self.ssh_destination() else {
            return false;
        };
        let Ok(saved) = remote::SshConnectionOptions::parse_command_line(&destination) else {
            return false;
        };
        saved.host.to_string() == options.host.to_string()
            && saved.port.is_none_or(|port| Some(port) == options.port)
            && (saved.username.is_none() || saved.username == options.username)
    }

    /// The status's label on this server, e.g. "In review" for `in-review`.
    pub fn status_label(&self, status: Option<&str>) -> String {
        let Some(status) = status else {
            return "No status".into();
        };
        self.statuses
            .iter()
            .find(|column| column.id == status)
            .map(|column| column.label.clone())
            .unwrap_or_else(|| status.to_owned())
    }
}

/// Every saved server's connection state and workspaces.
#[derive(Default)]
pub struct RemoteServers {
    servers: Vec<RemoteServer>,
    cached_paths: HashMap<String, Vec<PathBuf>>,
    _poll: Option<Task<()>>,
}

impl RemoteServers {
    pub fn servers(&self) -> &[RemoteServer] {
        &self.servers
    }

    /// Follows the saved-servers file: new entries are added, removed ones dropped, order kept.
    pub fn set_saved(&mut self, saved: &[SavedServer], cx: &mut Context<Self>) {
        let mut next = Vec::with_capacity(saved.len());
        for entry in saved {
            let existing = self
                .servers
                .iter()
                .position(|server| server.name == entry.name && server.address == entry.address);
            next.push(match existing {
                Some(index) => {
                    let mut server = self.servers[index].clone();
                    server.saved = entry.clone();
                    server
                }
                None => {
                    let mut server = RemoteServer::new(entry);
                    server.known_paths = self
                        .cached_paths
                        .get(&entry.name)
                        .cloned()
                        .unwrap_or_default();
                    server
                }
            });
        }
        self.servers = next;
        cx.notify();
    }

    pub fn set_listing(
        &mut self,
        name: &str,
        listing: anyhow::Result<(Arc<Client>, Vec<WorkspaceSummary>, Vec<StatusColumn>)>,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = self.servers.iter_mut().find(|server| server.name == name) else {
            return;
        };
        match listing {
            Ok((client, workspaces, statuses)) => {
                server.state = RemoteState::Connected {
                    server_version: client.server_version.clone(),
                };
                server.client = Some(client);
                let new_folder = workspaces.iter().any(|workspace| {
                    !server
                        .workspaces
                        .iter()
                        .any(|known| known.path == workspace.path)
                });
                server.workspaces = workspaces;
                server.statuses = statuses;
                let paths: Vec<PathBuf> = server
                    .workspaces
                    .iter()
                    .map(|workspace| workspace.path.clone())
                    .collect();
                let paths_changed = server.known_paths != paths;
                server.known_paths = paths.clone();
                if paths_changed && crate::enabled(cx) {
                    self.cached_paths.insert(name.to_owned(), paths);
                    save_cached_paths(&self.cached_paths, cx);
                }
                if new_folder && crate::enabled(cx) {
                    // Deferred: trusting reads this store, which is being updated right now.
                    cx.defer(crate::worktree_trust::recheck_open_windows);
                }
            }
            Err(error) => {
                server.state = RemoteState::Unavailable(format!("{error:#}"));
                server.client = None;
                server._events = None;
            }
        }
        cx.notify();
    }

    /// A server's workspaces without a connection, for tests and previews.
    pub fn set_workspaces(
        &mut self,
        name: &str,
        workspaces: Vec<WorkspaceSummary>,
        cx: &mut Context<Self>,
    ) {
        if let Some(server) = self.servers.iter_mut().find(|server| server.name == name) {
            server.workspaces = workspaces;
            cx.notify();
        }
    }

    /// Moves one of a server's workspaces to `status` on that server, then refreshes its listing.
    pub fn set_status(
        &mut self,
        name: &str,
        workspace_id: String,
        status: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = self.servers.iter_mut().find(|server| server.name == name) else {
            return;
        };
        if let Some(workspace) = server
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
        {
            workspace.status = status.clone();
        }
        cx.notify();
        let Some(client) = server.client.clone() else {
            return;
        };
        let name = name.to_owned();
        let set = Tokio::spawn_result(cx, async move {
            client
                .request(
                    "workspace.set",
                    json!({ "id": workspace_id, "status": status }),
                )
                .await?;
            fetch(client).await
        });
        cx.spawn(async move |this, cx| {
            let listing = set.await;
            this.update(cx, |this, cx| match listing {
                Ok((workspaces, statuses)) => {
                    if let Some(server) = this.servers.iter_mut().find(|s| s.name == name) {
                        server.workspaces = workspaces;
                        server.statuses = statuses;
                        cx.notify();
                    }
                }
                Err(error) => log::error!("moving a workspace on {name} failed: {error:#}"),
            })
        })
        .detach_and_log_err(cx);
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        self._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                let saved = SavedServers::load().log_err().unwrap_or_default().servers;
                if this
                    .update(cx, |this, cx| this.set_saved(&saved, cx))
                    .is_err()
                {
                    return;
                }
                for entry in saved {
                    let client = this
                        .read_with(cx, |this, _| {
                            this.servers
                                .iter()
                                .find(|server| server.name == entry.name)
                                .and_then(|server| server.client.clone())
                        })
                        .ok()
                        .flatten();
                    let name = entry.name.clone();
                    let listing = Tokio::spawn_result(cx, async move {
                        let (client, events) = match client {
                            Some(client) => (client, None),
                            None => {
                                let (client, events) =
                                    Client::connect_saved(&entry, "icarus-panel").await?;
                                (client, Some(events))
                            }
                        };
                        let (workspaces, statuses) = fetch(client.clone()).await?;
                        anyhow::Ok(((client, workspaces, statuses), events))
                    })
                    .await;
                    let (listing, events) = match listing {
                        Ok((listing, events)) => (Ok(listing), events),
                        Err(error) => (Err(error), None),
                    };
                    let updated = this.update(cx, |this, cx| {
                        this.set_listing(&name, listing, cx);
                        let Some(mut events) = events else {
                            return;
                        };
                        let forward = cx.spawn(async move |_, cx| {
                            loop {
                                let next = Tokio::spawn(cx, async move {
                                    let event = events.recv().await;
                                    (events, event)
                                })
                                .await;
                                let Ok((returned, Some(event))) = next else {
                                    return;
                                };
                                events = returned;
                                cx.update(|cx| {
                                    crate::handle_view_event(&event.topic, event.payload, cx)
                                });
                            }
                        });
                        if let Some(server) = this.servers.iter_mut().find(|s| s.name == name) {
                            server._events = Some(Rc::new(forward));
                        }
                    });
                    if updated.is_err() {
                        return;
                    }
                }
                cx.background_executor().timer(REFRESH).await;
            }
        }));
    }
}

async fn fetch(client: Arc<Client>) -> anyhow::Result<(Vec<WorkspaceSummary>, Vec<StatusColumn>)> {
    let workspaces = client.request("workspace.list", json!({})).await?;
    let statuses = client
        .request("config.get", json!({ "key": "workspace.statuses" }))
        .await
        .unwrap_or_default();
    Ok((
        serde_json::from_value(workspaces)?,
        serde_json::from_value(statuses).unwrap_or_default(),
    ))
}

struct GlobalRemoteServers(Entity<RemoteServers>);

impl Global for GlobalRemoteServers {}

const CACHED_PATHS_KEY: &str = "icarus_saved_server_folders";

fn load_cached_paths(cx: &App) -> HashMap<String, Vec<PathBuf>> {
    db::kvp::KeyValueStore::global(cx)
        .read_kvp(CACHED_PATHS_KEY)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).log_err())
        .unwrap_or_default()
}

fn save_cached_paths(paths: &HashMap<String, Vec<PathBuf>>, cx: &App) {
    let Some(json) = serde_json::to_string(paths).log_err() else {
        return;
    };
    let store = db::kvp::KeyValueStore::global(cx);
    db::write_and_log(cx, move || async move {
        store.write_kvp(CACHED_PATHS_KEY.to_string(), json).await
    });
}

/// The store if the app created it; routing never starts watching on its own.
pub fn existing_store(cx: &App) -> Option<Entity<RemoteServers>> {
    cx.try_global::<GlobalRemoteServers>()
        .map(|store| store.0.clone())
}

/// The app-wide store; it starts watching saved servers the first time a real panel asks for it.
pub fn store(cx: &mut App, watch: bool) -> Entity<RemoteServers> {
    let store = match cx.try_global::<GlobalRemoteServers>() {
        Some(store) => store.0.clone(),
        None => {
            let store = cx.new(|_| RemoteServers::default());
            cx.set_global(GlobalRemoteServers(store.clone()));
            if crate::enabled(cx) {
                // Routing needs the saved servers' folders before the first listing arrives, or the
                // first agent tab of a saved server's workspace would go to this machine's server.
                let cached = load_cached_paths(cx);
                let saved = SavedServers::load().log_err().unwrap_or_default().servers;
                store.update(cx, |store, cx| {
                    store.cached_paths = cached;
                    store.set_saved(&saved, cx);
                });
            }
            store
        }
    };
    if watch {
        store.update(cx, |store, cx| {
            if store._poll.is_none() {
                store.start(cx);
            }
        });
    }
    store
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tests::{panel_window, summary};

    pub(crate) fn saved(name: &str, address: &str) -> SavedServer {
        SavedServer {
            name: name.into(),
            address: address.into(),
            token: None,
            fingerprint: None,
            client_id: None,
            command: None,
            ssh: None,
        }
    }

    #[gpui::test]
    async fn saved_servers_show_in_the_panel_with_their_workspaces(cx: &mut gpui::TestAppContext) {
        let (_, panel, mut cx) = panel_window(vec![summary("main", "/w/main", None)], cx).await;
        let store = panel.read_with(&cx, |panel, _| panel.remote_servers().clone());
        store.update(&mut cx, |store, cx| {
            store.set_saved(
                &[
                    saved("studio", "100.64.0.2:4000"),
                    saved("box", "ssh://box"),
                ],
                cx,
            )
        });
        store.read_with(&cx, |store, _| {
            let names: Vec<_> = store.servers().iter().map(|s| s.name.clone()).collect();
            assert_eq!(names, ["studio", "box"]);
            assert!(
                store
                    .servers()
                    .iter()
                    .all(|server| server.state == RemoteState::Connecting)
            );
        });

        store.update(&mut cx, |store, cx| {
            store.set_listing("box", Err(anyhow::anyhow!("connection refused")), cx);
        });
        store.read_with(&cx, |store, _| {
            assert_eq!(
                store.servers()[1].state,
                RemoteState::Unavailable("connection refused".into())
            );
        });

        // Removing a server from the saved list drops it; the other keeps its state.
        store.update(&mut cx, |store, cx| {
            store.set_saved(&[saved("box", "ssh://box")], cx)
        });
        store.read_with(&cx, |store, _| {
            assert_eq!(store.servers().len(), 1);
            assert!(matches!(
                store.servers()[0].state,
                RemoteState::Unavailable(_)
            ));
        });
    }

    #[gpui::test]
    fn folders_from_the_last_launch_route_before_the_first_listing(cx: &mut gpui::TestAppContext) {
        let store = cx.new(|_| RemoteServers::default());
        store.update(cx, |store, cx| {
            store
                .cached_paths
                .insert("desk".into(), vec![PathBuf::from("/srv/ws/remote")]);
            store.set_saved(&[saved("desk", "100.64.0.2:4000")], cx);
            assert_eq!(
                store.servers()[0].workspace_paths(),
                [Path::new("/srv/ws/remote")],
                "a fresh launch knows the folders before the server answers"
            );
            store.set_workspaces("desk", vec![summary("other", "/srv/ws/other", None)], cx);
            assert_eq!(
                store.servers()[0].workspace_paths(),
                [Path::new("/srv/ws/other")]
            );
        });
    }

    #[test]
    fn status_labels_come_from_that_servers_columns() {
        let mut server = RemoteServer::new(&saved("studio", "100.64.0.2:4000"));
        server.statuses = vec![StatusColumn {
            id: "in-review".into(),
            label: "In review".into(),
            color: None,
            icon: None,
        }];
        assert_eq!(server.status_label(Some("in-review")), "In review");
        assert_eq!(server.status_label(Some("custom")), "custom");
        assert_eq!(server.status_label(None), "No status");
    }
}
