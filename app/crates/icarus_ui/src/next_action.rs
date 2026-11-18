use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use gpui::{AnyWindowHandle, App, Context, Entity, Global, Render, Task, WeakEntity, Window};
use gpui_tokio::Tokio;
use icarus_client::Client;
use project::Project;
use serde::Deserialize;
use ui::{Tooltip, prelude::*};

use crate::RunNextAction;

const NEXT_ACTION_REFRESH: Duration = Duration::from_secs(3);

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

/// Each window's button, so the keybinding runs the step of the window it was pressed in.
#[derive(Default)]
struct NextActionButtons(HashMap<AnyWindowHandle, WeakEntity<NextActionButton>>);

impl Global for NextActionButtons {}

/// The title bar's "next step" button for the Icarus workspace shown in this window.
pub struct NextActionButton {
    client: Option<Arc<Client>>,
    workspace_id: Option<String>,
    action: Option<NextAction>,
    running: bool,
    _poll: Task<()>,
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
        .request("workspace.next_action", serde_json::json!({ "id": id }))
        .await?;
    let reply: NextActionReply = serde_json::from_value(reply)?;
    Ok((workspace_id, reply.action))
}

impl NextActionButton {
    pub fn new(project: Entity<Project>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let handle = window.window_handle();
        let weak_self = cx.entity().downgrade();
        cx.default_global::<NextActionButtons>()
            .0
            .insert(handle, weak_self);
        if !crate::enabled(cx) {
            return Self {
                client: None,
                workspace_id: None,
                action: None,
                running: false,
                _poll: Task::ready(()),
            };
        }
        let project = project.downgrade();
        let poll = cx.spawn(async move |this, cx| {
            loop {
                if this.upgrade().is_none() {
                    return;
                }
                let client = cx.update(|cx| crate::routing::client_for_weak_project(&project, cx));
                if let Some(client) = client {
                    let paths = cx.update(|cx| project_paths(&project, cx));
                    let result = Tokio::spawn_result(cx, lookup(client.clone(), paths)).await;
                    let updated = this.update(cx, |button, cx| match result {
                        Ok((workspace_id, action)) => {
                            button.client = Some(client);
                            if button.workspace_id != workspace_id || button.action != action {
                                button.workspace_id = workspace_id;
                                button.action = action;
                                cx.notify();
                            }
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
                cx.background_executor().timer(NEXT_ACTION_REFRESH).await;
            }
        });
        Self {
            client: None,
            workspace_id: None,
            action: None,
            running: false,
            _poll: poll,
        }
    }

    /// A button that never contacts a server and shows `action`, for visual tests.
    pub fn preview(action: NextAction) -> Self {
        Self {
            client: None,
            workspace_id: None,
            action: Some(action),
            running: false,
            _poll: Task::ready(()),
        }
    }

    pub fn action(&self) -> Option<&NextAction> {
        self.action.as_ref()
    }

    /// Makes `button` the one the sidebar strip shows in `window` (previews and visual tests).
    pub fn show_in_window(button: &Entity<Self>, window: &Window, cx: &mut App) {
        cx.default_global::<NextActionButtons>()
            .0
            .insert(window.window_handle(), button.downgrade());
    }

    pub fn for_window(window: &Window, cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<NextActionButtons>()
            .and_then(|buttons| buttons.0.get(&window.window_handle()))
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

/// Orca's icon per step: merge steps a merge, commit & push an upload, ready for review a check.
fn next_action_icon(kind: &str) -> IconName {
    match kind.replace('_', "-").as_str() {
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

pub(crate) fn register(cx: &mut App) {
    cx.observe_new(|workspace: &mut workspace::Workspace, _, _| {
        workspace.register_action(|_workspace, _: &RunNextAction, window, cx| {
            let handle = window.window_handle();
            let button = cx
                .try_global::<NextActionButtons>()
                .and_then(|buttons| buttons.0.get(&handle).and_then(WeakEntity::upgrade));
            if let Some(button) = button {
                button.update(cx, |button, cx| button.run(window, cx));
            }
        });
    })
    .detach();
}
