//! Remote Control for the active agent tab: a status-bar toggle that restarts the agent's Claude Code
//! with `--remote-control` (server side, `agent.remote_control`) and offers the claude.ai link once
//! Claude Code reports it.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use gpui::{
    App, AppContext as _, ClipboardItem, Context, Entity, Global, IntoElement, Render, Task,
    Window, actions,
};
use gpui_tokio::Tokio;
use serde::Deserialize;
use serde_json::json;
use ui::{ContextMenu, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace};

use crate::{app_client, enabled};

actions!(
    icarus,
    [
        /// Turns Claude Code's Remote Control on or off for the active agent tab.
        ToggleRemoteControl,
    ]
);

/// While the link hasn't appeared yet, ask again this often.
const WAITING_FOR_LINK: Duration = Duration::from_secs(2);
const IDLE_REFRESH: Duration = Duration::from_secs(30);

type AgentResolver = Arc<dyn Fn(&dyn ItemHandle, &App) -> Option<String>>;

struct GlobalAgentResolver(AgentResolver);

impl Global for GlobalAgentResolver {}

/// The app crate owns agent tabs; it tells this crate which server agent an item shows.
pub fn set_agent_resolver(
    cx: &mut App,
    resolver: impl Fn(&dyn ItemHandle, &App) -> Option<String> + 'static,
) {
    cx.set_global(GlobalAgentResolver(Arc::new(resolver)));
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct RemoteControlState {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub remote_control: bool,
    #[serde(default)]
    pub remote_control_url: Option<String>,
}

#[derive(Clone)]
pub struct RemoteControlSource {
    pub info: Rc<dyn Fn(String, &mut App) -> Task<Result<RemoteControlState>>>,
    pub set: Rc<dyn Fn(String, bool, &mut App) -> Task<Result<()>>>,
}

impl RemoteControlSource {
    pub fn server() -> Self {
        Self {
            info: Rc::new(|id, cx| {
                let Some(client) = app_client(cx) else {
                    return Task::ready(Err(anyhow::anyhow!("not connected to the Icarus server")));
                };
                Tokio::spawn_result(cx, async move {
                    let value = client.request("agent.info", json!({ "id": id })).await?;
                    Ok(serde_json::from_value(value)?)
                })
            }),
            set: Rc::new(|id, enabled, cx| {
                let Some(client) = app_client(cx) else {
                    return Task::ready(Err(anyhow::anyhow!("not connected to the Icarus server")));
                };
                Tokio::spawn_result(cx, async move {
                    client
                        .request(
                            "agent.remote_control",
                            json!({ "id": id, "enabled": enabled }),
                        )
                        .await?;
                    Ok(())
                })
            }),
        }
    }
}

pub struct RemoteControlButton {
    source: RemoteControlSource,
    agent_id: Option<String>,
    state: Option<RemoteControlState>,
    busy: bool,
    error: Option<String>,
    _poll: Task<()>,
}

impl RemoteControlButton {
    pub fn new(source: RemoteControlSource) -> Self {
        Self {
            source,
            agent_id: None,
            state: None,
            busy: false,
            error: None,
            _poll: Task::ready(()),
        }
    }

    pub fn state(&self) -> Option<&RemoteControlState> {
        self.state.as_ref()
    }

    pub fn agent_id(&self) -> Option<&str> {
        self.agent_id.as_deref()
    }

    /// Follows `agent_id` (the active agent tab's server agent, if any) and keeps its state current.
    pub fn set_agent(&mut self, agent_id: Option<String>, cx: &mut Context<Self>) {
        if self.agent_id == agent_id {
            return;
        }
        self.agent_id = agent_id;
        self.state = None;
        self.error = None;
        self._poll = match self.agent_id.clone() {
            Some(id) => cx.spawn(async move |this, cx| {
                loop {
                    let Ok(info) = this.update(cx, |this, cx| (this.source.info)(id.clone(), cx))
                    else {
                        return;
                    };
                    let info = info.await;
                    let Ok(waiting) = this.update(cx, |this, cx| {
                        match info {
                            Ok(state) => this.state = Some(state),
                            Err(error) => this.error = Some(format!("{error:#}")),
                        }
                        cx.notify();
                        this.state.as_ref().is_some_and(|state| {
                            state.remote_control && state.remote_control_url.is_none()
                        })
                    }) else {
                        return;
                    };
                    let delay = if waiting {
                        WAITING_FOR_LINK
                    } else {
                        IDLE_REFRESH
                    };
                    cx.background_executor().timer(delay).await;
                }
            }),
            None => Task::ready(()),
        };
        cx.notify();
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        let (Some(id), Some(state)) = (self.agent_id.clone(), self.state.clone()) else {
            return;
        };
        let enable = !state.remote_control;
        self.busy = true;
        self.error = None;
        cx.notify();
        let set = (self.source.set)(id, enable, cx);
        cx.spawn(async move |this, cx| {
            let result = set.await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(()) => {
                        if let Some(state) = &mut this.state {
                            state.remote_control = enable;
                            if !enable {
                                state.remote_control_url = None;
                            }
                        }
                        // Re-follow so the poll asks for the link quickly.
                        let agent = this.agent_id.take();
                        this.set_agent(agent, cx);
                    }
                    Err(error) => this.error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn menu(&self, window: &mut Window, cx: &mut Context<Self>) -> Entity<ContextMenu> {
        let state = self.state.clone().unwrap_or_default();
        let this = cx.entity().downgrade();
        ContextMenu::build(window, cx, move |mut menu, _, _| {
            if let Some(url) = state.remote_control_url.clone() {
                let open = url.clone();
                menu = menu
                    .entry("Open in claude.ai", None, move |_, cx| cx.open_url(&open))
                    .entry("Copy link", None, move |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                    })
                    .separator();
            } else if state.remote_control {
                menu = menu.label("Waiting for Claude Code's link…").separator();
            }
            let label = if state.remote_control {
                "Turn Remote Control off"
            } else {
                "Turn Remote Control on"
            };
            menu.entry(label, None, move |_, cx| {
                this.update(cx, |this, cx| this.toggle(cx)).log_err();
            })
        })
    }
}

impl Render for RemoteControlButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(state) = self.state.clone() else {
            return div().into_any_element();
        };
        // Remote Control is a Claude Code feature.
        if state.provider != "claude" {
            return div().into_any_element();
        }
        let (label, color) = match (&state.remote_control, &state.remote_control_url) {
            (true, Some(_)) => ("Remote", Color::Accent),
            (true, None) => ("Remote…", Color::Muted),
            (false, _) => ("Remote off", Color::Muted),
        };
        let tooltip = match (&self.error, &state.remote_control_url, state.remote_control) {
            (Some(error), _, _) => format!("Remote Control: {error}"),
            (None, Some(url), _) => format!("Remote Control is on: {url}"),
            (None, None, true) => "Remote Control is starting".to_owned(),
            (None, None, false) => {
                "Continue this agent from claude.ai or the Claude app".to_owned()
            }
        };
        let this = cx.entity();
        PopoverMenu::new("icarus-remote-control")
            .menu(move |window, cx| Some(this.update(cx, |this, cx| this.menu(window, cx))))
            .anchor(gpui::Anchor::BottomRight)
            .trigger_with_tooltip(
                Button::new("icarus-remote-control-button", label)
                    .label_size(LabelSize::Small)
                    .color(color)
                    .disabled(self.busy),
                Tooltip::text(tooltip),
            )
            .into_any_element()
    }
}

impl StatusItemView for RemoteControlButton {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let agent = active_pane_item.and_then(|item| {
            cx.try_global::<GlobalAgentResolver>()
                .and_then(|resolver| (resolver.0)(item, cx))
        });
        self.set_agent(agent, cx);
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}

pub(crate) fn register(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        if !enabled(cx) {
            return;
        }
        let button = cx.new(|_| RemoteControlButton::new(RemoteControlSource::server()));
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_right_item(button.clone(), window, cx);
        });
        workspace.register_action(move |_, _: &ToggleRemoteControl, _, cx| {
            button.update(cx, |button, cx| button.toggle(cx));
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[gpui::test]
    async fn the_toggle_turns_remote_control_on_and_waits_for_the_link(
        cx: &mut gpui::TestAppContext,
    ) {
        let server: Rc<RefCell<RemoteControlState>> = Rc::new(RefCell::new(RemoteControlState {
            provider: "claude".into(),
            remote_control: false,
            remote_control_url: None,
        }));
        let calls: Rc<RefCell<Vec<(String, bool)>>> = Rc::default();
        let source = RemoteControlSource {
            info: Rc::new({
                let server = server.clone();
                move |_, _| Task::ready(Ok(server.borrow().clone()))
            }),
            set: Rc::new({
                let server = server.clone();
                let calls = calls.clone();
                move |id, enabled, _| {
                    calls.borrow_mut().push((id, enabled));
                    server.borrow_mut().remote_control = enabled;
                    Task::ready(Ok(()))
                }
            }),
        };
        let button = cx.new(|_| RemoteControlButton::new(source));
        button.update(cx, |button, cx| {
            button.set_agent(Some("agent-1".into()), cx)
        });
        cx.run_until_parked();
        button.read_with(cx, |button, _| {
            assert_eq!(button.state().map(|s| s.remote_control), Some(false));
        });

        button.update(cx, |button, cx| button.toggle(cx));
        cx.run_until_parked();
        assert_eq!(calls.borrow().as_slice(), [("agent-1".to_owned(), true)]);
        button.read_with(cx, |button, _| {
            let state = button.state().expect("state");
            assert!(state.remote_control);
            assert_eq!(state.remote_control_url, None, "the link comes later");
        });

        server.borrow_mut().remote_control_url = Some("https://claude.ai/code/abc".into());
        cx.executor().advance_clock(WAITING_FOR_LINK * 2);
        cx.run_until_parked();
        button.read_with(cx, |button, _| {
            assert_eq!(
                button
                    .state()
                    .and_then(|s| s.remote_control_url.clone())
                    .as_deref(),
                Some("https://claude.ai/code/abc"),
                "polling picks the link up"
            );
        });

        button.update(cx, |button, cx| button.set_agent(None, cx));
        button.read_with(cx, |button, _| assert!(button.state().is_none()));
    }
}
