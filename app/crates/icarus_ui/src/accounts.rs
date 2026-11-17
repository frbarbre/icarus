//! Agent accounts in the status bar: the default account per provider with its usage meter, and a
//! menu to switch the default or add an account. Icarus never touches credentials: adding one shows
//! the provider's own login command to run.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use gpui::{App, AppContext as _, Context, Entity, IntoElement, Render, Task, Window, actions};
use gpui_tokio::Tokio;
use serde::Deserialize;
use serde_json::{Value, json};
use ui::{ContextMenu, PopoverMenu, Tooltip, prelude::*};
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
/// Usage older than this says how old it is, since it only updates when an agent's turn reports it.
const STALE_AFTER_MS: u64 = 10 * 60 * 1000;

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
    #[serde(default)]
    pub usage: Value,
}

/// One rate-limit window's use, 0–100.
#[derive(Debug, Clone, PartialEq)]
pub struct Meter {
    pub window: &'static str,
    pub percent: u32,
    /// The window reset after the usage was seen, so its use since is unknown.
    pub expired: bool,
}

impl Meter {
    pub fn text(&self) -> String {
        if self.expired {
            format!("{} reset", self.window)
        } else {
            format!("{} {}%", self.window, self.percent)
        }
    }
}

impl Account {
    /// Claude Code's `unifiedWindows` (five_hour, seven_day) as percentages.
    pub fn meters(&self) -> Vec<Meter> {
        let windows = &self.usage["rate_limits"]["unifiedWindows"];
        [("five_hour", "5h"), ("seven_day", "7d")]
            .into_iter()
            .filter_map(|(key, short)| {
                let utilization = windows[key]["utilization"].as_f64()?;
                Some(Meter {
                    window: short,
                    percent: (utilization * 100.0).round().clamp(0.0, 100.0) as u32,
                    expired: windows[key]["expired"].as_bool().unwrap_or_default(),
                })
            })
            .collect()
    }

    /// How old the usage is, when it's old enough to say so.
    pub fn usage_age(&self) -> Option<String> {
        let age_ms = self.usage["age_ms"].as_u64()?;
        (age_ms >= STALE_AFTER_MS)
            .then(|| crate::github_panel::format_relative((age_ms / 1000) as i64))
    }

    fn meters_text(&self) -> String {
        let mut parts: Vec<String> = self.meters().iter().map(Meter::text).collect();
        if !parts.is_empty()
            && let Some(age) = self.usage_age()
        {
            parts.push(age);
        }
        parts.join(" · ")
    }

    fn display_name(&self) -> String {
        if self.managed {
            self.email.clone().unwrap_or_else(|| self.label.clone())
        } else {
            "System login".to_owned()
        }
    }
}

pub(crate) fn provider_name(provider: &str) -> &str {
    match provider {
        "claude" => "Claude",
        "codex" => "Codex",
        other => other,
    }
}

/// The status item stays out of the way for a single login with nothing to show.
pub fn worth_showing(accounts: &[Account]) -> bool {
    accounts.iter().any(|account| account.managed)
        || accounts
            .iter()
            .any(|account| account.default && !account.meters().is_empty())
}

/// What the status bar says: the default account's provider and its fullest window, e.g. "Claude 61%".
pub fn summary(accounts: &[Account]) -> Option<String> {
    let account = accounts
        .iter()
        .filter(|account| account.default)
        .max_by_key(|account| account.meters().iter().map(|m| m.percent).max())?;
    let peak = account.meters().into_iter().max_by_key(|m| m.percent);
    let name = provider_name(&account.provider);
    Some(match (peak, account.managed) {
        (Some(meter), _) => format!("{name} {}%", meter.percent),
        (None, true) => format!("{name}: {}", account.display_name()),
        (None, false) => name.to_owned(),
    })
}

#[derive(Clone)]
pub struct AccountsSource {
    pub list: Rc<dyn Fn(&mut App) -> Task<Result<Vec<Account>>>>,
    pub select: Rc<dyn Fn(String, String, &mut App) -> Task<Result<()>>>,
    pub add: Rc<dyn Fn(String, String, &mut App) -> Task<Result<Account>>>,
}

impl AccountsSource {
    pub fn server() -> Self {
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
            select: Rc::new(|provider, id, cx| {
                let Some(client) = app_client(cx) else {
                    return Task::ready(Err(anyhow::anyhow!("not connected to the Icarus server")));
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
                    return Task::ready(Err(anyhow::anyhow!("not connected to the Icarus server")));
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

pub struct AccountsButton {
    source: AccountsSource,
    accounts: Vec<Account>,
    /// What an open menu lists; the persistent menu's builder reads it when it rebuilds.
    shown: Rc<RefCell<Vec<Account>>>,
    workspace: gpui::WeakEntity<Workspace>,
    _poll: Task<()>,
}

impl AccountsButton {
    pub fn new(
        source: AccountsSource,
        workspace: gpui::WeakEntity<Workspace>,
        poll: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut button = Self {
            source,
            accounts: Vec::new(),
            shown: Rc::default(),
            workspace,
            _poll: Task::ready(()),
        };
        button._poll = if poll {
            cx.spawn(async move |this, cx| {
                loop {
                    if this.update(cx, |this, cx| this.reload(cx)).is_err() {
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

    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let list = (self.source.list)(cx);
        cx.spawn(async move |this, cx| {
            let Some(accounts) = list.await.log_err() else {
                return;
            };
            this.update(cx, |this, cx| this.set_accounts(accounts, cx))
                .log_err();
        })
        .detach();
    }

    pub fn select(&mut self, provider: String, id: String, cx: &mut Context<Self>) {
        let select = (self.source.select)(provider, id, cx);
        cx.spawn(async move |this, cx| {
            select.await.log_err();
            this.update(cx, |this, cx| this.reload(cx)).log_err();
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
                this.reload(cx);
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

    /// Opens with the accounts already known, then reloads them and rebuilds the open menu, so the
    /// usage it shows is current rather than up to a poll interval old.
    fn menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<ContextMenu> {
        *self.shown.borrow_mut() = self.accounts.clone();
        let shown = self.shown.clone();
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build_persistent(window, cx, move |menu, _, _| {
            Self::menu_items(
                menu.keep_open_on_confirm(false),
                &shown.borrow(),
                this.clone(),
            )
        });
        let list = (self.source.list)(cx);
        let open_menu = menu.downgrade();
        cx.spawn_in(window, async move |this, cx| {
            let accounts = list.await?;
            this.update(cx, |this, cx| this.set_accounts(accounts, cx))?;
            if let Some(menu) = open_menu.upgrade() {
                menu.update_in(cx, |menu, window, cx| menu.rebuild(window, cx))?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
        menu
    }

    fn set_accounts(&mut self, accounts: Vec<Account>, cx: &mut Context<Self>) {
        *self.shown.borrow_mut() = accounts.clone();
        self.accounts = accounts;
        cx.notify();
    }

    fn menu_items(
        mut menu: ContextMenu,
        accounts: &[Account],
        this: gpui::WeakEntity<Self>,
    ) -> ContextMenu {
        let mut providers: Vec<String> = Vec::new();
        for account in accounts {
            if !providers.contains(&account.provider) {
                providers.push(account.provider.clone());
            }
        }
        for (index, provider) in providers.iter().enumerate() {
            if index > 0 {
                menu = menu.separator();
            }
            menu = menu.header(provider_name(provider).to_owned());
            for account in accounts.iter().filter(|a| &a.provider == provider) {
                let meters = account.meters_text();
                let label = if meters.is_empty() {
                    account.display_name()
                } else {
                    format!("{} — {meters}", account.display_name())
                };
                let this = this.clone();
                let provider = account.provider.clone();
                let id = account.id.clone();
                menu = menu.toggleable_entry(
                    label,
                    account.default,
                    ui::IconPosition::Start,
                    None,
                    move |_, cx| {
                        this.update(cx, |this, cx| this.select(provider.clone(), id.clone(), cx))
                            .log_err();
                    },
                );
            }
            let this = this.clone();
            let provider = provider.clone();
            menu = menu.entry(
                format!("Add a {} account…", provider_name(&provider)),
                None,
                move |_, cx| {
                    this.update(cx, |this, cx| this.add(provider.clone(), cx))
                        .log_err();
                },
            );
        }
        menu
    }
}

impl Render for AccountsButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !worth_showing(&self.accounts) {
            return div().into_any_element();
        }
        let Some(summary) = summary(&self.accounts) else {
            return div().into_any_element();
        };
        let tooltip = self
            .accounts
            .iter()
            .filter(|account| account.default)
            .map(|account| {
                let meters = account.meters_text();
                format!(
                    "{} ({}){}",
                    provider_name(&account.provider),
                    account.display_name(),
                    if meters.is_empty() {
                        String::new()
                    } else {
                        format!(": {meters}")
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let this = cx.entity();
        PopoverMenu::new("icarus-accounts")
            .menu(move |window, cx| Some(this.update(cx, |this, cx| this.menu(window, cx))))
            .anchor(gpui::Anchor::BottomRight)
            .trigger_with_tooltip(
                Button::new("icarus-accounts-button", summary).label_size(LabelSize::Small),
                Tooltip::text(tooltip),
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
struct AccountsButtons(Vec<gpui::WeakEntity<AccountsButton>>);

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
        button.update(cx, |button, cx| button.reload(cx));
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
        let weak = cx.entity().downgrade();
        let button = cx.new(|cx| AccountsButton::new(AccountsSource::server(), weak, true, cx));
        cx.default_global::<AccountsButtons>()
            .0
            .push(button.downgrade());
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_right_item(button.clone(), window, cx);
        });
        workspace.register_action(move |_, _: &RefreshAccounts, _, cx| {
            button.update(cx, |button, cx| button.reload(cx));
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::panel_window;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn account(id: &str, managed: bool, default: bool, usage: Value) -> Account {
        Account {
            id: id.into(),
            provider: "claude".into(),
            label: format!("{id} label"),
            managed,
            default,
            email: managed.then(|| format!("{id}@example.com")),
            login_command: managed.then(|| format!("CLAUDE_CONFIG_DIR=/a/{id} claude /login")),
            usage,
        }
    }

    fn usage(five_hour: f64, seven_day: f64) -> Value {
        json!({ "rate_limits": { "unifiedWindows": {
            "five_hour": { "utilization": five_hour },
            "seven_day": { "utilization": seven_day },
        } } })
    }

    #[test]
    fn a_single_login_stays_hidden_until_it_has_usage_to_show() {
        let plain = [account("system", false, true, Value::Null)];
        assert!(!worth_showing(&plain));
        let metered = [account("system", false, true, usage(0.17, 0.61))];
        assert!(worth_showing(&metered));
        assert_eq!(summary(&metered).as_deref(), Some("Claude 61%"));
        assert_eq!(
            metered[0].meters(),
            [
                Meter {
                    window: "5h",
                    percent: 17,
                    expired: false,
                },
                Meter {
                    window: "7d",
                    percent: 61,
                    expired: false,
                }
            ]
        );
        let two = [
            account("system", false, false, Value::Null),
            account("work", true, true, Value::Null),
        ];
        assert!(worth_showing(&two), "a second account always shows");
        assert_eq!(summary(&two).as_deref(), Some("Claude: work@example.com"));
    }

    #[gpui::test]
    async fn the_menu_switches_the_default_and_adding_shows_the_login_command(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        let selected: Rc<RefCell<Vec<(String, String)>>> = Rc::default();
        let source = AccountsSource {
            list: Rc::new(|_| {
                Task::ready(Ok(vec![
                    account("system", false, true, usage(0.1, 0.2)),
                    account("work", true, false, Value::Null),
                ]))
            }),
            select: Rc::new({
                let selected = selected.clone();
                move |provider, id, _| {
                    selected.borrow_mut().push((provider, id));
                    Task::ready(Ok(()))
                }
            }),
            add: Rc::new(|_, label, _| {
                let mut added = account("new", true, false, Value::Null);
                added.label = label;
                Task::ready(Ok(added))
            }),
        };
        let weak = workspace.downgrade();
        let button = cx.new(|cx| AccountsButton::new(source, weak, false, cx));
        button.update(&mut cx, |button, cx| button.reload(cx));
        cx.run_until_parked();
        button.read_with(&cx, |button, _| assert_eq!(button.accounts().len(), 2));

        button.update(&mut cx, |button, cx| {
            button.select("claude".into(), "work".into(), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            selected.borrow().as_slice(),
            [("claude".to_owned(), "work".to_owned())]
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

    #[test]
    fn a_window_that_reset_since_and_old_usage_say_so() {
        let mut usage = usage(0.65, 0.45);
        usage["rate_limits"]["unifiedWindows"]["five_hour"] =
            json!({ "utilization": 0.0, "expired": true });
        usage["age_ms"] = json!(3 * 3600 * 1000);
        let stale = account("system", false, true, usage.clone());
        assert_eq!(stale.meters_text(), "5h reset · 7d 45% · 3h ago");
        assert_eq!(summary(&[stale]).as_deref(), Some("Claude 45%"));

        usage["age_ms"] = json!(60 * 1000);
        let fresh = account("system", false, true, usage);
        assert_eq!(
            fresh.meters_text(),
            "5h reset · 7d 45%",
            "recent usage needs no age"
        );
    }

    #[gpui::test]
    async fn opening_the_menu_reloads_the_usage(cx: &mut gpui::TestAppContext) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        let five_hour = Rc::new(RefCell::new(0.65));
        let source = AccountsSource {
            list: Rc::new({
                let five_hour = five_hour.clone();
                move |_| {
                    Task::ready(Ok(vec![account(
                        "system",
                        false,
                        true,
                        usage(*five_hour.borrow(), 0.45),
                    )]))
                }
            }),
            select: Rc::new(|_, _, _| Task::ready(Ok(()))),
            add: Rc::new(|_, _, _| Task::ready(Err(anyhow::anyhow!("unused")))),
        };
        let weak = workspace.downgrade();
        let button = cx.new(|cx| AccountsButton::new(source, weak, false, cx));
        button.update(&mut cx, |button, cx| button.reload(cx));
        cx.run_until_parked();
        assert_eq!(
            button.read_with(&cx, |button, _| summary(button.accounts())),
            Some("Claude 65%".into())
        );

        *five_hour.borrow_mut() = 0.07;
        let _menu = button.update_in(&mut cx, |button, window, cx| button.menu(window, cx));
        cx.run_until_parked();
        assert_eq!(
            button.read_with(&cx, |button, _| summary(button.accounts())),
            Some("Claude 45%".into())
        );
        assert_eq!(
            button.read_with(&cx, |button, _| button.shown.borrow()[0].meters_text()),
            "5h 7% · 7d 45%"
        );
    }
}
