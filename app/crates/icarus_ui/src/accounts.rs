//! Agent usage in the status bar, laid out like Orca's: each provider's icon, its tightest window as
//! a bar and every window's use with its reset, and a roster popover with a submenu per provider
//! holding its windows and account switching. The server asks the providers; Icarus never stores
//! credentials, and adding an account shows the provider's own login command to run.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use gpui::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Context, DefiniteLength, Entity, FontWeight,
    Hsla, IntoElement, Render, Task, WeakEntity, Window, actions, px, relative,
};
use gpui_tokio::Tokio;
use serde::Deserialize;
use serde_json::json;
use ui::{
    ButtonLike, ContextMenu, ContextMenuSubmenu, IconButton, IconPosition, PopoverMenu,
    PopoverMenuHandle, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::notifications::NotificationId;
use workspace::notifications::simple_message_notification::MessageNotification;
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace};

use crate::{app_client, enabled};

actions!(
    icarus,
    [
        /// Reloads agent accounts and their usage from the Icarus server.
        RefreshAccounts,
    ]
);

const REFRESH: Duration = Duration::from_secs(60);
const COMPACT_KEY: &str = "icarus_usage_compact";
const SESSION_MINUTES: u64 = 300;
const WEEK_MINUTES: u64 = 10_080;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Account {
    pub id: String,
    pub provider: String,
    pub label: String,
    #[serde(default)]
    pub managed: bool,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub login_command: Option<String>,
}

impl Account {
    fn display_name(&self) -> String {
        if self.managed {
            self.email.clone().unwrap_or_else(|| self.label.clone())
        } else {
            "System login".to_owned()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct UsageWindow {
    pub key: String,
    pub used_percent: f64,
    pub window_minutes: u64,
    #[serde(default)]
    pub resets_at_ms: Option<u64>,
}

impl UsageWindow {
    pub fn percent(&self) -> u32 {
        if self.used_percent.is_finite() {
            self.used_percent.round().clamp(0.0, 100.0) as u32
        } else {
            0
        }
    }

    fn title(&self) -> &str {
        match self.key.as_str() {
            "session" => "Session",
            "weekly" => "Weekly",
            "fable" => "Fable",
            other => other,
        }
    }

    fn short_label(&self) -> String {
        match self.key.as_str() {
            "fable" => "Fable".to_owned(),
            _ => window_label(self.window_minutes),
        }
    }

    /// The status bar's label: the time left when known, as Orca's chips; Fable keeps its name.
    pub fn strip_label(&self, now_ms: u64) -> String {
        match (self.key.as_str(), self.resets_at_ms) {
            ("fable", _) => "Fable".to_owned(),
            (_, Some(resets)) => format_reset(resets as i64 - now_ms as i64),
            _ => window_label(self.window_minutes),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProviderUsage {
    pub provider: String,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub windows: Vec<UsageWindow>,
    #[serde(default)]
    pub failure: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub updated_at_ms: Option<u64>,
}

impl ProviderUsage {
    /// A provider with no login on this computer stays out of the way.
    pub fn worth_showing(&self) -> bool {
        !self.windows.is_empty() || self.failure.as_deref().is_some_and(|f| f != "no_login")
    }

    pub fn headline(&self) -> Option<&UsageWindow> {
        self.windows
            .iter()
            .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent))
    }

    fn soonest_reset(&self) -> Option<u64> {
        self.windows.iter().filter_map(|w| w.resets_at_ms).min()
    }

    pub fn status_label(&self) -> &'static str {
        match self.failure.as_deref() {
            Some("expired") => "Sign-in expired",
            Some("limited") => "Limited",
            Some("network") => "Network issue",
            Some("failed") => "Refresh failed",
            Some("no_login") => "not signed in",
            Some(_) => "Usage unavailable",
            None => "No usage data",
        }
    }

    /// The status bar's "percent label" pairs.
    pub fn strip_sections(&self, now_ms: u64, compact: bool) -> Vec<(u32, String)> {
        if compact {
            return self
                .headline()
                .map(|w| vec![(w.percent(), w.strip_label(now_ms))])
                .unwrap_or_default();
        }
        self.windows
            .iter()
            .map(|w| (w.percent(), w.strip_label(now_ms)))
            .collect()
    }
}

pub(crate) fn provider_name(provider: &str) -> &str {
    match provider {
        "claude" => "Claude",
        "codex" => "Codex",
        other => other,
    }
}

/// Orca's reset countdown: minutes, then hours and minutes, then days and hours, all floored.
pub fn format_reset(ms: i64) -> String {
    if ms <= 0 {
        return "now".to_owned();
    }
    let minutes = ms / 60_000;
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours, minutes) {
        (0, 0, minutes) => format!("{minutes}m"),
        (0, hours, 0) => format!("{hours}h"),
        (0, hours, minutes) => format!("{hours}h {minutes}m"),
        (days, 0, _) => format!("{days}d"),
        (days, hours, _) => format!("{days}d {hours}h"),
    }
}

fn resets_in(resets_at_ms: u64, now_ms: u64) -> String {
    match format_reset(resets_at_ms as i64 - now_ms as i64).as_str() {
        "now" => "Resets now".to_owned(),
        left => format!("Resets in {left}"),
    }
}

pub fn window_label(minutes: u64) -> String {
    match minutes {
        WEEK_MINUTES => "wk".to_owned(),
        SESSION_MINUTES => "5h".to_owned(),
        60 => "1h".to_owned(),
        minutes if minutes < 60 => format!("{minutes}m"),
        minutes if minutes % 1440 == 0 => format!("{}d", minutes / 1440),
        minutes if minutes % 60 == 0 => format!("{}h", minutes / 60),
        minutes => format!("{minutes}m"),
    }
}

pub fn format_time_ago(ms: u64) -> String {
    let minutes = ms / 60_000;
    match minutes {
        0 => "just now".to_owned(),
        minutes if minutes < 60 => format!("{minutes}m ago"),
        minutes => format!("{}h ago", minutes / 60),
    }
}

/// Highest use first, as Orca's roster; providers without usage go last.
pub fn roster_order(usage: &[ProviderUsage]) -> Vec<&ProviderUsage> {
    let mut rows: Vec<&ProviderUsage> = usage.iter().filter(|u| u.worth_showing()).collect();
    rows.sort_by(|a, b| {
        let peak = |u: &ProviderUsage| u.headline().map(|w| w.used_percent);
        peak(b).unwrap_or(-1.0).total_cmp(&peak(a).unwrap_or(-1.0))
    });
    rows
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn bar_color(percent: u32, cx: &App) -> Hsla {
    match percent {
        0..60 => cx.theme().colors().text_muted.opacity(0.4),
        60..80 => cx.theme().status().warning,
        _ => cx.theme().status().error,
    }
}

fn percent_color(percent: u32) -> Color {
    match percent {
        0..60 => Color::Default,
        60..80 => Color::Warning,
        _ => Color::Error,
    }
}

/// Read from the text colour so the track shows on light and dark surfaces alike.
fn track_color(cx: &App) -> Hsla {
    cx.theme().colors().text_muted.opacity(0.15)
}

fn bar(percent: u32, width: DefiniteLength, height: f32, cx: &App) -> gpui::Div {
    div()
        .flex_none()
        .w(width)
        .h(px(height))
        .rounded_full()
        .overflow_hidden()
        .bg(track_color(cx))
        .child(
            div()
                .h_full()
                .rounded_full()
                .w(relative(percent as f32 / 100.0))
                .bg(bar_color(percent, cx)),
        )
}

fn provider_icon(provider: &str) -> Icon {
    crate::cards::agent_provider_icon(provider)
}

fn icon_box(provider: &str, cx: &App) -> gpui::Div {
    div()
        .flex_none()
        .size(px(20.))
        .rounded_sm()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .flex()
        .items_center()
        .justify_center()
        .child(provider_icon(provider))
}

fn name_and_plan(usage: &ProviderUsage) -> gpui::Div {
    h_flex()
        .gap_1()
        .min_w_0()
        .child(Label::new(provider_name(&usage.provider).to_owned()).weight(FontWeight::MEDIUM))
        .when_some(usage.plan.clone(), |row, plan| {
            row.child(Label::new(format!("· {plan}")).color(Color::Muted))
        })
}

#[derive(Clone)]
pub struct AccountsSource {
    pub list: Rc<dyn Fn(&mut App) -> Task<Result<Vec<Account>>>>,
    pub usage: Rc<dyn Fn(bool, &mut App) -> Task<Result<Vec<ProviderUsage>>>>,
    pub select: Rc<dyn Fn(String, String, &mut App) -> Task<Result<()>>>,
    pub add: Rc<dyn Fn(String, String, &mut App) -> Task<Result<Account>>>,
}

impl AccountsSource {
    pub fn server() -> Self {
        fn not_connected<T>() -> Task<Result<T>> {
            Task::ready(Err(anyhow::anyhow!("not connected to the Icarus server")))
        }
        Self {
            list: Rc::new(|cx| {
                let Some(client) = app_client(cx) else {
                    return Task::ready(Ok(Vec::new()));
                };
                Tokio::spawn_result(cx, async move {
                    let value = client.request("account.list", json!({})).await?;
                    Ok(serde_json::from_value(value)?)
                })
            }),
            usage: Rc::new(|force, cx| {
                let Some(client) = app_client(cx) else {
                    return Task::ready(Ok(Vec::new()));
                };
                Tokio::spawn_result(cx, async move {
                    let value = client
                        .request("account.usage_summary", json!({ "force": force }))
                        .await?;
                    Ok(serde_json::from_value(value)?)
                })
            }),
            select: Rc::new(|provider, id, cx| {
                let Some(client) = app_client(cx) else {
                    return not_connected();
                };
                Tokio::spawn_result(cx, async move {
                    client
                        .request("account.select", json!({ "provider": provider, "id": id }))
                        .await?;
                    Ok(())
                })
            }),
            add: Rc::new(|provider, label, cx| {
                let Some(client) = app_client(cx) else {
                    return not_connected();
                };
                Tokio::spawn_result(cx, async move {
                    let value = client
                        .request(
                            "account.add",
                            json!({ "provider": provider, "label": label }),
                        )
                        .await?;
                    Ok(serde_json::from_value(value)?)
                })
            }),
        }
    }
}

/// Detailed or Compact, shared by every window's status item.
#[derive(Default)]
struct UsageViewMode {
    compact: bool,
}

impl gpui::Global for UsageViewMode {}

fn compact(cx: &App) -> bool {
    cx.try_global::<UsageViewMode>()
        .is_some_and(|mode| mode.compact)
}

/// What an open popover shows; its persistent builder reads this when it rebuilds.
#[derive(Default, Clone)]
struct Snapshot {
    accounts: Vec<Account>,
    usage: Vec<ProviderUsage>,
    compact: bool,
    refreshing: bool,
}

pub struct AccountsButton {
    source: AccountsSource,
    accounts: Vec<Account>,
    usage: Vec<ProviderUsage>,
    refreshing: bool,
    shown: Rc<RefCell<Snapshot>>,
    open_menu: Option<(WeakEntity<ContextMenu>, AnyWindowHandle)>,
    workspace: WeakEntity<Workspace>,
    menu_handle: PopoverMenuHandle<ContextMenu>,
    _poll: Task<()>,
}

impl AccountsButton {
    pub fn new(
        source: AccountsSource,
        workspace: WeakEntity<Workspace>,
        poll: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut button = Self {
            source,
            accounts: Vec::new(),
            usage: Vec::new(),
            refreshing: false,
            shown: Rc::default(),
            open_menu: None,
            workspace,
            menu_handle: PopoverMenuHandle::default(),
            _poll: Task::ready(()),
        };
        button._poll = if poll {
            cx.spawn(async move |this, cx| {
                loop {
                    if this.update(cx, |this, cx| this.reload(false, cx)).is_err() {
                        return;
                    }
                    cx.background_executor().timer(REFRESH).await;
                }
            })
        } else {
            Task::ready(())
        };
        button
    }

    /// A status item that never contacts a server and shows `accounts` and `usage`, for visual tests.
    pub fn preview(
        accounts: Vec<Account>,
        usage: Vec<ProviderUsage>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = AccountsSource {
            list: Rc::new({
                let accounts = accounts.clone();
                move |_| Task::ready(Ok(accounts.clone()))
            }),
            usage: Rc::new({
                let usage = usage.clone();
                move |_, _| Task::ready(Ok(usage.clone()))
            }),
            select: Rc::new(|_, _, _| Task::ready(Ok(()))),
            add: Rc::new(|_, _, _| Task::ready(Err(anyhow::anyhow!("a preview adds no accounts")))),
        };
        let mut button = Self::new(source, workspace, false, cx);
        button.accounts = accounts;
        button.usage = usage;
        button
    }

    /// Opens the roster popover, and with `open_first_provider` the top provider's submenu.
    pub fn show_roster(
        button: &Entity<Self>,
        open_first_provider: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let handle = button.read(cx).menu_handle.clone();
        handle.show(window, cx);
        if !open_first_provider {
            return;
        }
        let menu = button
            .read(cx)
            .open_menu
            .as_ref()
            .and_then(|(menu, _)| menu.upgrade());
        if let Some(menu) = menu {
            menu.update(cx, |menu, cx| {
                menu.select_first(&menu::SelectFirst, window, cx);
                menu.confirm(&menu::Confirm, window, cx);
            });
        }
    }

    pub fn hide_roster(button: &Entity<Self>, cx: &mut App) {
        // Not the handle's `hide`: it emits while borrowing the handle, which a top-level caller's
        // effect flush then borrows again.
        let menu = button
            .read(cx)
            .open_menu
            .as_ref()
            .and_then(|(menu, _)| menu.upgrade());
        if let Some(menu) = menu {
            menu.update(cx, |_, cx| cx.emit(gpui::DismissEvent));
        }
    }

    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    pub fn usage(&self) -> &[ProviderUsage] {
        &self.usage
    }

    /// Reloads accounts and usage; `force` asks the providers again instead of the server's cache.
    pub fn reload(&mut self, force: bool, cx: &mut Context<Self>) {
        let list = (self.source.list)(cx);
        let usage = (self.source.usage)(force, cx);
        if force {
            self.refreshing = true;
            self.sync_menu(cx);
        }
        cx.spawn(async move |this, cx| {
            let accounts = list.await.log_err();
            let usage = usage.await.log_err();
            this.update(cx, |this, cx| {
                if let Some(accounts) = accounts {
                    this.accounts = accounts;
                }
                if let Some(usage) = usage {
                    this.usage = usage;
                }
                if force {
                    this.refreshing = false;
                }
                this.sync_menu(cx);
            })
            .log_err();
        })
        .detach();
    }

    fn snapshot(&self, cx: &App) -> Snapshot {
        Snapshot {
            accounts: self.accounts.clone(),
            usage: self.usage.clone(),
            compact: compact(cx),
            refreshing: self.refreshing,
        }
    }

    /// Refreshes the status bar and rebuilds an open popover with what is now known.
    fn sync_menu(&mut self, cx: &mut Context<Self>) {
        *self.shown.borrow_mut() = self.snapshot(cx);
        cx.notify();
        let Some((menu, window)) = self.open_menu.clone() else {
            return;
        };
        let Some(menu) = menu.upgrade() else {
            self.open_menu = None;
            return;
        };
        window
            .update(cx, |_, window, cx| {
                menu.update(cx, |menu, cx| menu.rebuild(window, cx));
            })
            .log_err();
    }

    pub fn set_compact(&mut self, compact: bool, cx: &mut Context<Self>) {
        cx.default_global::<UsageViewMode>().compact = compact;
        let store = db::kvp::KeyValueStore::global(cx);
        db::write_and_log(cx, move || async move {
            store
                .write_kvp(COMPACT_KEY.to_string(), compact.to_string())
                .await
        });
        self.sync_menu(cx);
    }

    pub fn select(&mut self, provider: String, id: String, cx: &mut Context<Self>) {
        let select = (self.source.select)(provider, id, cx);
        cx.spawn(async move |this, cx| {
            select.await.log_err();
            this.update(cx, |this, cx| this.reload(true, cx)).log_err();
        })
        .detach();
    }

    /// Creates the account's own config dir on the server and shows the command that signs it in.
    pub fn add(&mut self, provider: String, cx: &mut Context<Self>) {
        let count = self
            .accounts
            .iter()
            .filter(|account| account.provider == provider && account.managed)
            .count();
        let label = format!("{} account {}", provider_name(&provider), count + 2);
        let add = (self.source.add)(provider, label, cx);
        let workspace = self.workspace.clone();
        cx.spawn(async move |this, cx| {
            let added = add.await;
            this.update(cx, |this, cx| {
                this.reload(false, cx);
                let message = match &added {
                    Ok(account) => format!(
                        "Added {}. Sign it in by running this in a terminal:",
                        account.label
                    ),
                    Err(error) => format!("Couldn't add the account: {error:#}"),
                };
                let command = added.ok().and_then(|account| account.login_command);
                workspace
                    .update(cx, |workspace, cx| {
                        struct AccountAdded;
                        workspace.show_notification(
                            NotificationId::unique::<AccountAdded>(),
                            cx,
                            |cx| {
                                cx.new(|cx| {
                                    let toast = MessageNotification::new(message, cx)
                                        .show_suppress_button(false);
                                    match command {
                                        Some(command) => {
                                            toast.with_title(command.clone()).copy_text(command)
                                        }
                                        None => toast,
                                    }
                                })
                            },
                        );
                    })
                    .log_err();
            })
            .log_err();
        })
        .detach();
    }

    /// Opens with what is already known, then reloads and rebuilds the open popover.
    fn menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<ContextMenu> {
        *self.shown.borrow_mut() = self.snapshot(cx);
        let shown = self.shown.clone();
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build_persistent(window, cx, move |menu, _, _| {
            roster_menu(menu, &shown.borrow(), this.clone(), now_ms())
        });
        self.open_menu = Some((menu.downgrade(), window.window_handle()));
        self.reload(false, cx);
        menu
    }

    fn render_strip(&self, cx: &App) -> Option<AnyElement> {
        let now = now_ms();
        let compact = compact(cx);
        let shown: Vec<&ProviderUsage> = self.usage.iter().filter(|u| u.worth_showing()).collect();
        let segments: Vec<AnyElement> = if shown.is_empty() {
            // Managed accounts keep the item reachable even before any usage is known.
            let mut providers: Vec<&str> = self
                .accounts
                .iter()
                .filter(|a| a.managed)
                .map(|a| a.provider.as_str())
                .collect();
            providers.dedup();
            providers
                .into_iter()
                .map(|provider| {
                    h_flex()
                        .gap_1()
                        .child(provider_icon(provider))
                        .child(
                            Label::new("--")
                                .size(LabelSize::Small)
                                .color(Color::Disabled),
                        )
                        .into_any_element()
                })
                .collect()
        } else {
            shown
                .into_iter()
                .map(|usage| strip_segment(usage, now, compact, cx))
                .collect()
        };
        (!segments.is_empty()).then(|| h_flex().gap_3().children(segments).into_any_element())
    }
}

fn strip_segment(usage: &ProviderUsage, now_ms: u64, compact: bool, cx: &App) -> AnyElement {
    let segment = h_flex().gap_1p5().child(provider_icon(&usage.provider));
    let Some(headline) = usage.headline() else {
        return segment
            .child(
                Icon::new(IconName::Warning)
                    .size(IconSize::XSmall)
                    .color(Color::Warning),
            )
            .child(
                Label::new(usage.status_label())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element();
    };
    let mut sections = h_flex().gap_1();
    for (index, (percent, label)) in usage
        .strip_sections(now_ms, compact)
        .into_iter()
        .enumerate()
    {
        if index > 0 {
            sections = sections.child(Label::new("·").size(LabelSize::Small).color(Color::Muted));
        }
        sections = sections
            .child(
                Label::new(format!("{percent}%"))
                    .size(LabelSize::Small)
                    .color(percent_color(percent)),
            )
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted));
    }
    segment
        .when(!compact, |segment| {
            segment.child(bar(headline.percent(), px(48.).into(), 6., cx))
        })
        .child(sections)
        .into_any_element()
}

fn roster_menu(
    menu: ContextMenu,
    shown: &Snapshot,
    this: WeakEntity<AccountsButton>,
    now_ms: u64,
) -> ContextMenu {
    let compact = shown.compact;
    let refreshing = shown.refreshing;
    let mut menu = menu
        .fixed_width(px(360.).into())
        .keep_open_on_confirm(false)
        .custom_row({
            let this = this.clone();
            move |_, _| {
                let this = this.clone();
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(Label::new("Usage").weight(FontWeight::SEMIBOLD))
                    .child(div().flex_1())
                    .child(
                        Label::new("all agents")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        IconButton::new("icarus-usage-refresh", IconName::RotateCw)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .disabled(refreshing)
                            .tooltip(Tooltip::text("Refresh usage"))
                            .on_click(move |_, _, cx| {
                                this.update(cx, |this, cx| this.reload(true, cx)).log_err();
                            }),
                    )
                    .into_any_element()
            }
        })
        .custom_row({
            let this = this.clone();
            move |_, cx| {
                let mode =
                    |id: &'static str, label: &'static str, tip: &'static str, value: bool| {
                        let this = this.clone();
                        let selected = compact == value;
                        let colors = cx.theme().colors();
                        div()
                            .flex_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(gpui::transparent_black())
                            .when(selected, |segment| {
                                segment
                                    .bg(colors.elevated_surface_background)
                                    .border_color(colors.border_variant)
                            })
                            .child(
                                Button::new(id, label)
                                    .full_width()
                                    .label_size(LabelSize::Small)
                                    .color(if selected {
                                        Color::Default
                                    } else {
                                        Color::Muted
                                    })
                                    .style(ButtonStyle::Transparent)
                                    .tooltip(Tooltip::text(tip))
                                    .on_click(move |_, _, cx| {
                                        this.update(cx, |this, cx| this.set_compact(value, cx))
                                            .log_err();
                                    }),
                            )
                    };
                h_flex()
                    .w_full()
                    .p_0p5()
                    .gap_0p5()
                    .rounded_md()
                    .bg(track_color(cx))
                    .child(mode(
                        "icarus-usage-detailed",
                        "Detailed",
                        "Full usage with bars, labels, and percentages",
                        false,
                    ))
                    .child(mode(
                        "icarus-usage-compact",
                        "Compact",
                        "Condensed usage: one summary per provider",
                        true,
                    ))
                    .into_any_element()
            }
        })
        .separator();

    let rows = roster_order(&shown.usage);
    if rows.is_empty() {
        menu = menu.custom_row(|_, _| {
            Label::new("No agent usage on this computer yet")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element()
        });
    }
    for usage in rows {
        let accounts: Vec<Account> = shown
            .accounts
            .iter()
            .filter(|a| a.provider == usage.provider)
            .cloned()
            .collect();
        let submenu_usage = usage.clone();
        let this = this.clone();
        let row_usage = usage.clone();
        menu = menu.item(
            ContextMenuSubmenu::new(
                provider_name(&usage.provider).to_owned(),
                move |menu, _, _| {
                    provider_menu(menu, &submenu_usage, &accounts, this.clone(), now_ms)
                },
            )
            .id(SharedString::from(format!(
                "icarus-usage-{}",
                usage.provider
            )))
            .custom_trigger(move |_, cx| roster_row(&row_usage, compact, now_ms, cx)),
        );
    }
    menu.separator().custom_entry(
        |_, _| {
            h_flex()
                .w_full()
                .justify_between()
                .child(Label::new("Manage Accounts…"))
                .child(
                    Icon::new(IconName::ChevronRight)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element()
        },
        |window, cx| crate::settings_pages::open_agent_settings(window, cx),
    )
}

fn roster_row(usage: &ProviderUsage, compact: bool, now_ms: u64, cx: &App) -> AnyElement {
    let right = match (usage.headline(), compact) {
        (Some(headline), true) => format!("{}% {}", headline.percent(), headline.short_label()),
        (Some(_), false) => usage
            .soonest_reset()
            .map(|resets| resets_in(resets, now_ms))
            .unwrap_or_default(),
        (None, _) => usage.status_label().to_owned(),
    };
    let first = h_flex()
        .w_full()
        .gap_2()
        .child(icon_box(&usage.provider, cx))
        .child(name_and_plan(usage))
        .child(div().flex_1())
        .child(
            Label::new(right)
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(
            Icon::new(IconName::ChevronRight)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        );
    let windows = (!compact && !usage.windows.is_empty()).then(|| {
        h_flex()
            .pl(px(30.))
            .gap_3()
            .children(usage.windows.iter().map(|window| {
                h_flex()
                    .gap_1()
                    .child(
                        Label::new(window.short_label())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(bar(window.percent(), px(28.).into(), 5., cx))
                    .child(
                        Label::new(format!("{}%", window.percent()))
                            .size(LabelSize::XSmall)
                            .color(percent_color(window.percent())),
                    )
            }))
    });
    v_flex()
        .w_full()
        .py_0p5()
        .gap_1()
        .child(first)
        .children(windows)
        .into_any_element()
}

fn provider_menu(
    menu: ContextMenu,
    usage: &ProviderUsage,
    accounts: &[Account],
    this: WeakEntity<AccountsButton>,
    now_ms: u64,
) -> ContextMenu {
    let header = usage.clone();
    let mut menu = menu
        .fixed_width(px(300.).into())
        .keep_open_on_confirm(false)
        .custom_row(move |_, cx| {
            h_flex()
                .w_full()
                .gap_2()
                .child(icon_box(&header.provider, cx))
                .child(name_and_plan(&header))
                .child(div().flex_1())
                .when_some(header.updated_at_ms, |row, at| {
                    row.child(
                        Label::new(format!(
                            "Updated {}",
                            format_time_ago(now_ms.saturating_sub(at))
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                })
                .into_any_element()
        })
        .separator();
    for window in &usage.windows {
        let window = window.clone();
        let title = window.title().to_owned();
        menu = menu.custom_row(move |_, cx| {
            v_flex()
                .w_full()
                .gap_1()
                .py_0p5()
                .child(
                    Label::new(title.clone())
                        .size(LabelSize::Small)
                        .weight(FontWeight::MEDIUM),
                )
                .child(bar(window.percent(), relative(1.), 6., cx))
                .child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .child(
                            Label::new(format!("{}% used", window.percent()))
                                .size(LabelSize::XSmall)
                                .color(percent_color(window.percent())),
                        )
                        .when_some(window.resets_at_ms, |row, resets| {
                            row.child(
                                Label::new(resets_in(resets, now_ms))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        }),
                )
                .into_any_element()
        });
    }
    if let Some(error) = usage.error.clone() {
        menu = menu.custom_row(move |_, _| {
            Label::new(error.clone())
                .size(LabelSize::XSmall)
                .color(Color::Error)
                .into_any_element()
        });
    }
    if accounts.is_empty() {
        return menu;
    }
    menu = menu.separator().header("Accounts");
    for account in accounts {
        let this = this.clone();
        let provider = account.provider.clone();
        let id = account.id.clone();
        menu = menu.toggleable_entry(
            account.display_name(),
            account.default,
            IconPosition::Start,
            None,
            move |_, cx| {
                this.update(cx, |this, cx| this.select(provider.clone(), id.clone(), cx))
                    .log_err();
            },
        );
    }
    let provider = usage.provider.clone();
    menu.entry(
        format!("Add a {} account…", provider_name(&provider)),
        None,
        move |_, cx| {
            this.update(cx, |this, cx| this.add(provider.clone(), cx))
                .log_err();
        },
    )
}

impl Render for AccountsButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(strip) = self.render_strip(cx) else {
            return div().into_any_element();
        };
        let this = cx.entity();
        PopoverMenu::new("icarus-accounts")
            .with_handle(self.menu_handle.clone())
            .menu(move |window, cx| Some(this.update(cx, |this, cx| this.menu(window, cx))))
            .anchor(gpui::Anchor::BottomRight)
            .trigger_with_tooltip(
                ButtonLike::new("icarus-accounts-button").child(strip),
                Tooltip::text("Agent usage and accounts"),
            )
            .into_any_element()
    }
}

impl StatusItemView for AccountsButton {
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

#[derive(Default)]
struct AccountsButtons(Vec<WeakEntity<AccountsButton>>);

impl gpui::Global for AccountsButtons {}

/// Handles the server's `account.usage`: an agent's turn ended with fresh usage.
pub(crate) fn usage_changed(cx: &mut App) {
    let buttons = &mut cx.default_global::<AccountsButtons>().0;
    buttons.retain(|button| button.upgrade().is_some());
    let buttons: Vec<_> = buttons
        .iter()
        .filter_map(|button| button.upgrade())
        .collect();
    for button in buttons {
        button.update(cx, |button, cx| button.reload(false, cx));
    }
}

pub(crate) fn register(cx: &mut App) {
    let compact = db::kvp::KeyValueStore::global(cx)
        .read_kvp(COMPACT_KEY)
        .ok()
        .flatten()
        .is_some_and(|value| value == "true");
    cx.default_global::<UsageViewMode>().compact = compact;
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        if !enabled(cx) {
            return;
        }
        let weak = cx.entity().downgrade();
        let button = cx.new(|cx| AccountsButton::new(AccountsSource::server(), weak, true, cx));
        cx.default_global::<AccountsButtons>()
            .0
            .push(button.downgrade());
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_right_item(button.clone(), window, cx);
        });
        workspace.register_action(move |_, _: &RefreshAccounts, _, cx| {
            button.update(cx, |button, cx| button.reload(true, cx));
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::panel_window;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    const HOUR: u64 = 3_600_000;

    fn account(id: &str, managed: bool, default: bool) -> Account {
        Account {
            id: id.into(),
            provider: "claude".into(),
            label: format!("{id} label"),
            managed,
            default,
            email: managed.then(|| format!("{id}@example.com")),
            login_command: managed.then(|| format!("CLAUDE_CONFIG_DIR=/a/{id} claude /login")),
        }
    }

    fn window(key: &str, percent: f64, minutes: u64, resets_at_ms: Option<u64>) -> UsageWindow {
        UsageWindow {
            key: key.into(),
            used_percent: percent,
            window_minutes: minutes,
            resets_at_ms,
        }
    }

    fn claude(session: f64) -> ProviderUsage {
        ProviderUsage {
            provider: "claude".into(),
            plan: Some("Max".into()),
            windows: vec![
                window("session", session, 300, Some(4 * HOUR + 40 * 60_000)),
                window(
                    "weekly",
                    11.4,
                    10_080,
                    Some(6 * 24 * HOUR + 8 * HOUR + 59_000),
                ),
                window("fable", 0.0, 10_080, Some(6 * 24 * HOUR)),
            ],
            failure: None,
            error: None,
            updated_at_ms: Some(0),
        }
    }

    fn failed(provider: &str, failure: &str) -> ProviderUsage {
        ProviderUsage {
            provider: provider.into(),
            plan: None,
            windows: Vec::new(),
            failure: Some(failure.into()),
            error: Some("why".into()),
            updated_at_ms: None,
        }
    }

    #[test]
    fn resets_and_window_labels_read_like_orcas() {
        assert_eq!(format_reset(0), "now");
        assert_eq!(format_reset(59_999), "0m");
        assert_eq!(format_reset(45 * 60_000), "45m");
        assert_eq!(format_reset(3 * HOUR as i64), "3h");
        assert_eq!(format_reset((4 * HOUR + 40 * 60_000) as i64), "4h 40m");
        assert_eq!(
            format_reset((6 * 24 * HOUR + 8 * HOUR + 59_000) as i64),
            "6d 8h"
        );
        assert_eq!(format_reset((2 * 24 * HOUR) as i64), "2d");
        assert_eq!(resets_in(10, 20), "Resets now");
        assert_eq!(resets_in(HOUR, 0), "Resets in 1h");
        assert_eq!(window_label(10_080), "wk");
        assert_eq!(window_label(300), "5h");
        assert_eq!(window_label(43_200), "30d");
        assert_eq!(window_label(30), "30m");
        assert_eq!(format_time_ago(30_000), "just now");
        assert_eq!(format_time_ago(5 * 60_000), "5m ago");
        assert_eq!(format_time_ago(3 * HOUR), "3h ago");
    }

    #[test]
    fn the_strip_shows_each_window_with_its_reset_and_fable_by_name() {
        let usage = claude(2.0);
        assert_eq!(
            usage.strip_sections(0, false),
            [
                (2, "4h 40m".to_owned()),
                (11, "6d 8h".to_owned()),
                (0, "Fable".to_owned())
            ]
        );
        assert_eq!(usage.strip_sections(0, true), [(11, "6d 8h".to_owned())]);
        assert_eq!(usage.headline().map(|w| w.key.as_str()), Some("weekly"));
    }

    #[test]
    fn providers_without_a_login_stay_hidden_and_the_roster_sorts_by_use() {
        let expired = failed("codex", "expired");
        assert!(expired.worth_showing());
        assert_eq!(expired.status_label(), "Sign-in expired");
        assert!(!failed("codex", "no_login").worth_showing());

        let usage = [failed("codex", "no_login"), expired, claude(2.0)];
        let order: Vec<&str> = roster_order(&usage)
            .iter()
            .map(|u| u.provider.as_str())
            .collect();
        assert_eq!(order, ["claude", "codex"]);
    }

    fn source(
        usage: Rc<RefCell<Vec<ProviderUsage>>>,
        forced: Rc<Cell<usize>>,
        selected: Rc<RefCell<Vec<(String, String)>>>,
    ) -> AccountsSource {
        AccountsSource {
            list: Rc::new(|_| {
                Task::ready(Ok(vec![
                    account("system", false, true),
                    account("work", true, false),
                ]))
            }),
            usage: Rc::new(move |force, _| {
                if force {
                    forced.set(forced.get() + 1);
                }
                Task::ready(Ok(usage.borrow().clone()))
            }),
            select: Rc::new(move |provider, id, _| {
                selected.borrow_mut().push((provider, id));
                Task::ready(Ok(()))
            }),
            add: Rc::new(|_, label, _| {
                let mut added = account("new", true, false);
                added.label = label;
                Task::ready(Ok(added))
            }),
        }
    }

    #[gpui::test]
    async fn switching_reasks_the_provider_and_adding_shows_the_login_command(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        let selected: Rc<RefCell<Vec<(String, String)>>> = Rc::default();
        let forced = Rc::new(Cell::new(0));
        let usage = Rc::new(RefCell::new(vec![claude(2.0)]));
        let source = source(usage, forced.clone(), selected.clone());
        let weak = workspace.downgrade();
        let button = cx.new(|cx| AccountsButton::new(source, weak, false, cx));
        button.update(&mut cx, |button, cx| button.reload(false, cx));
        cx.run_until_parked();
        button.read_with(&cx, |button, _| {
            assert_eq!(button.accounts().len(), 2);
            assert_eq!(button.usage().len(), 1);
        });

        button.update(&mut cx, |button, cx| {
            button.select("claude".into(), "work".into(), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            selected.borrow().as_slice(),
            [("claude".to_owned(), "work".to_owned())]
        );
        assert_eq!(
            forced.get(),
            1,
            "a new default account's usage is asked for at once"
        );

        let before = workspace.read_with(&cx, |workspace, _| workspace.notification_ids().len());
        button.update(&mut cx, |button, cx| button.add("claude".into(), cx));
        cx.run_until_parked();
        assert_eq!(
            workspace.read_with(&cx, |workspace, _| workspace.notification_ids().len()),
            before + 1,
            "adding shows the command that signs the account in"
        );
    }

    #[gpui::test]
    async fn the_popover_reloads_on_open_and_follows_the_view_mode(cx: &mut gpui::TestAppContext) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        let usage = Rc::new(RefCell::new(vec![claude(65.0)]));
        let source = source(usage.clone(), Rc::default(), Rc::default());
        let weak = workspace.downgrade();
        let button = cx.new(|cx| AccountsButton::new(source, weak, false, cx));
        button.update(&mut cx, |button, cx| button.reload(false, cx));
        cx.run_until_parked();

        usage.borrow_mut()[0].windows[0].used_percent = 7.0;
        let menu = button.update_in(&mut cx, |button, window, cx| button.menu(window, cx));
        cx.run_until_parked();
        button.read_with(&cx, |button, _| {
            assert_eq!(button.shown.borrow().usage[0].windows[0].percent(), 7);
            assert!(!button.shown.borrow().compact);
        });

        button.update(&mut cx, |button, cx| button.set_compact(true, cx));
        cx.run_until_parked();
        button.read_with(&cx, |button, _| assert!(button.shown.borrow().compact));
        drop(menu);
    }
}
