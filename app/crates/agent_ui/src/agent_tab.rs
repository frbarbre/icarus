use std::time::Duration;

use acp_thread::ThreadStatus;
use agent_client_protocol::v2::schema as acp_v2;
use anyhow::Context as _;
use db::kvp::KeyValueStore;
use gpui::{
    AnyView, App, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, SharedString, Styled, Subscription, Task, WeakEntity, Window, actions,
    div,
};
use project::Project;
use ui::{Color, Icon, IconName, Label, LabelSize, prelude::*};
use workspace::item::{Item, ItemEvent, SerializableItem};
use workspace::{ItemId, Pane, SaveIntent, Workspace, WorkspaceId};

use crate::conversation_view::ThreadView;
use crate::thread_metadata_store::ThreadId;
use crate::{AgentPanel, ConversationView};

actions!(
    icarus,
    [
        /// Opens a new agent tab and points the composer at it.
        NewAgent,
        /// Moves focus to the composer at the bottom of the workspace.
        FocusComposer,
    ]
);

/// An agent conversation shown as a center tab; its composer lives in the workspace's [`AgentComposer`].
pub struct AgentTab {
    conversation_view: Entity<ConversationView>,
    requested_agent_id: Option<String>,
    observed_thread: Option<EntityId>,
    /// What the last save said about messages, kept while a restored conversation hasn't loaded.
    saved_had_messages: bool,
    _subscriptions: Vec<Subscription>,
}

impl AgentTab {
    pub fn new(
        conversation_view: Entity<ConversationView>,
        requested_agent_id: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&conversation_view, |tab, _, cx| {
            tab.observe_root_thread(cx);
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        });
        let mut tab = Self {
            conversation_view,
            requested_agent_id,
            observed_thread: None,
            saved_had_messages: false,
            _subscriptions: vec![subscription],
        };
        tab.observe_root_thread(cx);
        tab
    }

    pub fn conversation_view(&self) -> &Entity<ConversationView> {
        &self.conversation_view
    }

    /// Puts a fresh conversation in this tab, keeping its pane and position.
    fn replace_conversation(
        &mut self,
        conversation_view: Entity<ConversationView>,
        cx: &mut Context<Self>,
    ) {
        self.conversation_view = conversation_view.clone();
        self.requested_agent_id = None;
        self.observed_thread = None;
        self._subscriptions = vec![cx.observe(&conversation_view, |tab, _, cx| {
            tab.observe_root_thread(cx);
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        })];
        self.observe_root_thread(cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// Whether the conversation has anything in it; a restore that loses an empty one loses nothing.
    fn has_messages(&self, cx: &App) -> bool {
        self.thread_view(cx)
            .is_some_and(|view| !view.read(cx).thread.read(cx).entries().is_empty())
    }

    /// The server agent behind this tab: its ACP session id, or the one it was opened for while loading.
    pub fn agent_id(&self, cx: &App) -> Option<String> {
        self.thread_view(cx)
            .map(|view| view.read(cx).thread.read(cx).session_id().0.to_string())
            .or_else(|| self.requested_agent_id.clone())
    }

    pub fn thread_view(&self, cx: &App) -> Option<Entity<ThreadView>> {
        self.conversation_view.read(cx).root_thread_view()
    }

    fn observe_root_thread(&mut self, cx: &mut Context<Self>) {
        let Some(thread_view) = self.thread_view(cx) else {
            return;
        };
        let thread = thread_view.read(cx).thread.clone();
        if self.observed_thread == Some(thread.entity_id()) {
            return;
        }
        self.observed_thread = Some(thread.entity_id());
        self._subscriptions.push(cx.observe(&thread, |_, _, cx| {
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        }));
    }

    fn is_working(&self, cx: &App) -> bool {
        self.thread_view(cx)
            .is_some_and(|view| view.read(cx).thread.read(cx).status() != ThreadStatus::Idle)
    }
}

impl EventEmitter<ItemEvent> for AgentTab {}

impl Focusable for AgentTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.conversation_view.read(cx).focus_handle(cx)
    }
}

impl Item for AgentTab {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.conversation_view.read(cx).title(cx)
    }

    fn tab_icon(&self, _window: &Window, cx: &App) -> Option<Icon> {
        let working = self.is_working(cx);
        Some(
            Icon::new(if working {
                IconName::ArrowCircle
            } else {
                IconName::ZedAgent
            })
            .color(if working { Color::Accent } else { Color::Muted }),
        )
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(self.conversation_view.read(cx).title(cx))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for AgentTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.conversation_view.clone())
    }
}

fn serialized_agent_key(workspace_id: WorkspaceId, item_id: ItemId) -> String {
    format!("icarus-agent-tab:{}:{item_id}", i64::from(workspace_id))
}

/// What a restored tab needs: the server session to load, and the thread whose saved draft it shows.
#[derive(serde::Serialize, serde::Deserialize)]
struct SerializedAgentTab {
    agent_id: String,
    thread_id: ThreadId,
    // Older saves don't say; treat them as holding a conversation so nothing is replaced silently.
    #[serde(default = "default_true")]
    had_messages: bool,
}

fn default_true() -> bool {
    true
}

/// How long a restored tab waits to learn whether its agent loaded.
const RESTORE_WATCH_STEP: Duration = Duration::from_millis(100);
const RESTORE_WATCH_STEPS: usize = 600;

/// When a restored tab's agent is gone and it never had a message, starts a fresh agent in the same tab with
/// the same draft instead of showing the load error.
fn replace_if_lost_while_empty(
    tab: WeakEntity<AgentTab>,
    panel: WeakEntity<AgentPanel>,
    workspace: WeakEntity<Workspace>,
    thread_id: ThreadId,
    window: &mut Window,
    cx: &mut App,
) {
    window
        .spawn(cx, async move |cx| {
            for _ in 0..RESTORE_WATCH_STEPS {
                cx.background_executor().timer(RESTORE_WATCH_STEP).await;
                let state = tab.read_with(cx, |tab, cx| {
                    if tab.conversation_view.read(cx).failed_to_load() {
                        Some(true)
                    } else if tab.thread_view(cx).is_some() {
                        Some(false)
                    } else {
                        None
                    }
                })?;
                match state {
                    Some(false) => return anyhow::Ok(()),
                    Some(true) => {
                        return cx.update(|window, cx| {
                            let (Some(tab), Some(panel)) = (tab.upgrade(), panel.upgrade()) else {
                                return;
                            };
                            let conversation_view = panel.update(cx, |panel, cx| {
                                panel.new_tab_conversation(None, None, Some(thread_id), window, cx)
                            });
                            tab.update(cx, |tab, cx| {
                                tab.replace_conversation(conversation_view, cx)
                            });
                            let composer = workspace.upgrade().and_then(|workspace| {
                                AgentComposer::for_workspace(workspace.read(cx))
                            });
                            if let Some(composer) = composer {
                                composer.update(cx, |composer, cx| {
                                    if composer.target().as_ref() == Some(&tab) {
                                        composer.observed_target = None;
                                        composer.set_target(tab, cx);
                                    }
                                });
                            }
                        });
                    }
                    None => {}
                }
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

impl SerializableItem for AgentTab {
    fn serialized_item_kind() -> &'static str {
        "IcarusAgentTab"
    }

    fn cleanup(
        _workspace_id: WorkspaceId,
        _alive_items: Vec<ItemId>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        Task::ready(Ok(()))
    }

    fn deserialize(
        _project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let saved =
            KeyValueStore::global(cx).read_kvp(&serialized_agent_key(workspace_id, item_id));
        window.spawn(cx, async move |cx| {
            let saved = saved?.context("no agent recorded for this tab")?;
            let SerializedAgentTab {
                agent_id,
                thread_id,
                had_messages,
            } = serde_json::from_str(&saved)?;
            // Panels load alongside restored items, so the tab waits for the agent panel it builds on.
            for _ in 0..PANEL_WAIT_STEPS {
                let panel =
                    workspace.read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx))?;
                if let Some(panel) = panel {
                    return cx.update(|window, cx| {
                        let conversation_view = panel.update(cx, |panel, cx| {
                            panel.new_tab_conversation(
                                None,
                                Some(acp_v2::SessionId::new(agent_id.clone())),
                                Some(thread_id),
                                window,
                                cx,
                            )
                        });
                        let tab = cx.new(|cx| {
                            let mut tab = AgentTab::new(conversation_view, Some(agent_id), cx);
                            tab.saved_had_messages = had_messages;
                            tab
                        });
                        if !had_messages {
                            replace_if_lost_while_empty(
                                tab.downgrade(),
                                panel.downgrade(),
                                workspace.clone(),
                                thread_id,
                                window,
                                cx,
                            );
                        }
                        tab
                    });
                }
                cx.background_executor().timer(PANEL_WAIT_STEP).await;
            }
            anyhow::bail!("the agent panel never loaded")
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let had_messages = if self.thread_view(cx).is_some() {
            self.has_messages(cx)
        } else {
            self.saved_had_messages
        };
        let saved = SerializedAgentTab {
            agent_id: self.agent_id(cx)?,
            thread_id: self.conversation_view.read(cx).thread_id,
            had_messages,
        };
        let saved = serde_json::to_string(&saved).ok()?;
        let store = KeyValueStore::global(cx);
        Some(cx.background_spawn(async move {
            store
                .write_kvp(serialized_agent_key(workspace_id, item_id), saved)
                .await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(event, ItemEvent::UpdateTab)
    }
}

const PANEL_WAIT_STEP: Duration = Duration::from_millis(50);
const PANEL_WAIT_STEPS: usize = 200;

/// Picks the composer's target: the active agent tab, else the most recently used one still open.
pub(crate) fn choose_target(
    active: Option<EntityId>,
    recent: &[EntityId],
    open: &[EntityId],
) -> Option<EntityId> {
    if let Some(active) = active.filter(|id| open.contains(id)) {
        return Some(active);
    }
    recent
        .iter()
        .rev()
        .find(|id| open.contains(id))
        .or_else(|| open.last())
        .copied()
}

/// The one composer of a workspace, always shown under the center panes, typing into the target agent's draft.
pub struct AgentComposer {
    workspace: WeakEntity<Workspace>,
    target: Option<WeakEntity<AgentTab>>,
    recent: Vec<EntityId>,
    observed_target: Option<EntityId>,
    ensure_scheduled: bool,
    _target_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl AgentComposer {
    fn new(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscription =
            cx.subscribe_in(
                workspace,
                window,
                |composer, _, event, window, cx| match event {
                    workspace::Event::ActiveItemChanged
                    | workspace::Event::ItemAdded { .. }
                    | workspace::Event::ItemRemoved { .. }
                    | workspace::Event::PanelAdded(_) => composer.schedule_ensure(window, cx),
                    _ => {}
                },
            );
        Self {
            workspace: workspace.downgrade(),
            target: None,
            recent: Vec::new(),
            observed_target: None,
            ensure_scheduled: false,
            _target_subscriptions: Vec::new(),
            _subscriptions: vec![subscription],
        }
    }

    pub fn for_workspace(workspace: &Workspace) -> Option<Entity<Self>> {
        workspace.center_footer()?.clone().downcast::<Self>().ok()
    }

    pub fn target(&self) -> Option<Entity<AgentTab>> {
        self.target.as_ref().and_then(WeakEntity::upgrade)
    }

    fn schedule_ensure(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ensure_scheduled {
            return;
        }
        self.ensure_scheduled = true;
        // Deferred so an item moved between panes (removed, then added) never looks like the last tab closing.
        cx.defer_in(window, |composer, window, cx| {
            composer.ensure_scheduled = false;
            composer.ensure_agent(window, cx);
        });
    }

    /// Keeps a target agent, opening a fresh agent tab when the workspace has none.
    fn ensure_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        if workspace.read(cx).is_restoring() {
            // Restored agent tabs may still be on their way; a fresh agent now would be one too many.
            cx.spawn_in(window, async move |composer, cx| {
                cx.background_executor().timer(PANEL_WAIT_STEP * 4).await;
                composer.update_in(cx, |composer, window, cx| {
                    composer.schedule_ensure(window, cx)
                })
            })
            .detach_and_log_err(cx);
            return;
        }
        let (open_tabs, active) = {
            let workspace = workspace.read(cx);
            let open_tabs: Vec<Entity<AgentTab>> =
                workspace.items_of_type::<AgentTab>(cx).collect();
            let active = workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<AgentTab>())
                .map(|tab| tab.entity_id());
            (open_tabs, active)
        };
        if open_tabs.is_empty() {
            if !workspace_can_host_agents(&workspace, cx) {
                return;
            }
            workspace.update(cx, |workspace, cx| {
                open_agent_tab(workspace, None, None, false, window, cx);
            });
            return;
        }
        let open_ids: Vec<EntityId> = open_tabs.iter().map(|tab| tab.entity_id()).collect();
        self.recent.retain(|id| open_ids.contains(id));
        let Some(chosen) = choose_target(active, &self.recent, &open_ids) else {
            return;
        };
        if let Some(tab) = open_tabs.into_iter().find(|tab| tab.entity_id() == chosen) {
            self.set_target(tab, cx);
        }
    }

    fn set_target(&mut self, tab: Entity<AgentTab>, cx: &mut Context<Self>) {
        let id = tab.entity_id();
        self.recent.retain(|recent| *recent != id);
        self.recent.push(id);
        if self.observed_target != Some(id) {
            self.observed_target = Some(id);
            let conversation_view = tab.read(cx).conversation_view.clone();
            self._target_subscriptions = vec![
                cx.observe(&tab, |_, _, cx| cx.notify()),
                cx.observe(&conversation_view, |composer, _, cx| {
                    composer.observe_target_thread(cx);
                    cx.notify();
                }),
            ];
            self.target = Some(tab.downgrade());
            self.observe_target_thread(cx);
        }
        cx.notify();
    }

    fn observe_target_thread(&mut self, cx: &mut Context<Self>) {
        let Some(thread_view) = self.target().and_then(|tab| tab.read(cx).thread_view(cx)) else {
            return;
        };
        self._target_subscriptions
            .push(cx.observe(&thread_view, |_, _, cx| cx.notify()));
    }

    fn target_thread_view(&self, cx: &App) -> Option<Entity<ThreadView>> {
        self.target()?.read(cx).thread_view(cx)
    }

    /// Focuses the target agent's draft.
    pub fn focus(composer: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let focus_handle = composer
            .read(cx)
            .target_thread_view(cx)
            .map(|thread_view| thread_view.read(cx).message_editor.focus_handle(cx));
        if let Some(focus_handle) = focus_handle {
            focus_handle.focus(window, cx);
        }
    }

    /// Swaps a tab whose agent no longer exists for a fresh agent in the same pane.
    fn replace_failed_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(workspace), Some(tab)) = (self.workspace.upgrade(), self.target()) else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let pane = workspace.pane_for(&tab);
            open_agent_tab(workspace, None, pane.clone(), true, window, cx);
            if let Some(pane) = pane {
                pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(tab.entity_id(), SaveIntent::Skip, window, cx)
                        .detach_and_log_err(cx);
                });
            }
        });
    }

    /// Adds an attachment chip to the target agent's draft; it is sent with the next prompt.
    pub fn insert_attachment(
        &mut self,
        id: String,
        kind: String,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.target() else {
            log::error!("no agent to attach {kind} {id:?} to");
            return;
        };
        let conversation_view = tab.read(cx).conversation_view.clone();
        conversation_view.update(cx, |conversation_view, cx| {
            conversation_view.insert_attachment(id, kind, title, window, cx);
        });
    }
}

impl Render for AgentComposer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let composer = self.target_thread_view(cx).map(|thread_view| {
            thread_view.update(cx, |thread_view, cx| {
                thread_view.render_hosted_composer(window, cx)
            })
        });
        let failed = self
            .target()
            .is_some_and(|tab| tab.read(cx).conversation_view.read(cx).failed_to_load());
        div()
            .id("icarus-agent-composer")
            .w_full()
            .flex_none()
            .map(|this| match composer {
                Some(composer) => this.child(composer),
                None => this
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .px_3()
                    .py_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Label::new(if failed {
                                    "This agent couldn't be loaded."
                                } else {
                                    "Starting agent…"
                                })
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            )
                            .when(failed, |this| {
                                this.child(
                                    Button::new("replace-failed-agent", "Start a new agent")
                                        .label_size(LabelSize::Small)
                                        .on_click(cx.listener(|composer, _, window, cx| {
                                            composer.replace_failed_target(window, cx);
                                        })),
                                )
                            }),
                    ),
            })
    }
}

fn workspace_can_host_agents(workspace: &Entity<Workspace>, cx: &App) -> bool {
    let workspace = workspace.read(cx);
    let project = workspace.project().read(cx);
    !project.is_via_collab()
        && project.visible_worktrees(cx).next().is_some()
        && workspace.panel::<AgentPanel>(cx).is_some()
}

/// Opens an agent tab in `pane` (default: the active pane). `agent_id` loads that existing server agent
/// session (`session/load`) instead of starting a new one.
pub fn open_agent_tab(
    workspace: &mut Workspace,
    agent_id: Option<String>,
    pane: Option<Entity<Pane>>,
    focus: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<AgentTab>> {
    open_tab(workspace, None, agent_id, pane, focus, window, cx)
}

/// Opens an agent tab backed by `server`, so visual tests can show agents without a real one.
#[cfg(any(test, feature = "test-support"))]
pub fn open_agent_tab_with_server(
    workspace: &mut Workspace,
    server: std::rc::Rc<dyn agent_servers::AgentServer>,
    focus: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<AgentTab>> {
    open_tab(workspace, Some(server), None, None, focus, window, cx)
}

fn open_tab(
    workspace: &mut Workspace,
    server: Option<std::rc::Rc<dyn agent_servers::AgentServer>>,
    agent_id: Option<String>,
    pane: Option<Entity<Pane>>,
    focus: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<AgentTab>> {
    let pane = pane.unwrap_or_else(|| workspace.active_pane().clone());
    // A server session can back only one tab, so asking for an open agent moves its tab instead.
    let existing = agent_id.as_ref().and_then(|agent_id| {
        workspace
            .items_of_type::<AgentTab>(cx)
            .find(|tab| tab.read(cx).agent_id(cx).as_ref() == Some(agent_id))
    });
    let tab = match existing {
        Some(tab) => {
            match workspace.pane_for(&tab) {
                Some(source) if source != pane => {
                    let index = pane.read(cx).items_len();
                    workspace::move_item(&source, &pane, tab.entity_id(), index, true, window, cx);
                }
                _ => {
                    let index = pane.read(cx).index_for_item(&tab);
                    if let Some(index) = index {
                        pane.update(cx, |pane, cx| {
                            pane.activate_item(index, true, focus, window, cx)
                        });
                    }
                }
            }
            tab
        }
        None => {
            let panel = workspace.panel::<AgentPanel>(cx)?;
            let resume_session_id = agent_id.clone().map(acp_v2::SessionId::new);
            let conversation_view = panel.update(cx, |panel, cx| {
                panel.new_tab_conversation(server, resume_session_id, None, window, cx)
            });
            let tab = cx.new(|cx| AgentTab::new(conversation_view, agent_id, cx));
            workspace.add_item(pane, Box::new(tab.clone()), None, true, focus, window, cx);
            tab
        }
    };
    if let Some(composer) = AgentComposer::for_workspace(workspace) {
        let target = tab.clone();
        // Deferred: this may run from the composer's own update, when it opens the replacement for a closed tab.
        window.defer(cx, move |window, cx| {
            composer.update(cx, |composer, cx| composer.set_target(target, cx));
            if focus {
                AgentComposer::focus(&composer, window, cx);
            }
        });
    }
    Some(tab)
}

/// Picks the agent server for new and restored agent tabs in a project (Icarus: the server that owns
/// the project's workspace); `None` keeps the panel's selected agent.
pub struct TabAgentServer(
    pub  std::rc::Rc<
        dyn Fn(&Entity<Project>, &App) -> Option<std::rc::Rc<dyn agent_servers::AgentServer>>,
    >,
);

impl gpui::Global for TabAgentServer {}

/// Present once agent tabs are on; the agent panel then leaves sessions to the tabs.
pub struct AgentTabsEnabled;

impl gpui::Global for AgentTabsEnabled {}

/// Gives every workspace window agent tabs and one shared composer; Icarus calls this, upstream Zed does not.
pub fn init(cx: &mut App) {
    cx.set_global(AgentTabsEnabled);
    workspace::register_serializable_item::<AgentTab>(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        if let Some(window) = window {
            install(workspace, window, cx);
        }
    })
    .detach();
}

/// Adds the shared composer and the agent actions to one workspace.
pub fn install(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let handle = cx.entity();
    let composer = cx.new(|cx| AgentComposer::new(&handle, window, cx));
    workspace.set_center_footer(Some(AnyView::from(composer.clone())), cx);
    composer.update(cx, |composer, cx| composer.schedule_ensure(window, cx));
    workspace.register_action(|workspace, _: &NewAgent, window, cx| {
        open_agent_tab(workspace, None, None, true, window, cx);
    });
    workspace.register_action(|workspace, _: &FocusComposer, window, cx| {
        if let Some(composer) = AgentComposer::for_workspace(workspace) {
            AgentComposer::focus(&composer, window, cx);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::StubAgentConnection;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use project::Project;
    use std::path::Path;
    use workspace::MultiWorkspace;

    struct Probe;

    fn ids(cx: &mut TestAppContext, count: usize) -> Vec<EntityId> {
        (0..count)
            .map(|_| cx.update(|cx| cx.new(|_| Probe).entity_id()))
            .collect()
    }

    #[test]
    fn saves_from_before_message_tracking_count_as_conversations() {
        let thread_id = ThreadId::new();
        let old = serde_json::json!({ "agent_id": "agent-1", "thread_id": thread_id });
        let saved: SerializedAgentTab = serde_json::from_value(old).expect("old save");
        assert!(
            saved.had_messages,
            "an unknown conversation is never silently replaced"
        );
        let round_trip: SerializedAgentTab = serde_json::from_str(
            &serde_json::to_string(&SerializedAgentTab {
                agent_id: "agent-1".into(),
                thread_id,
                had_messages: false,
            })
            .expect("save"),
        )
        .expect("load");
        assert!(!round_trip.had_messages);
    }

    #[gpui::test]
    fn the_active_agent_tab_is_the_target(cx: &mut TestAppContext) {
        let tabs = ids(cx, 3);
        assert_eq!(
            choose_target(Some(tabs[1]), &[tabs[2], tabs[0]], &tabs),
            Some(tabs[1])
        );
    }

    #[gpui::test]
    fn without_an_active_agent_the_most_recent_open_one_is_the_target(cx: &mut TestAppContext) {
        let tabs = ids(cx, 3);
        assert_eq!(
            choose_target(None, &[tabs[0], tabs[2]], &tabs),
            Some(tabs[2])
        );
        let open = [tabs[0], tabs[1]];
        assert_eq!(
            choose_target(None, &[tabs[0], tabs[2]], &open),
            Some(tabs[0]),
            "a closed tab is skipped"
        );
    }

    #[gpui::test]
    fn an_agent_that_was_never_used_is_still_a_target(cx: &mut TestAppContext) {
        let tabs = ids(cx, 2);
        assert_eq!(choose_target(None, &[], &tabs), Some(tabs[1]));
        assert_eq!(choose_target(None, &[], &[]), None);
    }

    fn agent_tabs(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Vec<Entity<AgentTab>> {
        workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<AgentTab>(cx).collect()
        })
    }

    fn composer_target(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Option<Entity<AgentTab>> {
        workspace.read_with(cx, |workspace, cx| {
            AgentComposer::for_workspace(workspace)?.read(cx).target()
        })
    }

    fn draft(
        tab: &Entity<AgentTab>,
        cx: &mut VisualTestContext,
    ) -> Entity<crate::message_editor::MessageEditor> {
        tab.read_with(cx, |tab, cx| {
            tab.thread_view(cx)
                .expect("connected agent")
                .read(cx)
                .message_editor
                .clone()
        })
    }

    fn close_tab(
        workspace: &Entity<Workspace>,
        tab: &Entity<AgentTab>,
        cx: &mut VisualTestContext,
    ) {
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update_in(cx, |pane, window, cx| {
            pane.close_item_by_id(tab.entity_id(), SaveIntent::Skip, window, cx)
                .detach_and_log_err(cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn each_agent_keeps_its_draft_and_closing_the_last_opens_a_fresh_one(
        cx: &mut TestAppContext,
    ) {
        crate::test_support::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| {
            <dyn fs::Fs>::set_global(fs.clone(), cx);
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            crate::thread_metadata_store::init(cx);
            init(cx);
        });
        let _connection =
            crate::test_support::set_stub_agent_connection(StubAgentConnection::new());
        fs.insert_tree("/project", serde_json::json!({ "a.txt": "" }))
            .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            panel.update(cx, |panel, cx| {
                panel.select_agent(crate::Agent::Stub, window, cx)
            });
            workspace.add_panel(panel, window, cx);
        });
        cx.run_until_parked();

        let first = agent_tabs(&workspace, cx);
        assert_eq!(first.len(), 1, "a workspace always has an agent");
        let first = first[0].clone();
        assert_eq!(composer_target(&workspace, cx), Some(first.clone()));
        let first_draft = draft(&first, cx);
        first_draft.update_in(cx, |editor, window, cx| {
            editor.set_text("first draft", window, cx)
        });

        let second = workspace
            .update_in(cx, |workspace, window, cx| {
                open_agent_tab(workspace, None, None, true, window, cx)
            })
            .expect("second agent");
        cx.run_until_parked();
        assert_eq!(agent_tabs(&workspace, cx).len(), 2);
        assert_eq!(composer_target(&workspace, cx), Some(second.clone()));
        let second_draft = draft(&second, cx);
        assert_ne!(first_draft, second_draft, "every agent has its own draft");
        assert_eq!(second_draft.read_with(cx, |editor, cx| editor.text(cx)), "");
        assert!(
            !second_draft.read_with(cx, |editor, cx| editor.editor().read(cx).mode().is_full()),
            "an empty agent's composer keeps its height; a full-height editor collapses under the panes"
        );
        second_draft.update_in(cx, |editor, window, cx| {
            editor.set_text("second draft", window, cx)
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&first, true, true, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(composer_target(&workspace, cx), Some(first.clone()));
        assert_eq!(
            first_draft.read_with(cx, |editor, cx| editor.text(cx)),
            "first draft"
        );
        assert_eq!(
            second_draft.read_with(cx, |editor, cx| editor.text(cx)),
            "second draft"
        );

        close_tab(&workspace, &first, cx);
        assert_eq!(
            composer_target(&workspace, cx),
            Some(second.clone()),
            "closing the target moves the composer to the agent used before it"
        );

        close_tab(&workspace, &second, cx);
        let remaining = agent_tabs(&workspace, cx);
        assert_eq!(
            remaining.len(),
            1,
            "closing the last agent opens a fresh one"
        );
        assert!(remaining[0] != first && remaining[0] != second);
        assert_eq!(composer_target(&workspace, cx), Some(remaining[0].clone()));
    }
}
