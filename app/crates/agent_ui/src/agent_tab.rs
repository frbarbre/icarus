use std::time::Duration;

use acp_thread::ThreadStatus;
use agent_client_protocol::v2::schema as acp_v2;
use agent_servers::harness::{self, Harness};
use anyhow::Context as _;
use db::kvp::KeyValueStore;
use gpui::{
    AnyView, App, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, SharedString, Styled, Subscription, Task, WeakEntity, Window, actions,
    div,
};
use project::Project;
use ui::{AgentDotState, AgentStateIndicator, Color, Icon, IconName, Label, LabelSize, prelude::*};
use workspace::item::{Item, ItemEvent, SerializableItem};
use workspace::{ItemId, Pane, SaveIntent, Workspace, WorkspaceId, pane};

use crate::conversation_view::ThreadView;
use crate::thread_metadata_store::ThreadId;
use crate::{AgentPanel, ConversationView};

actions!(
    icarus,
    [
        /// Opens a new agent tab and points the composer at it.
        NewAgent,
        /// Opens a new Claude Code agent tab.
        NewClaudeAgent,
        /// Opens a new Codex agent tab.
        NewCodexAgent,
        /// Moves focus to the composer at the bottom of the workspace.
        FocusComposer,
    ]
);

/// Picks the agent server that starts a new `provider` agent in a project; Icarus sets it.
pub struct TabAgentProviderServer(
    pub  std::rc::Rc<
        dyn Fn(&Entity<Project>, &str, &App) -> Option<std::rc::Rc<dyn agent_servers::AgentServer>>,
    >,
);

impl gpui::Global for TabAgentProviderServer {}

/// Opens a new agent tab running `provider` (`claude` or `codex`) and focuses the composer on it.
pub fn open_provider_agent_tab(
    workspace: &mut Workspace,
    provider: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<AgentTab>> {
    let server = cx
        .try_global::<TabAgentProviderServer>()
        .map(|picker| picker.0.clone())
        .and_then(|picker| picker(workspace.project(), provider, cx));
    open_tab(workspace, server, None, None, true, window, cx)
}

/// Longest title a tab shows; its tooltip shows the whole one.
const TAB_TITLE_LIMIT: usize = 40;

/// What an agent tab is called: its conversation's title, else the harness it runs.
pub(crate) fn tab_title(title: SharedString, harness: Option<&Harness>) -> SharedString {
    match harness {
        Some(harness) if title.as_ref() == crate::DEFAULT_THREAD_TITLE => harness.name.into(),
        _ => title,
    }
}

/// The harness behind a conversation: what its session reports, else what its agent was started for.
pub(crate) fn conversation_harness(
    conversation_view: &ConversationView,
    cx: &App,
) -> Option<&'static Harness> {
    conversation_view
        .root_thread_view()
        .and_then(|view| thread_harness(view.read(cx).thread.read(cx)))
        .or_else(|| harness::harness_for_agent_id(&conversation_view.agent_key().id().0))
}

pub(crate) fn thread_harness(thread: &acp_thread::AcpThread) -> Option<&'static Harness> {
    harness::session_harness(thread.session_info().meta.as_ref())
}

/// An agent conversation shown as a center tab; its composer lives in the workspace's [`AgentComposer`].
pub struct AgentTab {
    conversation_view: Entity<ConversationView>,
    requested_agent_id: Option<String>,
    observed_thread: Option<EntityId>,
    /// What the last save said about messages, kept while a restored conversation hasn't loaded.
    saved_had_messages: bool,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    /// Set by the composer while it types into this agent and other agent tabs are open.
    marked_as_target: bool,
    _focus_subscription: Subscription,
    _subscriptions: Vec<Subscription>,
}

impl AgentTab {
    pub fn new(
        conversation_view: Entity<ConversationView>,
        requested_agent_id: Option<String>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&conversation_view, |tab, _, cx| {
            tab.observe_root_thread(cx);
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        });
        let focus_handle = cx.focus_handle();
        // Activating the tab (click, keys, a notification) means "talk to this agent".
        let focus_subscription = cx.on_focus(&focus_handle, window, |tab, window, cx| {
            tab.focus_composer(window, cx)
        });
        let mut tab = Self {
            conversation_view,
            requested_agent_id,
            observed_thread: None,
            saved_had_messages: false,
            focus_handle,
            workspace,
            marked_as_target: false,
            _focus_subscription: focus_subscription,
            _subscriptions: vec![subscription],
        };
        tab.observe_root_thread(cx);
        tab
    }

    /// Points the workspace's composer at this agent and moves focus into it.
    fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(composer) = self
            .workspace
            .upgrade()
            .and_then(|workspace| AgentComposer::for_workspace(workspace.read(cx)))
        else {
            return;
        };
        let tab = cx.entity();
        window.defer(cx, move |window, cx| {
            composer.update(cx, |composer, cx| composer.set_target(tab, cx));
            AgentComposer::focus(&composer, window, cx);
        });
    }

    fn set_marked_as_target(&mut self, marked: bool, cx: &mut Context<Self>) {
        if self.marked_as_target != marked {
            self.marked_as_target = marked;
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        }
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

    /// Whether the tab can't show its conversation: it failed to load, or connected without one.
    fn unavailable(&self, cx: &App) -> bool {
        let conversation_view = self.conversation_view.read(cx);
        conversation_view.failed_to_load() || conversation_view.connected_without_thread()
    }

    /// Loads this tab's agent session again in place.
    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.conversation_view.update(cx, |conversation_view, cx| {
            conversation_view.retry_load(window, cx)
        });
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

    /// What the tab's state glyph shows; an idle agent shows only its logo, as in Orca.
    fn dot_state(&self, cx: &App) -> Option<AgentDotState> {
        let view = self.thread_view(cx)?;
        let thread = view.read(cx).thread.read(cx);
        if thread.is_waiting_for_confirmation() {
            Some(AgentDotState::Permission)
        } else if thread.status() != ThreadStatus::Idle {
            Some(AgentDotState::Working)
        } else {
            None
        }
    }

    fn identity_icon(&self, cx: &App) -> Icon {
        let icon = self
            .harness(cx)
            .map_or(IconName::ZedAgent, |harness| harness.icon);
        Icon::new(icon).color(Color::Muted)
    }

    pub fn harness(&self, cx: &App) -> Option<&'static Harness> {
        conversation_harness(self.conversation_view.read(cx), cx)
    }

    fn title(&self, cx: &App) -> SharedString {
        tab_title(self.conversation_view.read(cx).title(cx), self.harness(cx))
    }
}

impl EventEmitter<ItemEvent> for AgentTab {}

impl Focusable for AgentTab {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for AgentTab {
    type Event = ItemEvent;

    fn tab_content(
        &self,
        params: workspace::item::TabContentParams,
        _window: &Window,
        cx: &App,
    ) -> gpui::AnyElement {
        let color = if self.marked_as_target {
            Color::Default
        } else {
            params.text_color()
        };
        let title = self.tab_content_text(params.detail.unwrap_or_default(), cx);
        let label = Label::new(util::truncate_and_trailoff(&title, TAB_TITLE_LIMIT))
            .single_line()
            .color(color);
        let Some(state) = self.dot_state(cx) else {
            return label.into_any_element();
        };
        // Orca puts the state before the agent's logo; the pane draws `tab_icon` first, so both live here.
        h_flex()
            .gap_1()
            .child(AgentStateIndicator::new(state))
            .child(self.identity_icon(cx).size(IconSize::Small))
            .child(label)
            .into_any_element()
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.title(cx)
    }

    fn tab_icon(&self, _window: &Window, cx: &App) -> Option<Icon> {
        self.dot_state(cx).is_none().then(|| self.identity_icon(cx))
    }

    fn tab_marked(&self, _cx: &App) -> bool {
        self.marked_as_target
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(self.title(cx))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn on_removed(&self, cx: &mut Context<Self>) {
        let Some(agent_id) = self.agent_id(cx) else {
            return;
        };
        let tab = cx.entity().downgrade();
        let workspace = self.workspace.clone();
        // Deferred: a tab moved between panes is removed before it is added again.
        cx.defer(move |cx| {
            let Some(tab) = tab.upgrade() else {
                end_closed_agent(agent_id, cx);
                return;
            };
            let still_open = workspace.upgrade().is_some_and(|workspace| {
                workspace
                    .read(cx)
                    .items_of_type::<AgentTab>(cx)
                    .any(|open| open == tab)
            });
            if !still_open {
                end_closed_agent(agent_id, cx);
            }
        });
    }
}

/// Ends the session behind a closed agent tab; Icarus sets it, as Orca kills a closed tab's agent.
pub struct TabAgentClosed(pub std::rc::Rc<dyn Fn(String, &mut App)>);

impl gpui::Global for TabAgentClosed {}

fn end_closed_agent(agent_id: String, cx: &mut App) {
    if let Some(handler) = cx
        .try_global::<TabAgentClosed>()
        .map(|handler| handler.0.clone())
    {
        handler(agent_id, cx);
    }
}

impl Render for AgentTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .debug_selector(|| format!("agent-tab:{}", self.title(cx)))
            .relative()
            .size_full()
            .child(self.conversation_view.clone())
            .when(self.unavailable(cx), |this| {
                let blank = self.conversation_view.read(cx).connected_without_thread();
                this.child(
                    v_flex()
                        .id("agent-tab-unavailable")
                        .debug_selector(|| "agent-tab-unavailable".into())
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .when(blank, |this| this.bottom_0().justify_center())
                        .when(!blank, |this| this.pt_8())
                        .items_center()
                        .gap_2()
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::Medium)
                                .color(Color::Warning),
                        )
                        .child(Label::new("This agent's conversation couldn't be loaded."))
                        .child(
                            div().debug_selector(|| "agent-tab-retry".into()).child(
                                Button::new("retry-agent", "Retry")
                                    .start_icon(Icon::new(IconName::RotateCw).size(IconSize::Small))
                                    .on_click(
                                        cx.listener(|tab, _, window, cx| tab.retry(window, cx)),
                                    ),
                            ),
                        ),
                )
            })
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
                            let mut tab = AgentTab::new(
                                conversation_view,
                                Some(agent_id),
                                workspace.clone(),
                                window,
                                cx,
                            );
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
    /// Focus asked for before the target's conversation loaded, with what had focus then; given once
    /// it has loaded, unless focus has moved on since.
    pending_focus: Option<Option<FocusHandle>>,
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
            pending_focus: None,
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
        cx.defer_in(window, |composer, _, cx| {
            composer.ensure_scheduled = false;
            composer.update_target(cx);
        });
    }

    /// Targets the active or most recent agent tab; with none open there is no composer.
    fn update_target(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
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
            if self.target.take().is_some() || self.observed_target.is_some() {
                self.observed_target = None;
                self.pending_focus = None;
                self._target_subscriptions.clear();
                cx.notify();
            }
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

    /// Marks the target's tab while several agents could be typed into.
    fn mark_target(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let tabs: Vec<Entity<AgentTab>> =
            workspace.read(cx).items_of_type::<AgentTab>(cx).collect();
        let target = self.observed_target;
        let several = tabs.len() > 1;
        for tab in tabs {
            let marked = several && Some(tab.entity_id()) == target;
            tab.update(cx, |tab, cx| tab.set_marked_as_target(marked, cx));
        }
    }

    fn set_target(&mut self, tab: Entity<AgentTab>, cx: &mut Context<Self>) {
        let id = tab.entity_id();
        self.recent.retain(|recent| *recent != id);
        self.recent.push(id);
        if self.observed_target != Some(id) {
            self.pending_focus = None;
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
        self.mark_target(cx);
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

    /// Focuses the target agent's draft, or does once its conversation has loaded.
    pub fn focus(composer: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let focus_handle = composer
            .read(cx)
            .target_thread_view(cx)
            .map(|thread_view| thread_view.read(cx).message_editor.focus_handle(cx));
        match focus_handle {
            Some(focus_handle) => focus_handle.focus(window, cx),
            None => {
                let focused = window.focused(cx);
                composer.update(cx, |composer, cx| {
                    composer.pending_focus = Some(focused);
                    cx.notify();
                })
            }
        }
    }

    /// Whether typing now goes into this composer's target agent.
    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.target_thread_view(cx).is_some_and(|thread_view| {
            thread_view
                .read(cx)
                .message_editor
                .focus_handle(cx)
                .contains_focused(window, cx)
        })
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

    // The composer sits outside every pane, so tab keys pressed in it would find no pane to act on
    // and fall through to the draft's own bindings (fold, delete word, typed characters).
    fn forward_to_active_pane<A: gpui::Action>(
        &mut self,
        action: &A,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self
            .workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).active_pane().clone())
        else {
            cx.propagate();
            return;
        };
        let pane_focus = pane.focus_handle(cx);
        let action = action.boxed_clone();
        window.defer(cx, move |window, cx| {
            pane_focus.dispatch_action(action.as_ref(), window, cx)
        });
    }
}

impl Render for AgentComposer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.pending_focus.is_some()
            && let Some(thread_view) = self.target_thread_view(cx)
            && let Some(focused_at_request) = self.pending_focus.take()
            && window.focused(cx) == focused_at_request
        {
            let focus_handle = thread_view.read(cx).message_editor.focus_handle(cx);
            window.defer(cx, move |window, cx| focus_handle.focus(window, cx));
        }
        let composer = self.target_thread_view(cx).map(|thread_view| {
            thread_view
                .update(cx, |thread_view, cx| {
                    thread_view.render_hosted_composer(window, cx)
                })
                .into_any_element()
        });
        let has_target = self.target().is_some();
        let failed = self
            .target()
            .is_some_and(|tab| tab.read(cx).conversation_view.read(cx).failed_to_load());
        div()
            .id("icarus-agent-composer")
            .debug_selector(|| "composer".into())
            .on_action(cx.listener(Self::forward_to_active_pane::<pane::ActivateItem>))
            .on_action(cx.listener(Self::forward_to_active_pane::<pane::ActivateNextItem>))
            .on_action(cx.listener(Self::forward_to_active_pane::<pane::ActivatePreviousItem>))
            .on_action(cx.listener(Self::forward_to_active_pane::<pane::ActivateLastItem>))
            .on_action(cx.listener(Self::forward_to_active_pane::<pane::CloseActiveItem>))
            .on_action(cx.listener(Self::forward_to_active_pane::<pane::CloseOtherItems>))
            .on_action(cx.listener(Self::forward_to_active_pane::<pane::CloseAllItems>))
            .on_action(cx.listener(Self::forward_to_active_pane::<pane::GoBack>))
            .on_action(cx.listener(Self::forward_to_active_pane::<pane::GoForward>))
            .w_full()
            .flex_none()
            .map(|this| match composer {
                Some(composer) => this
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .child(composer),
                None if !has_target => this,
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
            let workspace_handle = cx.entity().downgrade();
            let tab = cx
                .new(|cx| AgentTab::new(conversation_view, agent_id, workspace_handle, window, cx));
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

pub use agent_settings::AgentTabsEnabled;

/// Adds the focused editor's or terminal's selection to the composer's target agent.
pub(crate) fn add_selection_to_composer(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    use crate::completion_provider::AgentContextSource;
    let source = AgentContextSource::from_focused(workspace, window, cx)
        .or_else(|| AgentContextSource::from_active(workspace, cx));
    let Some(selection) = source.and_then(|source| source.read_selection(workspace, true, cx))
    else {
        return;
    };
    let Some(composer) = AgentComposer::for_workspace(workspace) else {
        return;
    };
    let Some(tab) = composer.read(cx).target() else {
        return;
    };
    let conversation_view = tab.read(cx).conversation_view.clone();
    conversation_view.update(cx, |conversation_view, cx| {
        conversation_view.insert_selection(selection, window, cx)
    });
    AgentComposer::focus(&composer, window, cx);
}

/// Swaps an agent panel that something opened for its dock's next panel (or closes the dock) and
/// focuses the composer, since agents live in tabs.
pub(crate) fn hide_agent_panel(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    for dock in workspace.all_docks() {
        let Some(index) = dock.read(cx).panel_index_for_type::<AgentPanel>() else {
            continue;
        };
        dock.update(cx, |dock, cx| {
            if dock.active_panel_index() != Some(index) {
                return;
            }
            match dock.first_enabled_panel_idx(cx) {
                Ok(other) if other != index => dock.activate_panel(other, window, cx),
                _ => dock.set_open(false, window, cx),
            }
        });
    }
    if let Some(composer) = AgentComposer::for_workspace(workspace) {
        AgentComposer::focus(&composer, window, cx);
    }
}

/// Gives every workspace window agent tabs and one shared composer; Icarus calls this, upstream Zed does not.
pub fn init(cx: &mut App) {
    cx.set_global(AgentTabsEnabled);
    crate::update_command_palette_filter(cx);
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
    workspace.register_action(|workspace, _: &NewClaudeAgent, window, cx| {
        open_provider_agent_tab(workspace, "claude", window, cx);
    });
    workspace.register_action(|workspace, _: &NewCodexAgent, window, cx| {
        open_provider_agent_tab(workspace, "codex", window, cx);
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

    #[test]
    fn an_untitled_tab_is_named_after_its_harness() {
        let codex = harness::harness("codex");
        assert_eq!(
            tab_title(crate::DEFAULT_THREAD_TITLE.into(), codex),
            "Codex"
        );
        assert_eq!(
            tab_title("Fix the login bug".into(), codex),
            "Fix the login bug"
        );
        assert_eq!(
            tab_title("Loading…".into(), codex),
            "Loading…",
            "loading and errors still say so"
        );
        assert_eq!(
            tab_title(crate::DEFAULT_THREAD_TITLE.into(), None),
            crate::DEFAULT_THREAD_TITLE
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
        let pane = workspace
            .read_with(cx, |workspace, _| workspace.pane_for(tab))
            .expect("the tab is open");
        pane.update_in(cx, |pane, window, cx| {
            pane.close_item_by_id(tab.entity_id(), SaveIntent::Skip, window, cx)
                .detach_and_log_err(cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn activating_an_agent_tab_focuses_its_draft_and_marks_it_while_typing(
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
            open_agent_tab(workspace, None, None, false, window, cx);
        });
        cx.run_until_parked();
        let first = agent_tabs(&workspace, cx)[0].clone();
        let marked = |tab: &Entity<AgentTab>, cx: &mut VisualTestContext| {
            tab.read_with(cx, |tab, _| tab.marked_as_target)
        };
        assert!(!marked(&first, cx), "one agent needs no marker");

        let second = workspace
            .update_in(cx, |workspace, window, cx| {
                open_agent_tab(workspace, None, None, false, window, cx)
            })
            .expect("second agent");
        cx.run_until_parked();
        assert!(marked(&second, cx) && !marked(&first, cx));

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&first, true, true, window, cx);
        });
        cx.run_until_parked();
        let first_draft = draft(&first, cx);
        assert!(
            cx.update(|window, cx| first_draft
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)),
            "activating the tab puts the cursor in its draft"
        );
        assert!(marked(&first, cx) && !marked(&second, cx));
        let composer = workspace
            .read_with(cx, |workspace, _| AgentComposer::for_workspace(workspace))
            .expect("composer");
        assert!(
            cx.update(|window, cx| composer.read(cx).is_focused(window, cx)),
            "typing goes to the marked agent"
        );

        cx.simulate_input("hi");
        assert_eq!(
            first_draft.read_with(cx, |editor, cx| editor.text(cx)),
            "hi"
        );
        assert!(
            marked(&first, cx),
            "the marker stays while focus is in the composer"
        );

        cx.dispatch_action(zed_actions::assistant::ToggleFocus);
        cx.run_until_parked();
        let (in_a_dock, has_button) = cx.update(|window, cx| {
            let workspace = workspace.read(cx);
            let panel = workspace.panel::<AgentPanel>(cx).expect("panel");
            let in_a_dock = workspace.all_docks().iter().any(|dock| {
                dock.read(cx)
                    .visible_panel()
                    .is_some_and(|visible| visible.panel_id() == panel.entity_id())
            });
            (
                in_a_dock,
                workspace::Panel::icon(panel.read(cx), window, cx).is_some(),
            )
        });
        assert!(
            !in_a_dock,
            "agents live in tabs; Zed's agent panel never shows"
        );
        assert!(!has_button, "and has no status bar button");
    }

    #[gpui::test]
    async fn a_tab_takes_its_sessions_title_and_harness_logo(cx: &mut TestAppContext) {
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
            open_agent_tab(workspace, None, None, false, window, cx);
        });
        cx.run_until_parked();
        let tab = agent_tabs(&workspace, cx)[0].clone();
        let named = |tab: &Entity<AgentTab>, cx: &mut VisualTestContext| {
            tab.read_with(cx, |tab, cx| {
                (
                    tab.tab_content_text(0, cx),
                    tab.harness(cx).map(|harness| harness.icon),
                )
            })
        };
        assert_eq!(named(&tab, cx), (crate::DEFAULT_THREAD_TITLE.into(), None));

        let thread = tab.read_with(cx, |tab, cx| {
            tab.thread_view(cx)
                .expect("connected agent")
                .read(cx)
                .thread
                .clone()
        });
        let codex = acp_v2::Meta::from_iter([(
            "icarus".into(),
            serde_json::json!({ "provider": "codex" }),
        )]);
        thread.update(cx, |thread, cx| {
            thread.update_session_info(acp_v2::SessionInfoUpdate::new().meta(codex), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            named(&tab, cx),
            ("Codex".into(), Some(IconName::AiOpenAi)),
            "before its first prompt the tab says which harness it runs"
        );

        thread.update(cx, |thread, cx| {
            thread.update_session_info(
                acp_v2::SessionInfoUpdate::new().title("Fix the login redirect"),
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            named(&tab, cx),
            ("Fix the login redirect".into(), Some(IconName::AiOpenAi)),
            "the session's title names the tab once the agent has one"
        );

        let harness_server: std::rc::Rc<dyn agent_servers::AgentServer> = std::rc::Rc::new(
            crate::test_support::StubAgentServer::new(StubAgentConnection::new()),
        );
        let selected_agent = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                let panel = workspace.read(cx).panel::<AgentPanel>(cx).expect("panel");
                panel.read(cx).selected_agent(cx)
            })
        };
        let selected_before = selected_agent(cx);
        let harness_tab = workspace
            .update_in(cx, |workspace, window, cx| {
                open_agent_tab_with_server(workspace, harness_server, false, window, cx)
            })
            .expect("harness tab");
        cx.run_until_parked();
        let key = harness_tab.read_with(cx, |tab, cx| {
            tab.conversation_view.read(cx).agent_key().clone()
        });
        assert_eq!(
            key,
            crate::Agent::Custom { id: "Test".into() },
            "a harness's tab connects to its own agent, so its session lists its own models"
        );
        assert_eq!(
            selected_agent(cx),
            selected_before,
            "and new tabs keep starting the selected agent"
        );
    }

    #[gpui::test]
    async fn a_tab_shows_its_agents_state_before_its_logo(cx: &mut TestAppContext) {
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
            open_agent_tab(workspace, None, None, false, window, cx);
            open_agent_tab(workspace, None, None, false, window, cx);
        });
        cx.run_until_parked();
        let tabs = agent_tabs(&workspace, cx);
        let state = |tab: &Entity<AgentTab>, cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                let tab = tab.read(cx);
                (tab.dot_state(cx), tab.tab_icon(window, cx).is_some())
            })
        };
        assert_eq!(
            state(&tabs[0], cx),
            (None, true),
            "idle shows only the logo"
        );

        let thread = |tab: &Entity<AgentTab>, cx: &mut VisualTestContext| {
            tab.read_with(cx, |tab, cx| {
                tab.thread_view(cx)
                    .expect("connected agent")
                    .read(cx)
                    .thread
                    .clone()
            })
        };
        let working = thread(&tabs[0], cx).update(cx, |thread, cx| {
            thread.send(vec!["Fix the login bug".into()], cx)
        });
        cx.run_until_parked();
        assert_eq!(
            state(&tabs[0], cx),
            (Some(AgentDotState::Working), false),
            "the state glyph and logo are drawn by the tab's content, state first"
        );

        let asking_thread = thread(&tabs[1], cx);
        let asking = asking_thread.update(cx, |thread, cx| {
            thread.send(vec!["Edit the login page".into()], cx)
        });
        cx.run_until_parked();
        asking_thread.update(cx, |thread, cx| {
            thread
                .update_session_state(
                    acp_v2::StateUpdate::RequiresAction(acp_v2::RequiresActionStateUpdate::new()),
                    cx,
                )
                .expect("requires action");
        });
        cx.run_until_parked();
        assert_eq!(
            state(&tabs[1], cx),
            (Some(AgentDotState::Permission), false),
            "an agent waiting for a permission answer asks for attention"
        );
        drop((working, asking));
    }

    #[gpui::test]
    async fn each_agent_keeps_its_draft_and_closing_one_ends_its_agent(cx: &mut TestAppContext) {
        crate::test_support::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let ended = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        cx.update(|cx| {
            <dyn fs::Fs>::set_global(fs.clone(), cx);
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            crate::thread_metadata_store::init(cx);
            init(cx);
            let ended = ended.clone();
            cx.set_global(TabAgentClosed(std::rc::Rc::new(move |agent_id, _| {
                ended.borrow_mut().push(agent_id)
            })));
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
        assert!(
            agent_tabs(&workspace, cx).is_empty(),
            "a new workspace starts without an agent"
        );
        assert_eq!(
            composer_target(&workspace, cx),
            None,
            "and so without a composer"
        );

        workspace.update_in(cx, |workspace, window, cx| {
            open_agent_tab(workspace, None, None, false, window, cx);
        });
        cx.run_until_parked();
        let first = agent_tabs(&workspace, cx);
        assert_eq!(first.len(), 1);
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

        let agent_id = |tab: &Entity<AgentTab>, cx: &mut VisualTestContext| {
            tab.read_with(cx, |tab, cx| tab.agent_id(cx))
                .expect("agent id")
        };
        let (first_id, second_id) = (agent_id(&first, cx), agent_id(&second, cx));

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_and_move(
                workspace.active_pane().clone(),
                workspace::SplitDirection::Right,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(agent_tabs(&workspace, cx).len(), 2);
        assert!(
            ended.borrow().is_empty(),
            "moving a tab to another pane keeps its agent"
        );

        close_tab(&workspace, &first, cx);
        assert_eq!(
            ended.borrow().as_slice(),
            std::slice::from_ref(&first_id),
            "closing a tab ends its agent"
        );
        assert_eq!(
            composer_target(&workspace, cx),
            Some(second.clone()),
            "closing the target moves the composer to the agent used before it"
        );

        close_tab(&workspace, &second, cx);
        assert_eq!(*ended.borrow(), [first_id, second_id]);
        assert!(
            agent_tabs(&workspace, cx).is_empty(),
            "every agent tab can be closed; none is opened in its place"
        );
        assert_eq!(composer_target(&workspace, cx), None);
        assert!(
            cx.debug_bounds("composer")
                .is_none_or(|bounds| bounds.size.height == gpui::px(0.)),
            "no composer without an agent tab"
        );
    }

    #[gpui::test]
    async fn a_tab_whose_agent_cannot_load_says_so_and_retries_the_same_session(
        cx: &mut TestAppContext,
    ) {
        let connection = crate::test_support::set_stub_agent_connection(
            StubAgentConnection::new().with_supports_load_session(true),
        );
        let (workspace, _project, mut cx) = workspace_with_agent(cx).await;
        let cx = &mut cx;
        connection.set_next_load_error(anyhow::anyhow!("session not found"));
        let server: std::rc::Rc<dyn agent_servers::AgentServer> = std::rc::Rc::new(
            crate::test_support::StubAgentServer::new(connection.clone()),
        );
        let tab = workspace
            .update_in(cx, |workspace, window, cx| {
                open_tab(
                    workspace,
                    Some(server),
                    Some("agent-restored".into()),
                    None,
                    true,
                    window,
                    cx,
                )
            })
            .expect("tab");
        cx.run_until_parked();
        assert!(tab.read_with(cx, |tab, cx| tab.unavailable(cx)));
        assert!(
            cx.debug_bounds("agent-tab-unavailable").is_some(),
            "the tab explains itself instead of staying blank"
        );

        let retry = cx.debug_bounds("agent-tab-retry").expect("retry button");
        cx.simulate_click(retry.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let (agent_id, unavailable) =
            tab.read_with(cx, |tab, cx| (tab.agent_id(cx), tab.unavailable(cx)));
        assert_eq!(
            agent_id.as_deref(),
            Some("agent-restored"),
            "the same session"
        );
        assert!(!unavailable, "retry loads it and shows the conversation");
        assert!(cx.debug_bounds("agent-tab-unavailable").is_none());
    }

    gpui::actions!(agent_tab_test, [WorkspaceShortcut]);

    async fn workspace_with_agent(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, Entity<Project>, VisualTestContext) {
        crate::test_support::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| {
            <dyn fs::Fs>::set_global(fs.clone(), cx);
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            crate::thread_metadata_store::init(cx);
            init(cx);
        });
        fs.insert_tree("/project", serde_json::json!({ "a.txt": "" }))
            .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            panel.update(cx, |panel, cx| {
                panel.select_agent(crate::Agent::Stub, window, cx)
            });
            workspace.add_panel(panel, window, cx);
            open_agent_tab(workspace, None, None, false, window, cx);
        });
        cx.run_until_parked();
        (workspace, project, cx)
    }

    fn add_editor(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> FocusHandle {
        workspace.update_in(cx, |workspace, window, cx| {
            let editor = cx.new(|cx| editor::Editor::multi_line(window, cx));
            let focus = editor.focus_handle(cx);
            workspace.add_item_to_active_pane(Box::new(editor), None, false, window, cx);
            focus
        })
    }

    fn add_terminal(
        workspace: &Entity<Workspace>,
        project: &Entity<Project>,
        cx: &mut VisualTestContext,
    ) -> FocusHandle {
        let project = project.downgrade();
        workspace.update_in(cx, |workspace, window, cx| {
            let terminal = cx.new(|cx| {
                terminal::TerminalBuilder::new_display_only(
                    terminal::terminal_settings::CursorShape::default(),
                    terminal::terminal_settings::AlternateScroll::On,
                    None,
                    0,
                    cx.background_executor(),
                    util::paths::PathStyle::local(),
                )
                .subscribe(cx)
            });
            let weak_workspace = cx.entity().downgrade();
            let database_id = workspace.database_id();
            let view = cx.new(|cx| {
                terminal_view::TerminalView::new(
                    terminal,
                    weak_workspace,
                    database_id,
                    project,
                    window,
                    cx,
                )
            });
            let focus = view.focus_handle(cx);
            workspace.add_item_to_active_pane(Box::new(view), None, false, window, cx);
            focus
        })
    }

    // Zed's editor keys first, then the imported Orca keys (Workspace/Editor/Terminal), which win as in the app.
    fn bind_user_navigation_keys(cx: &mut VisualTestContext) {
        cx.update(|_, cx| {
            cx.bind_keys([
                gpui::KeyBinding::new("alt-cmd-[", editor::actions::Fold, Some("Editor")),
                gpui::KeyBinding::new("alt-cmd-]", editor::actions::UnfoldLines, Some("Editor")),
                gpui::KeyBinding::new(
                    "ctrl-w",
                    editor::actions::DeleteToPreviousWordStart::default(),
                    Some("Editor"),
                ),
                gpui::KeyBinding::new("ctrl-t", editor::actions::Transpose, Some("Editor")),
            ]);
            for context in ["Workspace", "Editor", "Terminal"] {
                cx.bind_keys([
                    gpui::KeyBinding::new(
                        "alt-cmd-]",
                        pane::ActivateNextItem::default(),
                        Some(context),
                    ),
                    gpui::KeyBinding::new(
                        "alt-cmd-[",
                        pane::ActivatePreviousItem::default(),
                        Some(context),
                    ),
                    gpui::KeyBinding::new("alt-1", pane::ActivateItem(0), Some(context)),
                    gpui::KeyBinding::new("alt-3", pane::ActivateItem(2), Some(context)),
                    gpui::KeyBinding::new(
                        "ctrl-w",
                        pane::CloseOtherItems {
                            save_intent: None,
                            close_pinned: false,
                        },
                        Some(context),
                    ),
                    gpui::KeyBinding::new("ctrl-t", WorkspaceShortcut, Some(context)),
                ]);
            }
        });
    }

    fn items(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> (usize, usize) {
        workspace.read_with(cx, |workspace, cx| {
            let pane = workspace.active_pane().read(cx);
            (pane.items_len(), pane.active_item_index())
        })
    }

    fn has_focus(focus: &FocusHandle, cx: &mut VisualTestContext) -> bool {
        cx.update(|window, cx| focus.contains_focused(window, cx))
    }

    #[gpui::test]
    async fn navigation_keys_pressed_in_the_composer_navigate_and_move_focus_to_the_destination(
        cx: &mut TestAppContext,
    ) {
        let _connection =
            crate::test_support::set_stub_agent_connection(StubAgentConnection::new());
        let (workspace, project, mut cx) = workspace_with_agent(cx).await;
        let cx = &mut cx;
        let agent = agent_tabs(&workspace, cx)[0].clone();
        let file = add_editor(&workspace, cx);
        let terminal = add_terminal(&workspace, &project, cx);
        cx.run_until_parked();
        bind_user_navigation_keys(cx);
        let shortcut_runs = std::rc::Rc::new(std::cell::Cell::new(0));
        workspace.update(cx, |workspace, _| {
            let shortcut_runs = shortcut_runs.clone();
            workspace.register_action(move |_, _: &WorkspaceShortcut, _, _| {
                shortcut_runs.set(shortcut_runs.get() + 1)
            });
        });
        let draft = draft(&agent, cx);
        let draft_text =
            |cx: &mut VisualTestContext| draft.read_with(cx, |draft, cx| draft.text(cx));
        draft.update_in(cx, |draft, window, cx| {
            draft.set_text("fix the bug", window, cx)
        });
        let composer = workspace
            .read_with(cx, |workspace, _| AgentComposer::for_workspace(workspace))
            .expect("composer");
        let composer_focused = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| composer.read(cx).is_focused(window, cx))
        };

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&agent, true, true, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(items(&workspace, cx), (3, 0));
        assert!(
            composer_focused(cx),
            "moving to an agent tab focuses its draft"
        );

        cx.simulate_keystrokes("alt-cmd-]");
        cx.run_until_parked();
        assert_eq!(
            items(&workspace, cx),
            (3, 1),
            "next tab, not the draft's unfold"
        );
        assert!(has_focus(&file, cx) && !composer_focused(cx));

        cx.dispatch_action(FocusComposer);
        cx.run_until_parked();
        assert!(composer_focused(cx));
        cx.simulate_keystrokes("alt-cmd-]");
        cx.run_until_parked();
        assert_eq!(items(&workspace, cx), (3, 2));
        assert!(
            has_focus(&terminal, cx) && !composer_focused(cx),
            "a terminal tab reached from the composer takes the keys"
        );

        cx.dispatch_action(FocusComposer);
        cx.run_until_parked();
        cx.simulate_keystrokes("alt-cmd-[");
        cx.run_until_parked();
        assert_eq!(
            items(&workspace, cx),
            (3, 1),
            "previous tab, not the draft's fold"
        );
        assert!(has_focus(&file, cx) && !composer_focused(cx));

        cx.dispatch_action(FocusComposer);
        cx.run_until_parked();
        cx.simulate_keystrokes("alt-3");
        cx.run_until_parked();
        assert_eq!(items(&workspace, cx), (3, 2), "tab by index");
        assert!(has_focus(&terminal, cx) && !composer_focused(cx));

        cx.simulate_keystrokes("alt-1");
        cx.run_until_parked();
        assert_eq!(items(&workspace, cx), (3, 0));
        assert!(
            composer_focused(cx),
            "moving to the agent tab from the keyboard focuses its draft"
        );

        cx.simulate_keystrokes("ctrl-t");
        cx.run_until_parked();
        assert_eq!(
            shortcut_runs.get(),
            1,
            "workspace shortcuts run from the draft"
        );

        cx.simulate_keystrokes("ctrl-w");
        cx.run_until_parked();
        assert_eq!(items(&workspace, cx), (1, 0), "close other tabs");
        assert_eq!(
            draft_text(cx),
            "fix the bug",
            "no shortcut edited the draft or typed into it"
        );
    }

    #[gpui::test]
    async fn a_late_loading_agent_takes_focus_only_if_nothing_else_took_it_meanwhile(
        cx: &mut TestAppContext,
    ) {
        let _connection =
            crate::test_support::set_stub_agent_connection(StubAgentConnection::new());
        let (workspace, _project, mut cx) = workspace_with_agent(cx).await;
        let cx = &mut cx;
        let file = add_editor(&workspace, cx);
        cx.run_until_parked();
        let composer = workspace
            .read_with(cx, |workspace, _| AgentComposer::for_workspace(workspace))
            .expect("composer");
        let composer_focused = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| composer.read(cx).is_focused(window, cx))
        };
        let agent = composer_target(&workspace, cx).expect("agent");
        let request_focus_while_loading = |cx: &mut VisualTestContext| {
            composer.update(cx, |composer, _| composer.target = None);
            cx.update(|window, cx| AgentComposer::focus(&composer, window, cx));
            cx.run_until_parked();
        };
        let finish_loading = |cx: &mut VisualTestContext| {
            composer.update(cx, |composer, cx| {
                composer.target = Some(agent.downgrade());
                cx.notify();
            });
            cx.run_until_parked();
        };

        request_focus_while_loading(cx);
        cx.update(|window, cx| file.focus(window, cx));
        cx.run_until_parked();
        finish_loading(cx);
        assert!(
            has_focus(&file, cx) && !composer_focused(cx),
            "moving on while the agent loads keeps focus where the user went"
        );

        request_focus_while_loading(cx);
        finish_loading(cx);
        assert!(
            composer_focused(cx),
            "with focus still where it was asked from, the draft gets it"
        );
    }
}
