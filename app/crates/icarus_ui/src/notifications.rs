//! Server notifications in the app: a toast while the app is in front, a system banner (with sound)
//! when it isn't, the unread count on the Dock icon, and a status-bar bell listing recent ones.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Global, IntoElement, Render, SharedString, SystemNotification, SystemNotificationAction,
    SystemNotificationResponse, Task, WeakEntity, Window, actions,
};
use gpui_tokio::Tokio;
use icarus_client::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use ui::{IconButton, IconName, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::notifications::NotificationId;
use workspace::notifications::simple_message_notification::MessageNotification;
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace};

use crate::{app_client, enabled, with_workspace_window};

actions!(
    icarus,
    [
        /// Shows the recent Icarus notifications.
        ShowNotifications,
        /// Marks every Icarus notification as read.
        MarkAllNotificationsRead,
        /// Deletes every Icarus notification.
        ClearNotifications,
    ]
);

/// How many notifications the list keeps and the server is asked for.
const RECENT: usize = 50;
const UPDATE_AVAILABLE: &str = "update_available";
const INSTALL_UPDATE: &str = "install-update";

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Notice {
    pub id: String,
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub read: bool,
}

impl Notice {
    fn agent_id(&self) -> Option<String> {
        self.data
            .as_ref()
            .and_then(|data| data["agent_id"].as_str())
            .map(str::to_owned)
    }
}

/// The recent notifications and the server's unread count.
#[derive(Default)]
pub struct NotificationStore {
    items: Vec<Notice>,
    unread: usize,
    /// What the Dock badge was last set to, so tests can see it and it is only set on change.
    badge: Option<String>,
}

impl NotificationStore {
    pub fn items(&self) -> &[Notice] {
        &self.items
    }

    pub fn unread(&self) -> usize {
        self.unread
    }

    pub fn badge(&self) -> Option<&str> {
        self.badge.as_deref()
    }

    /// Replaces the list wholesale, for previews and screen tests.
    pub fn replace(&mut self, items: Vec<Notice>, cx: &mut Context<Self>) {
        self.set_all(items, cx);
    }

    fn set_all(&mut self, items: Vec<Notice>, cx: &mut Context<Self>) {
        self.unread = items.iter().filter(|notice| !notice.read).count();
        self.items = items;
        self.items.truncate(RECENT);
        self.update_badge();
        cx.notify();
    }

    fn push(&mut self, notice: Notice, cx: &mut Context<Self>) {
        self.items.retain(|existing| existing.id != notice.id);
        if !notice.read {
            self.unread += 1;
        }
        self.items.insert(0, notice);
        self.items.truncate(RECENT);
        self.update_badge();
        cx.notify();
    }

    fn set_unread(&mut self, unread: usize, cx: &mut Context<Self>) {
        self.unread = unread;
        if unread == 0 {
            for notice in &mut self.items {
                notice.read = true;
            }
        }
        self.update_badge();
        cx.notify();
    }

    fn mark_read(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(notice) = self.items.iter_mut().find(|notice| notice.id == id)
            && !notice.read
        {
            notice.read = true;
            self.unread = self.unread.saturating_sub(1);
            self.update_badge();
            cx.notify();
        }
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.items.clear();
        self.unread = 0;
        self.update_badge();
        cx.notify();
    }

    fn update_badge(&mut self) {
        let badge = (self.unread > 0).then(|| self.unread.to_string());
        if badge != self.badge {
            set_dock_badge(badge.as_deref());
            self.badge = badge;
        }
    }
}

struct GlobalNotifications(Entity<NotificationStore>);

impl Global for GlobalNotifications {}

pub fn store(cx: &mut App) -> Entity<NotificationStore> {
    if let Some(store) = cx.try_global::<GlobalNotifications>() {
        return store.0.clone();
    }
    let store = cx.new(|_| NotificationStore::default());
    cx.set_global(GlobalNotifications(store.clone()));
    store
}

#[cfg(target_os = "macos")]
fn set_dock_badge(label: Option<&str>) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;
    // Tests and background threads have no main-thread marker; the badge is cosmetic there.
    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    if cfg!(test) {
        return;
    }
    let label = label.map(NSString::from_str);
    NSApplication::sharedApplication(main_thread)
        .dockTile()
        .setBadgeLabel(label.as_deref());
}

#[cfg(not(target_os = "macos"))]
fn set_dock_badge(_label: Option<&str>) {}

#[cfg(target_os = "macos")]
fn play_sound() {
    use objc2_app_kit::NSSound;
    use objc2_foundation::NSString;
    if cfg!(test) {
        return;
    }
    if let Some(sound) = NSSound::soundNamed(&NSString::from_str("Glass")) {
        sound.play();
    }
}

#[cfg(not(target_os = "macos"))]
fn play_sound() {}

/// Called once the app's server connection is up: loads the recent notifications.
pub(crate) fn connected(client: Arc<Client>, cx: &mut gpui::AsyncApp) -> Task<()> {
    cx.spawn(async move |cx| {
        let listed = Tokio::spawn_result(cx, async move {
            client
                .request("notification.list", json!({ "limit": RECENT }))
                .await
        })
        .await;
        let Some(listed) = listed.log_err() else {
            return;
        };
        let Some(items) = serde_json::from_value::<Vec<Notice>>(listed).log_err() else {
            return;
        };
        cx.update(|cx| {
            let store = store(cx);
            store.update(cx, |store, cx| store.set_all(items, cx));
        });
    })
}

/// Handles the server's `notification` and `notifications.changed` events.
pub(crate) fn handle_event(topic: &str, payload: Value, cx: &mut App) {
    match topic {
        "notification" => {
            let sound = payload["sound"].as_bool().unwrap_or(true);
            let Some(notice) = serde_json::from_value::<Notice>(payload).log_err() else {
                return;
            };
            let store = store(cx);
            store.update(cx, |store, cx| store.push(notice.clone(), cx));
            present(&notice, sound, cx);
        }
        "notifications.changed" => {
            let unread = payload["unread"].as_u64().unwrap_or(0) as usize;
            let store = store(cx);
            store.update(cx, |store, cx| store.set_unread(unread, cx));
        }
        _ => {}
    }
}

fn app_in_front(cx: &mut App) -> bool {
    cx.windows().into_iter().any(|window| {
        window
            .update(cx, |_, window, _| window.is_window_active())
            .unwrap_or(false)
    })
}

/// A toast while someone is looking at the app; otherwise a system banner they can click.
fn present(notice: &Notice, sound: bool, cx: &mut App) {
    if app_in_front(cx) {
        show_toast(notice, cx);
        return;
    }
    let actions = if notice.kind == UPDATE_AVAILABLE {
        vec![SystemNotificationAction {
            id: INSTALL_UPDATE.into(),
            label: "Restart to update".into(),
        }]
    } else {
        Vec::new()
    };
    cx.show_system_notification(SystemNotification {
        tag: notice.id.clone().into(),
        title: notice.title.clone().into(),
        body: notice.body.clone().into(),
        actions,
    });
    if sound {
        play_sound();
    }
}

struct IcarusNotificationToast;

fn show_toast(notice: &Notice, cx: &mut App) {
    let Some((window, workspace)) = crate::front_workspace(cx) else {
        return;
    };
    let notice = notice.clone();
    window
        .update(cx, |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                let id = NotificationId::composite::<IcarusNotificationToast>(SharedString::from(
                    notice.id.clone(),
                ));
                workspace.show_notification(id, cx, |cx| cx.new(|cx| toast_for(notice, cx)));
            });
        })
        .log_err();
}

fn toast_for(notice: Notice, cx: &mut App) -> MessageNotification {
    let body = notice.body.clone();
    let toast = MessageNotification::new(body, cx)
        .with_title(notice.title.clone())
        .show_suppress_button(false);
    if notice.kind == UPDATE_AVAILABLE {
        return toast
            .primary_message("Restart to update")
            .primary_on_click(|window, cx| {
                confirm_update(window, cx);
                cx.emit(DismissEvent);
            });
    }
    if notice.workspace_id.is_none() {
        return toast;
    }
    toast
        .primary_message("Open")
        .primary_on_click(move |_, cx| {
            open_notice(notice.clone(), cx);
            cx.emit(DismissEvent);
        })
}

/// Jumps to what a notification is about: its workspace's window, and its agent's tab if it names one.
pub fn open_notice(notice: Notice, cx: &mut App) {
    mark_read(Some(notice.id.clone()), cx);
    let (Some(workspace_id), Some(client)) = (notice.workspace_id.clone(), app_client(cx)) else {
        return;
    };
    let agent_id = notice.agent_id();
    cx.spawn(async move |cx| {
        let info = Tokio::spawn_result(cx, async move {
            client
                .request("workspace.info", json!({ "id": workspace_id }))
                .await
        })
        .await?;
        let Some(path) = info["path"].as_str().map(PathBuf::from) else {
            anyhow::bail!("the server sent no path for this workspace");
        };
        cx.update(|cx| {
            with_workspace_window(path, cx, move |workspace, window, cx| {
                if let Some(agent_id) = agent_id {
                    crate::open_agent_tab(Some(agent_id), None, workspace, window, cx);
                }
            });
        });
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn mark_read(id: Option<String>, cx: &mut App) {
    let store = store(cx);
    match &id {
        Some(id) => store.update(cx, |store, cx| store.mark_read(id, cx)),
        None => store.update(cx, |store, cx| store.set_unread(0, cx)),
    }
    let Some(client) = app_client(cx) else {
        return;
    };
    let params = match id {
        Some(id) => json!({ "id": id }),
        None => json!({ "all": true }),
    };
    Tokio::spawn_result(cx, async move {
        client.request("notification.read", params).await
    })
    .detach_and_log_err(cx);
}

fn clear_all(cx: &mut App) {
    let store = store(cx);
    store.update(cx, |store, cx| store.clear(cx));
    let Some(client) = app_client(cx) else {
        return;
    };
    Tokio::spawn_result(cx, async move {
        client.request("notification.clear", json!({})).await
    })
    .detach_and_log_err(cx);
}

/// Asks before running `icarus update install`, which restarts the app's server.
fn confirm_update(window: &mut Window, cx: &mut App) {
    let answer = window.prompt(
        gpui::PromptLevel::Info,
        "Install the Icarus update?",
        Some("Icarus downloads the new version, swaps it in and restarts its server. Agents sleep and resume, terminals keep running."),
        &["Install and restart", "Not now"],
        cx,
    );
    cx.spawn(async move |_| {
        if answer.await? != 0 {
            return anyhow::Ok(());
        }
        let status = util::command::new_command(crate::terminals::icarus_program())
            .args(["update", "install"])
            .status()
            .await?;
        anyhow::ensure!(status.success(), "icarus update install failed: {status}");
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn handle_system_response(response: SystemNotificationResponse, cx: &mut App) {
    let store = store(cx);
    let notice = store
        .read(cx)
        .items
        .iter()
        .find(|notice| notice.id.as_str() == response.tag.as_ref())
        .cloned();
    let Some(notice) = notice else {
        return;
    };
    cx.activate(true);
    if response.action_id.as_deref() == Some(INSTALL_UPDATE) || notice.kind == UPDATE_AVAILABLE {
        mark_read(Some(notice.id), cx);
        if let Some((window, _)) = crate::front_workspace(cx) {
            window
                .update(cx, |_, window, cx| confirm_update(window, cx))
                .log_err();
        }
        return;
    }
    open_notice(notice, cx);
}

/// The status-bar bell: the unread count, and a list of recent notifications on click.
pub struct NotificationsButton {
    store: Entity<NotificationStore>,
    _observe: gpui::Subscription,
}

impl NotificationsButton {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let store = store(cx);
        let observe = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            _observe: observe,
        }
    }
}

impl Render for NotificationsButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let unread = self.store.read(cx).unread;
        let store = self.store.clone();
        let icon = if unread > 0 {
            IconName::BellDot
        } else {
            IconName::Bell
        };
        let tooltip: SharedString = match unread {
            0 => "Notifications".into(),
            1 => "1 unread notification".into(),
            count => format!("{count} unread notifications").into(),
        };
        PopoverMenu::new("icarus-notifications")
            .menu(move |window, cx| {
                let store = store.clone();
                Some(cx.new(|cx| NotificationsList::new(store, window, cx)))
            })
            .anchor(gpui::Anchor::BottomRight)
            .trigger_with_tooltip(
                IconButton::new("icarus-notifications-button", icon)
                    .icon_size(IconSize::Small)
                    .when(unread > 0, |button| button.icon_color(Color::Accent)),
                Tooltip::text(tooltip),
            )
    }
}

impl StatusItemView for NotificationsButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}

/// The popover under the bell.
pub struct NotificationsList {
    store: Entity<NotificationStore>,
    focus_handle: FocusHandle,
    _observe: gpui::Subscription,
}

impl NotificationsList {
    pub fn new(
        store: Entity<NotificationStore>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            focus_handle: cx.focus_handle(),
            _observe: observe,
        }
    }
}

impl EventEmitter<DismissEvent> for NotificationsList {}

impl Focusable for NotificationsList {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn relative_time(at_ms: u64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(at_ms);
    let seconds = now_ms.saturating_sub(at_ms) / 1000;
    match seconds {
        0..60 => "now".into(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86_400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

impl Render for NotificationsList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let items = store.items.clone();
        let unread = store.unread;
        let weak: WeakEntity<Self> = cx.entity().downgrade();
        let header = h_flex()
            .px_2()
            .py_1()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Label::new("Notifications")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("icarus-notifications-read-all", "Mark all read")
                            .label_size(LabelSize::Small)
                            .disabled(unread == 0)
                            .on_click(|_, _, cx| mark_read(None, cx)),
                    )
                    .child(
                        Button::new("icarus-notifications-clear", "Clear")
                            .label_size(LabelSize::Small)
                            .disabled(items.is_empty())
                            .on_click(|_, _, cx| clear_all(cx)),
                    ),
            );
        let hover_background = cx.theme().colors().element_hover;
        let unread_dot = cx.theme().colors().text_accent;
        let rows = items.into_iter().enumerate().map(move |(index, notice)| {
            let weak = weak.clone();
            let unread = !notice.read;
            h_flex()
                .id(("icarus-notification", index))
                .px_2()
                .py_1()
                .gap_2()
                .items_start()
                .cursor_pointer()
                .hover(move |style| style.bg(hover_background))
                .child(
                    div()
                        .mt_1p5()
                        .size_1p5()
                        .rounded_full()
                        .when(unread, |dot| dot.bg(unread_dot)),
                )
                .child(
                    v_flex()
                        .min_w_0()
                        .flex_1()
                        .child(
                            h_flex()
                                .justify_between()
                                .gap_2()
                                .child(
                                    Label::new(notice.title.clone())
                                        .size(LabelSize::Small)
                                        .truncate()
                                        .when(!unread, |label| label.color(Color::Muted)),
                                )
                                .child(
                                    Label::new(relative_time(notice.created_at))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                        .when(!notice.body.is_empty(), |column| {
                            column.child(
                                Label::new(notice.body.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                        }),
                )
                .on_click(move |_, _, cx| {
                    open_notice(notice.clone(), cx);
                    weak.update(cx, |_, cx| cx.emit(DismissEvent)).log_err();
                })
        });
        let empty = self.store.read(cx).items.is_empty();
        v_flex()
            .key_context("IcarusNotifications")
            .track_focus(&self.focus_handle)
            .w(rems(22.))
            .max_h(rems(28.))
            .elevation_2(cx)
            .child(header)
            .child(
                v_flex()
                    .id("icarus-notifications-rows")
                    .overflow_y_scroll()
                    .py_1()
                    .children(rows)
                    .when(empty, |list| {
                        list.child(
                            div().px_2().py_2().child(
                                Label::new("Nothing yet")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        )
                    }),
            )
    }
}

pub(crate) fn register(cx: &mut App) {
    store(cx);
    cx.on_system_notification_response(handle_system_response);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        if !enabled(cx) {
            return;
        }
        let button = cx.new(NotificationsButton::new);
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_right_item(button, window, cx);
        });
        workspace.register_action(|_, _: &MarkAllNotificationsRead, _, cx| mark_read(None, cx));
        workspace.register_action(|_, _: &ClearNotifications, _, cx| clear_all(cx));
        workspace.register_action(|workspace, _: &ShowNotifications, window, cx| {
            let store = store(cx);
            workspace.toggle_modal(window, cx, |window, cx| {
                NotificationsModal::new(store, window, cx)
            });
        });
    })
    .detach();
}

/// The list as a modal, for the keyboard (`icarus::ShowNotifications`).
pub struct NotificationsModal {
    list: Entity<NotificationsList>,
}

impl NotificationsModal {
    fn new(store: Entity<NotificationStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let list = cx.new(|cx| NotificationsList::new(store, window, cx));
        cx.subscribe(&list, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent))
            .detach();
        Self { list }
    }
}

impl EventEmitter<DismissEvent> for NotificationsModal {}

impl Focusable for NotificationsModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.list.read(cx).focus_handle.clone()
    }
}

impl workspace::ModalView for NotificationsModal {}

impl Render for NotificationsModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.list.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::panel_window;

    fn payload(id: &str, kind: &str) -> Value {
        json!({
            "id": id,
            "kind": kind,
            "subject": "agent-1",
            "title": format!("{id} title"),
            "body": "The agent finished its turn",
            "workspace_id": "ws-main",
            "data": { "agent_id": "agent-1" },
            "created_at": 1,
            "read": false,
            "sound": true,
        })
    }

    // The test platform only shows banners for an app with an identity, as a bundled app has.
    fn with_identity(cx: &mut gpui::VisualTestContext) {
        cx.cx
            .update(|cx| cx.set_app_identity("dev.icarus.test", "Icarus"));
    }

    #[gpui::test]
    async fn notifications_toast_in_front_banner_behind_and_count_on_the_badge(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        with_identity(&mut cx);
        let toasts = |cx: &mut gpui::VisualTestContext| {
            workspace.read_with(cx, |workspace, _| workspace.notification_ids().len())
        };
        let before = toasts(&mut cx);

        cx.cx
            .update(|cx| handle_event("notification", payload("one", "agent_turn_done"), cx));
        cx.run_until_parked();
        assert_eq!(
            toasts(&mut cx),
            before + 1,
            "in front, a notification is a toast"
        );
        let banners = cx.shown_system_notifications();
        assert!(banners.is_empty(), "no banner while the app is in front");

        cx.deactivate_window();
        cx.cx
            .update(|cx| handle_event("notification", payload("two", "ci_failed"), cx));
        cx.run_until_parked();
        let banners = cx.shown_system_notifications();
        assert_eq!(banners.len(), 1, "behind other apps, it is a system banner");
        assert_eq!(banners[0].tag.as_ref(), "two");
        assert_eq!(banners[0].title.as_ref(), "two title");

        let store = cx.cx.update(|cx| store(cx));
        store.read_with(&cx, |store, _| {
            assert_eq!(store.unread(), 2);
            assert_eq!(store.badge(), Some("2"));
            assert_eq!(
                store
                    .items()
                    .iter()
                    .map(|n| n.id.as_str())
                    .collect::<Vec<_>>(),
                ["two", "one"],
                "newest first"
            );
        });

        cx.cx
            .update(|cx| handle_event("notifications.changed", json!({ "unread": 0 }), cx));
        store.read_with(&cx, |store, _| {
            assert_eq!(store.badge(), None, "no unread, no badge");
            assert!(store.items().iter().all(|notice| notice.read));
        });
    }

    #[gpui::test]
    async fn an_update_banner_offers_restart_and_clicking_a_banner_marks_it_read(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_, _, mut cx) = panel_window(Vec::new(), cx).await;
        with_identity(&mut cx);
        cx.cx.update(|cx| register(cx));
        cx.deactivate_window();
        cx.cx
            .update(|cx| handle_event("notification", payload("upd", UPDATE_AVAILABLE), cx));
        cx.cx
            .update(|cx| handle_event("notification", payload("turn", "agent_turn_done"), cx));
        cx.run_until_parked();
        let banners = cx.shown_system_notifications();
        let update = banners
            .iter()
            .find(|banner| banner.tag.as_ref() == "upd")
            .expect("update banner");
        assert_eq!(update.actions.len(), 1);
        assert_eq!(update.actions[0].label.as_ref(), "Restart to update");

        cx.simulate_system_notification_response(SystemNotificationResponse {
            tag: "turn".into(),
            action_id: None,
        });
        cx.run_until_parked();
        let store = cx.cx.update(|cx| store(cx));
        store.read_with(&cx, |store, _| {
            let turn = store.items().iter().find(|n| n.id == "turn").expect("kept");
            assert!(turn.read, "clicking the banner marks it read");
            assert_eq!(store.unread(), 1);
        });
    }

    #[gpui::test]
    async fn the_bell_list_renders_recent_notifications(cx: &mut gpui::TestAppContext) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        cx.cx
            .update(|cx| handle_event("notification", payload("one", "agent_turn_done"), cx));
        cx.cx.update(|cx| register(cx));
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let store = store(cx);
            workspace.toggle_modal(window, cx, |window, cx| {
                NotificationsModal::new(store, window, cx)
            });
        });
        cx.run_until_parked();
        assert!(workspace.read_with(&cx, |workspace, cx| {
            workspace.active_modal::<NotificationsModal>(cx).is_some()
        }));
        cx.cx.update(|cx| mark_read(None, cx));
        let store = cx.cx.update(|cx| store(cx));
        store.read_with(&cx, |store, _| assert_eq!(store.unread(), 0));
        cx.cx.update(|cx| clear_all(cx));
        store.read_with(&cx, |store, _| assert!(store.items().is_empty()));
    }
}
