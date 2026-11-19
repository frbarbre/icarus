use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    App, Context, Entity, EntityId, Global, Render, Subscription, Task, WeakEntity, Window,
};
use gpui_tokio::Tokio;
use icarus_client::Client;
use project::Project;
use serde::Deserialize;
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;

use crate::RunNextAction;

/// Only local git is read this often; GitHub's side arrives as `workspace.next_action` events.
const LOCAL_GIT_REFRESH: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NextAction {
    pub kind: String,
    pub label: String,
    #[serde(default)]
    pub agent_step: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct NextActionReply {
    action: Option<NextAction>,
}

/// Each project's button: a window holds one title bar per workspace, so the keybinding runs the
/// step of the workspace it was pressed in.
#[derive(Default)]
struct NextActionButtons(HashMap<EntityId, WeakEntity<NextActionButton>>);

impl Global for NextActionButtons {}

/// The title bar's "next step" button for the Icarus workspace shown in this window.
pub struct NextActionButton {
    client: Option<Arc<Client>>,
    workspace_id: Option<String>,
    action: Option<NextAction>,
    running: bool,
    wake: Option<mpsc::UnboundedSender<()>>,
    _poll: Task<()>,
    _activation: Option<Subscription>,
}

fn project_paths(project: &WeakEntity<Project>, cx: &App) -> Vec<PathBuf> {
    project
        .upgrade()
        .map(|project| {
            project
                .read(cx)
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect()
        })
        .unwrap_or_default()
}

async fn lookup(
    client: Arc<Client>,
    paths: Vec<PathBuf>,
    selected: bool,
) -> Result<(Option<String>, Option<NextAction>)> {
    let workspaces = client
        .request("workspace.list", serde_json::json!({}))
        .await?;
    let workspace_id = workspaces
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|workspace| {
            let path = PathBuf::from(workspace["path"].as_str()?);
            if paths.contains(&path) {
                workspace["id"].as_str().map(str::to_string)
            } else {
                None
            }
        });
    let Some(id) = workspace_id.clone() else {
        return Ok((None, None));
    };
    let reply = client
        .request(
            "workspace.next_action",
            serde_json::json!({ "id": id, "selected": selected }),
        )
        .await?;
    let reply: NextActionReply = serde_json::from_value(reply)?;
    Ok((workspace_id, reply.action))
}

impl NextActionButton {
    pub fn new(project: Entity<Project>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let handle = window.window_handle();
        let weak_self = cx.entity().downgrade();
        let buttons = &mut cx.default_global::<NextActionButtons>().0;
        buttons.retain(|_, button| button.upgrade().is_some());
        buttons.insert(project.entity_id(), weak_self);
        if !crate::enabled(cx) {
            return Self::idle(None);
        }
        let project = project.downgrade();
        let (wake, mut woken) = mpsc::unbounded();
        // Becoming the window in front is Orca's worktree activation: the server refreshes a stale PR.
        let activation = cx.observe_window_activation(window, |button, window, _| {
            if window.is_window_active() {
                button.wake();
            }
        });
        let poll = cx.spawn(async move |this, cx| {
            loop {
                if this.upgrade().is_none() {
                    return;
                }
                let client = cx.update(|cx| crate::routing::client_for_weak_project(&project, cx));
                if let Some(client) = client {
                    let paths = cx.update(|cx| project_paths(&project, cx));
                    let selected = cx
                        .update_window(handle, |_, window, _| window.is_window_active())
                        .unwrap_or(false);
                    let result =
                        Tokio::spawn_result(cx, lookup(client.clone(), paths, selected)).await;
                    let updated = this.update(cx, |button, cx| match result {
                        Ok((workspace_id, action)) => {
                            button.client = Some(client);
                            button.set_state(workspace_id, action, cx);
                        }
                        Err(_) => {
                            button.client = None;
                            if button.action.take().is_some() {
                                cx.notify();
                            }
                        }
                    });
                    if updated.is_err() {
                        return;
                    }
                }
                let timer = cx.background_executor().timer(LOCAL_GIT_REFRESH);
                futures::future::select(timer, woken.next()).await;
            }
        });
        Self {
            wake: Some(wake),
            _poll: poll,
            _activation: Some(activation),
            ..Self::idle(None)
        }
    }

    fn idle(action: Option<NextAction>) -> Self {
        Self {
            client: None,
            workspace_id: None,
            action,
            running: false,
            wake: None,
            _poll: Task::ready(()),
            _activation: None,
        }
    }

    /// A button that never contacts a server and shows `action`, for visual tests.
    pub fn preview(action: NextAction) -> Self {
        Self::idle(Some(action))
    }

    fn wake(&self) {
        if let Some(wake) = &self.wake {
            wake.unbounded_send(()).ok();
        }
    }

    fn set_state(
        &mut self,
        workspace_id: Option<String>,
        action: Option<NextAction>,
        cx: &mut Context<Self>,
    ) {
        if self.workspace_id != workspace_id || self.action != action {
            self.workspace_id = workspace_id;
            self.action = action;
            cx.notify();
        }
    }

    /// The server's word on this workspace: a new next step is shown as is, a changed card is re-read.
    fn server_event(&mut self, topic: &str, payload: &serde_json::Value, cx: &mut Context<Self>) {
        let Some(id) = self.workspace_id.clone() else {
            return;
        };
        match topic {
            "workspace.next_action" if payload["workspace"].as_str() == Some(id.as_str()) => {
                if let Some(reply) =
                    serde_json::from_value::<NextActionReply>(payload.clone()).log_err()
                {
                    self.set_state(Some(id), reply.action, cx);
                }
            }
            "workspace.changed"
                if payload["workspaces"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|w| w.as_str() == Some(id.as_str()))) =>
            {
                self.wake();
            }
            _ => {}
        }
    }

    pub fn action(&self) -> Option<&NextAction> {
        self.action.as_ref()
    }

    /// Makes `button` the one the sidebar strip shows for `project` (previews and visual tests).
    pub fn show_for_project(button: &Entity<Self>, project: &Entity<Project>, cx: &mut App) {
        cx.default_global::<NextActionButtons>()
            .0
            .insert(project.entity_id(), button.downgrade());
    }

    pub fn for_project(project: &Entity<Project>, cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<NextActionButtons>()
            .and_then(|buttons| buttons.0.get(&project.entity_id()))
            .and_then(WeakEntity::upgrade)
    }

    /// Runs the current step; agent steps start a new agent session on the server, shown as a new agent tab.
    pub fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), Some(id)) = (self.client.clone(), self.workspace_id.clone()) else {
            return;
        };
        if self.action.is_none() || self.running {
            return;
        }
        self.running = true;
        cx.notify();
        let request = Tokio::spawn_result(cx, async move {
            client
                .request("workspace.run_next_action", serde_json::json!({ "id": id }))
                .await
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update(cx, |button, cx| {
                button.running = false;
                cx.notify();
            })?;
            let reply = result?;
            if let Some(session_id) = reply["started_agent"].as_str().map(str::to_string) {
                this.update_in(cx, |_, window, cx| {
                    if let Some(workspace) = workspace::Workspace::for_window(window, cx) {
                        workspace.update(cx, |workspace, cx| {
                            crate::open_agent_tab(Some(session_id), None, workspace, window, cx);
                        });
                    }
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }
}

/// Orca's icon per step, by the script button's id: merge steps a merge, commit & push an upload,
/// ready for review a check.
fn next_action_icon(kind: &str) -> IconName {
    let id = kind.strip_prefix("script:").unwrap_or(kind);
    match id.replace('_', "-").as_str() {
        "commit-and-push" => IconName::Upload,
        "ready-for-review" => IconName::CircleCheck,
        "create-pull-request" => IconName::PullRequest,
        _ => IconName::GitMerge,
    }
}

impl Render for NextActionButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(action) = self.action.clone() else {
            return div().into_any_element();
        };
        let icon = next_action_icon(&action.kind);
        let label = if self.running {
            format!("{}…", action.label)
        } else {
            action.label
        };
        Button::new("icarus-next-action", label)
            .style(ButtonStyle::Outlined)
            .label_size(LabelSize::Small)
            .start_icon(Icon::new(icon).size(IconSize::Small))
            .disabled(self.running)
            .tooltip(Tooltip::for_action_title(
                "Run the next step for this workspace",
                &RunNextAction,
            ))
            .on_click(cx.listener(|button, _, window, cx| button.run(window, cx)))
            .into_any_element()
    }
}

pub(crate) fn server_event(topic: &str, payload: &serde_json::Value, cx: &mut App) {
    let buttons: Vec<Entity<NextActionButton>> = cx
        .try_global::<NextActionButtons>()
        .map(|buttons| buttons.0.values().filter_map(WeakEntity::upgrade).collect())
        .unwrap_or_default();
    for button in buttons {
        button.update(cx, |button, cx| button.server_event(topic, payload, cx));
    }
}

pub(crate) fn register(cx: &mut App) {
    cx.observe_new(|workspace: &mut workspace::Workspace, _, _| {
        workspace.register_action(|workspace, _: &RunNextAction, window, cx| {
            let button = NextActionButton::for_project(workspace.project(), cx);
            if let Some(button) = button {
                button.update(cx, |button, cx| button.run(window, cx));
            }
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(kind: &str, label: &str) -> NextAction {
        NextAction {
            kind: kind.into(),
            label: label.into(),
            agent_step: kind != "merge",
        }
    }

    #[gpui::test]
    fn the_server_event_replaces_the_step_the_button_shows(cx: &mut gpui::TestAppContext) {
        let button = cx.new(|_| {
            let mut button = NextActionButton::preview(step("commit-and-push", "Commit & push"));
            button.workspace_id = Some("ws-1".into());
            button
        });
        let send = |topic: &str, payload: serde_json::Value, cx: &mut gpui::TestAppContext| {
            button.update(cx, |button, cx| button.server_event(topic, &payload, cx));
        };

        send(
            "workspace.next_action",
            json!({ "workspace": "ws-2", "action": { "kind": "merge", "label": "Merge" } }),
            cx,
        );
        button.read_with(cx, |button, _| {
            assert_eq!(
                button.action(),
                Some(&step("commit-and-push", "Commit & push"))
            );
        });

        send(
            "workspace.next_action",
            json!({
                "workspace": "ws-1",
                "action": { "kind": "merge", "label": "Merge", "agent_step": false },
                "local": { "unpushed_commits": 0 },
            }),
            cx,
        );
        button.read_with(cx, |button, _| {
            assert_eq!(button.action(), Some(&step("merge", "Merge")));
        });

        send(
            "workspace.next_action",
            json!({ "workspace": "ws-1", "action": null }),
            cx,
        );
        button.read_with(cx, |button, _| assert_eq!(button.action(), None));
    }
}
