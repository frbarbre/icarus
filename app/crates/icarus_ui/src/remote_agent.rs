//! Agent tabs for workspaces on a saved server: `icarus --server <name> acp`, so the session runs on
//! that server. Projects opened over SSH run `icarus acp` on that host, installing it first if asked.

use std::rc::Rc;

use acp_thread::AgentConnection;
use agent_servers::{AgentServer, AgentServerDelegate};
use anyhow::Result;
use gpui::{App, Entity, Task};
use project::agent_server_store::AgentServerCommand;
use project::{AgentId, Project};
use remote::RemoteConnectionOptions;

use crate::routing::{self, ServerRef};
use crate::terminals::icarus_program;

/// The agent id Icarus registers in the default settings (`agent_servers.Icarus`).
pub(crate) const ICARUS_AGENT: &str = "Icarus";

pub struct IcarusOnServer {
    server: String,
}

impl IcarusOnServer {
    pub fn new(server: String) -> Self {
        Self { server }
    }

    pub fn command(&self) -> AgentServerCommand {
        AgentServerCommand {
            path: icarus_program().into(),
            args: vec!["--server".into(), self.server.clone(), "acp".into()],
            env: None,
        }
    }
}

impl AgentServer for IcarusOnServer {
    fn logo(&self) -> ui::IconName {
        ui::IconName::Terminal
    }

    fn agent_id(&self) -> AgentId {
        AgentId::new(ICARUS_AGENT)
    }

    fn connect(
        &self,
        _delegate: AgentServerDelegate,
        project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let command = self.command();
        let store = project.read(cx).agent_server_store().downgrade();
        let agent_id = self.agent_id();
        cx.spawn(async move |cx| {
            agent_servers::connect_acp(
                agent_id,
                project,
                command,
                store,
                None,
                Default::default(),
                cx,
            )
            .await
        })
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn std::any::Any> {
        self
    }
}

/// The agent server for a new tab in `project`: the saved server that owns a local folder's
/// workspace; `icarus acp` on the host (installed on request) for projects opened over SSH; `None`
/// (the configured `icarus acp`) for this machine's workspaces and other remote kinds.
pub fn agent_server_for_project(
    project: &Entity<Project>,
    cx: &App,
) -> Option<Rc<dyn AgentServer>> {
    if !crate::enabled(cx) {
        return None;
    }
    let project = project.read(cx);
    if let Some(options) = project.remote_connection_options(cx) {
        return match options {
            RemoteConnectionOptions::Ssh(_) => Some(Rc::new(
                crate::ssh_install::IcarusOverSsh::new(options.display_name()),
            )),
            _ => None,
        };
    }
    match routing::server_for_project(project, cx) {
        ServerRef::Primary => routing::primary(cx)
            .map(|name| Rc::new(IcarusOnServer::new(name)) as Rc<dyn AgentServer>),
        ServerRef::Saved(name) => Some(Rc::new(IcarusOnServer::new(name))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::servers::tests::saved;
    use crate::tests::{panel_window, summary};
    use std::path::PathBuf;

    #[test]
    fn agent_tabs_for_a_saved_server_run_icarus_acp_against_it() {
        let server = IcarusOnServer::new("desk".into());
        assert_eq!(server.agent_id(), AgentId::new(ICARUS_AGENT));
        let command = server.command();
        assert_eq!(command.args, ["--server", "desk", "acp"]);
        assert!(command.path.ends_with("icarus"));
    }

    /// The test window's project is the folder `/project`.
    #[gpui::test]
    async fn a_window_on_a_saved_servers_folder_gets_that_servers_agent(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, panel, mut cx) =
            panel_window(vec![summary("main", "/w/main", None)], cx).await;
        let store = panel.read_with(&cx, |panel, _| panel.remote_servers().clone());
        let project = workspace.read_with(&cx, |workspace, _| workspace.project().clone());
        cx.update(|_, cx| {
            cx.set_global(crate::IcarusEnabled);
            cx.default_global::<crate::AppConnection>().workspace_paths =
                vec![PathBuf::from("/w/main")];
            assert!(
                agent_server_for_project(&project, cx).is_none(),
                "a folder no server lists keeps the configured agent"
            );
        });
        store.update(&mut cx, |store, cx| {
            store.set_saved(&[saved("desk", "100.64.0.2:4000")], cx);
            store.set_workspaces("desk", vec![summary("remote", "/project", None)], cx);
        });
        cx.update(|_, cx| {
            assert_eq!(
                routing::server_for_project(project.read(cx), cx),
                ServerRef::Saved("desk".into())
            );
            let server = agent_server_for_project(&project, cx).expect("desk's agent");
            let server = server
                .into_any()
                .downcast::<IcarusOnServer>()
                .ok()
                .expect("an Icarus agent on a saved server");
            assert_eq!(server.command().args, ["--server", "desk", "acp"]);
        });
    }
}
