//! Other Icarus servers this app watches (one client, many servers — plan §2): the ones saved with
//! `icarus remote add`, each with its own connection, listed in the Workspaces panel next to the
//! local server's workspaces.

use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AppContext as _, Context, Entity, Global, Task};
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
    client: Option<Arc<Client>>,
}

impl RemoteServer {
    fn new(saved: &SavedServer) -> Self {
        Self {
            name: saved.name.clone(),
            address: saved.address.clone(),
            state: RemoteState::Connecting,
            workspaces: Vec::new(),
            statuses: Vec::new(),
            client: None,
        }
    }

    pub fn client(&self) -> Option<Arc<Client>> {
        self.client.clone()
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
                Some(index) => self.servers[index].clone(),
                None => RemoteServer::new(entry),
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
                server.workspaces = workspaces;
                server.statuses = statuses;
            }
            Err(error) => {
                server.state = RemoteState::Unavailable(format!("{error:#}"));
                server.client = None;
            }
        }
        cx.notify();
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
                        let client = match client {
                            Some(client) => client,
                            None => Client::connect_saved(&entry, "icarus-panel").await?.0,
                        };
                        let (workspaces, statuses) = fetch(client.clone()).await?;
                        anyhow::Ok((client, workspaces, statuses))
                    })
                    .await;
                    if this
                        .update(cx, |this, cx| this.set_listing(&name, listing, cx))
                        .is_err()
                    {
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

/// The app-wide store; it starts watching saved servers the first time a real panel asks for it.
pub fn store(cx: &mut App, watch: bool) -> Entity<RemoteServers> {
    let store = match cx.try_global::<GlobalRemoteServers>() {
        Some(store) => store.0.clone(),
        None => {
            let store = cx.new(|_| RemoteServers::default());
            cx.set_global(GlobalRemoteServers(store.clone()));
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
mod tests {
    use super::*;
    use crate::tests::{panel_window, summary};

    fn saved(name: &str, address: &str) -> SavedServer {
        SavedServer {
            name: name.into(),
            address: address.into(),
            token: None,
            fingerprint: None,
            client_id: None,
            command: None,
        }
    }

    #[gpui::test]
    async fn saved_servers_show_in_the_panel_with_their_workspaces(cx: &mut gpui::TestAppContext) {
        let (_, panel, mut cx) = panel_window(vec![summary("main", "/w/main", None)], cx).await;
        let store = panel.read_with(&cx, |panel, _| panel.remote_servers().clone());
        store.update(&mut cx, |store, cx| {
            store.set_saved(
                &[
                    saved("athena", "100.64.0.2:4000"),
                    saved("box", "ssh://box"),
                ],
                cx,
            )
        });
        store.read_with(&cx, |store, _| {
            let names: Vec<_> = store.servers().iter().map(|s| s.name.clone()).collect();
            assert_eq!(names, ["athena", "box"]);
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

    #[test]
    fn status_labels_come_from_that_servers_columns() {
        let mut server = RemoteServer::new(&saved("athena", "100.64.0.2:4000"));
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
