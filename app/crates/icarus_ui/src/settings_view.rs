//! Icarus's own settings as a tab (plan §4, features.md §2.17). Everything goes through the server's
//! `config.*` RPCs, so the user, project and project-local layers stay authoritative and each row
//! says which layer its value comes from. Complex values open the layer's JSON file instead.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, SharedString, Styled, Subscription, Task, WeakEntity, Window, actions,
    div,
};
use icarus_client::Client;
use serde_json::{Value, json};
use ui::{
    ContextMenu, IconButton, IconName, Label, LabelSize, PopoverMenu, Switch, ToggleButtonGroup,
    ToggleButtonSimple, ToggleState, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::item::{Item, ItemEvent};
use workspace::{OpenOptions, Workspace};

use crate::cards::CARD_FIELDS;
use crate::model::StatusColumn;
use crate::review::{app_client, request};
use crate::status_style::{STATUS_COLORS, STATUS_ICONS, status_icon, swatch_color};

actions!(
    icarus,
    [
        /// Opens Icarus's own settings: statuses, rules, agents, integrations and more.
        OpenSettings
    ]
);

/// The layer edits go to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
    Local,
}

impl Scope {
    fn layer(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
            Scope::Local => "local",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Control {
    Toggle,
    Choice(&'static [&'static str]),
    Text,
    Integer,
}

struct Field {
    key: &'static str,
    label: &'static str,
    help: &'static str,
    control: Control,
}

const fn field(
    key: &'static str,
    label: &'static str,
    help: &'static str,
    control: Control,
) -> Field {
    Field {
        key,
        label,
        help,
        control,
    }
}

const AGENTS: &[Field] = &[
    field(
        "agents.default",
        "Default agent",
        "Provider for new agent sessions.",
        Control::Choice(&["claude", "codex"]),
    ),
    field(
        "agents.remote_control",
        "Remote Control for new agents",
        "Start Claude Code sessions with Remote Control on.",
        Control::Toggle,
    ),
];

const AUTOMATION: &[Field] = &[
    field(
        "rules.enabled",
        "Run the automation loop",
        "Poll pull requests and plan rule actions.",
        Control::Toggle,
    ),
    field(
        "rules.armed",
        "Act on plans",
        "Until on, every tick is a dry run.",
        Control::Toggle,
    ),
    field(
        "rules.interval_ms",
        "Polling interval (ms)",
        "How often the rules run.",
        Control::Integer,
    ),
    field(
        "rules.orca_compat.enabled",
        "Move cards by pull request state",
        "The status rules below.",
        Control::Toggle,
    ),
];

const INTEGRATIONS: &[Field] = &[
    field(
        "linear.api_key_command",
        "Linear key command",
        "A command that prints your Linear API key; Icarus never stores the key.",
        Control::Text,
    ),
    field(
        "linear.poll_interval_ms",
        "Linear refresh (ms)",
        "How often linked issues refresh.",
        Control::Integer,
    ),
];

const NOTIFICATIONS: &[Field] = &[
    field(
        "notifications.enabled",
        "Notifications",
        "Show the server's notifications.",
        Control::Toggle,
    ),
    field(
        "notifications.sound",
        "Sound",
        "Play a sound with macOS banners.",
        Control::Toggle,
    ),
];

const PORTS: &[Field] = &[
    field(
        "ports.labels_command",
        "Port labels command",
        "Prints `port label` lines for a workspace's services.",
        Control::Text,
    ),
    field(
        "previews.primary_app",
        "Primary preview",
        "Which preview ⌥⌘P opens (e.g. frontend).",
        Control::Text,
    ),
    field(
        "previews.environment_pattern",
        "Preview environments",
        "Deployment environments that count as previews (regex).",
        Control::Text,
    ),
];

const UPDATES: &[Field] = &[field(
    "updates.channel",
    "Update channel",
    "Where Icarus looks for new versions.",
    Control::Choice(&["stable", "none"]),
)];

/// Orca's PR conditions, in the order its settings list them.
pub const CONDITIONS: &[(&str, &str)] = &[
    ("merged", "Merged"),
    ("closed", "Closed"),
    ("reviewing", "Reviewing"),
    ("merge-queue", "In the merge queue"),
    ("conflicts", "Has conflicts"),
    ("draft", "Draft"),
    ("changes-requested", "Changes requested"),
    ("merging", "Merging"),
    ("pm-approval", "Waiting for PM approval"),
    ("review", "In review"),
];

const STATUS_BY_CONDITION: &str = "rules.orca_compat.statusByCondition";

#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub layer: String,
    pub path: Option<String>,
}

/// What the view shows, loaded in one go.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub resolved: Value,
    pub sources: HashMap<String, Source>,
    pub kinds: Vec<(String, bool)>,
    pub layouts: Vec<String>,
    pub paths: Value,
}

/// `a.b.c` in a JSON tree.
pub fn lookup<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.')
        .try_fold(value, |value, part| value.get(part))
        .filter(|value| !value.is_null())
}

pub fn slug(label: &str) -> String {
    let mut out = String::new();
    for c in label.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
}

pub struct SettingsView {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    client: Option<Arc<Client>>,
    root: Option<PathBuf>,
    project: Option<String>,
    scope: Scope,
    snapshot: Option<Snapshot>,
    loading: Option<Task<()>>,
    error: Option<String>,
    gh: Option<String>,
    editors: HashMap<&'static str, Entity<Editor>>,
    status_editor: Entity<Editor>,
    renaming: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ItemEvent> for SettingsView {}

impl Focusable for SettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SettingsView {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        root: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut editors = HashMap::new();
        let mut subscriptions = Vec::new();
        for field in [AUTOMATION, INTEGRATIONS, PORTS]
            .into_iter()
            .flatten()
            .filter(|f| matches!(f.control, Control::Text | Control::Integer))
        {
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Not set", window, cx);
                editor
            });
            let key = field.key;
            let integer = field.control == Control::Integer;
            subscriptions.push(cx.subscribe_in(
                &editor,
                window,
                move |this, editor, event, window, cx| {
                    if matches!(event, EditorEvent::Blurred) {
                        let text = editor.read(cx).text(cx);
                        this.commit_text(key, text.trim(), integer, window, cx);
                    }
                },
            ));
            editors.insert(key, editor);
        }
        let status_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("New status name", window, cx);
            editor
        });
        let mut view = Self {
            workspace,
            focus_handle: cx.focus_handle(),
            client: app_client(cx),
            root,
            project: None,
            scope: Scope::User,
            snapshot: None,
            loading: None,
            error: None,
            gh: None,
            editors,
            status_editor,
            renaming: None,
            _subscriptions: subscriptions,
        };
        view.reload(window, cx);
        view
    }

    /// The view filled from a fixed snapshot, for screenshots and tests.
    pub fn preview(snapshot: Snapshot, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = Self::new(WeakEntity::new_invalid(), None, window, cx);
        view.loading = None;
        view.apply(snapshot, window, cx);
        view
    }

    pub fn scope(&self) -> Scope {
        self.scope
    }

    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let root = self.root.clone();
        let keys: Vec<&'static str> = [
            AGENTS,
            AUTOMATION,
            INTEGRATIONS,
            NOTIFICATIONS,
            PORTS,
            UPDATES,
        ]
        .into_iter()
        .flatten()
        .map(|f| f.key)
        .chain([
            "workspace.statuses",
            "workspace.cardFields",
            STATUS_BY_CONDITION,
        ])
        .collect();
        let project = self.project.clone();
        let workspaces = request(client.clone(), "workspace.list", json!({}), cx);
        self.loading = Some(cx.spawn_in(window, async move |this, cx| {
            let project = match (project, workspaces, root) {
                (Some(project), _, _) => Some(project),
                (None, Some(list), Some(root)) => list.await.ok().and_then(|list| {
                    list.as_array()?
                        .iter()
                        .find(|w| w["path"].as_str().map(PathBuf::from).as_ref() == Some(&root))
                        .and_then(|w| w["project_id"].as_str().map(str::to_owned))
                }),
                _ => None,
            };
            let loaded: Result<Snapshot> = async {
                let call =
                    |method: &'static str, params: Value, cx: &mut gpui::AsyncWindowContext| {
                        cx.update(|_, cx| request(client.clone(), method, params, cx))
                            .ok()
                            .flatten()
                    };
                let scope = json!({ "project": project });
                let resolved = match call("config.get", scope.clone(), cx) {
                    Some(task) => task.await?,
                    None => anyhow::bail!("not connected to the Icarus server"),
                };
                let mut snapshot = Snapshot {
                    resolved,
                    ..Snapshot::default()
                };
                if let Some(task) = call("notification.kinds", json!({}), cx) {
                    let kinds = task.await.unwrap_or_default();
                    snapshot.kinds = kinds["kinds"]
                        .as_object()
                        .map(|kinds| {
                            kinds
                                .iter()
                                .map(|(k, v)| (k.clone(), v.as_bool().unwrap_or(true)))
                                .collect()
                        })
                        .unwrap_or_default();
                }
                if let Some(task) = call("layout.list", scope.clone(), cx) {
                    snapshot.layouts = task
                        .await
                        .ok()
                        .and_then(|l| l.as_object().map(|l| l.keys().cloned().collect()))
                        .unwrap_or_default();
                }
                if let Some(task) = call("config.paths", scope.clone(), cx) {
                    snapshot.paths = task.await.unwrap_or_default();
                }
                for key in keys {
                    let params = json!({ "key": key, "project": project });
                    if let Some(task) = call("config.explain", params, cx)
                        && let Ok(explained) = task.await
                        && let Some(layer) = explained["from"]["layer"].as_str()
                    {
                        snapshot.sources.insert(
                            key.to_owned(),
                            Source {
                                layer: layer.to_owned(),
                                path: explained["from"]["path"].as_str().map(str::to_owned),
                            },
                        );
                    }
                }
                Ok(snapshot)
            }
            .await;
            this.update_in(cx, |this, window, cx| {
                this.loading = None;
                this.project = project;
                match loaded {
                    Ok(snapshot) => this.apply(snapshot, window, cx),
                    Err(error) => this.error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn apply(&mut self, snapshot: Snapshot, window: &mut Window, cx: &mut Context<Self>) {
        for (key, editor) in &self.editors {
            let text = match lookup(&snapshot.resolved, key) {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Number(number)) => number.to_string(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => String::new(),
            };
            editor.update(cx, |editor, cx| {
                if editor.text(cx) != text {
                    editor.set_text(text, window, cx);
                }
            });
        }
        self.snapshot = Some(snapshot);
        cx.notify();
    }

    fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        self.scope = scope;
        cx.notify();
    }

    /// Writes `value` to the chosen layer (or removes the key there with `None`), then reloads.
    pub fn write(
        &mut self,
        key: &str,
        value: Option<Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let method = if value.is_some() {
            "config.set"
        } else {
            "config.unset"
        };
        let mut params = json!({ "key": key, "layer": self.scope.layer() });
        if let Some(value) = value {
            params["value"] = value;
        }
        if self.scope != Scope::User {
            params["project"] = json!(self.project);
        }
        let Some(task) = request(self.client.clone(), method, params, cx) else {
            self.error = Some("Not connected to the Icarus server.".into());
            cx.notify();
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(_) => {
                        this.error = None;
                        this.reload(window, cx);
                    }
                    Err(error) => this.error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn commit_text(
        &mut self,
        key: &'static str,
        text: &str,
        integer: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .snapshot
            .as_ref()
            .and_then(|s| lookup(&s.resolved, key))
            .cloned();
        let value = if text.is_empty() {
            None
        } else if integer {
            match text.parse::<u64>() {
                Ok(n) => Some(json!(n)),
                Err(_) => {
                    self.error = Some(format!("{key} needs a whole number"));
                    cx.notify();
                    return;
                }
            }
        } else {
            Some(json!(text))
        };
        let same = match (&value, &current) {
            (None, None) => true,
            (Some(new), Some(Value::Array(_))) => {
                Some(new.as_str().unwrap_or_default())
                    == current
                        .as_ref()
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .as_deref()
            }
            (Some(new), Some(old)) => new == old,
            _ => false,
        };
        if !same {
            self.write(key, value, window, cx);
        }
    }

    fn statuses(&self) -> Vec<StatusColumn> {
        self.snapshot
            .as_ref()
            .and_then(|s| lookup(&s.resolved, "workspace.statuses"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    fn write_statuses(
        &mut self,
        statuses: &[StatusColumn],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = Value::Array(
            statuses
                .iter()
                .map(|s| {
                    let mut entry = json!({ "id": s.id, "label": s.label });
                    if let Some(color) = &s.color {
                        entry["color"] = json!(color);
                    }
                    if let Some(icon) = &s.icon {
                        entry["icon"] = json!(icon);
                    }
                    entry
                })
                .collect(),
        );
        self.write("workspace.statuses", Some(value), window, cx);
    }

    pub fn add_or_rename_status(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let label = self.status_editor.read(cx).text(cx).trim().to_owned();
        if label.is_empty() {
            return;
        }
        let mut statuses = self.statuses();
        match self.renaming.take() {
            Some(id) => {
                if let Some(status) = statuses.iter_mut().find(|s| s.id == id) {
                    status.label = label;
                }
            }
            None => {
                let id = slug(&label);
                if id.is_empty() || statuses.iter().any(|s| s.id == id) {
                    self.error = Some(format!("A status named \"{label}\" already exists."));
                    cx.notify();
                    return;
                }
                statuses.push(StatusColumn {
                    id,
                    label,
                    color: None,
                    icon: None,
                });
            }
        }
        self.status_editor
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.write_statuses(&statuses, window, cx);
    }

    fn move_status(&mut self, id: &str, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        let mut statuses = self.statuses();
        let Some(index) = statuses.iter().position(|s| s.id == id) else {
            return;
        };
        let target = index as isize + by;
        if target < 0 || target as usize >= statuses.len() {
            return;
        }
        statuses.swap(index, target as usize);
        self.write_statuses(&statuses, window, cx);
    }

    fn remove_status(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let mut statuses = self.statuses();
        statuses.retain(|s| s.id != id);
        self.write_statuses(&statuses, window, cx);
    }

    fn restyle_status(
        &mut self,
        id: &str,
        color: Option<&str>,
        icon: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut statuses = self.statuses();
        if let Some(status) = statuses.iter_mut().find(|s| s.id == id) {
            if let Some(color) = color {
                status.color = Some(color.to_owned());
            }
            if let Some(icon) = icon {
                status.icon = Some(icon.to_owned());
            }
        }
        self.write_statuses(&statuses, window, cx);
    }

    pub fn toggle_card_field(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let mut fields: Vec<String> = self
            .snapshot
            .as_ref()
            .and_then(|s| lookup(&s.resolved, "workspace.cardFields"))
            .and_then(Value::as_array)
            .map(|f| {
                f.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if let Some(index) = fields.iter().position(|f| f == id) {
            fields.remove(index);
        } else {
            fields.push(id.to_owned());
        }
        self.write("workspace.cardFields", Some(json!(fields)), window, cx);
    }

    fn open_layer_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.scope.layer();
        let Some(path) = self
            .snapshot
            .as_ref()
            .and_then(|s| s.paths[key].as_str())
            .map(PathBuf::from)
        else {
            self.error = Some("This layer has no file here (pick a project first).".into());
            cx.notify();
            return;
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).log_err();
        }
        if !path.exists() {
            std::fs::write(&path, "{\n}\n").log_err();
        }
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_abs_path(path, OpenOptions::default(), window, cx)
                    .detach_and_log_err(cx);
            })
            .log_err();
    }

    fn check_gh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.gh = Some("Checking…".into());
        cx.notify();
        let task = cx.background_spawn(async move {
            match util::command::new_command("gh")
                .args(["auth", "status"])
                .output()
                .await
            {
                Ok(output) => {
                    let text = format!(
                        "{}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    let first = text
                        .lines()
                        .map(str::trim)
                        .find(|l| l.contains("Logged in") || l.contains("not logged"))
                        .unwrap_or(if output.status.success() {
                            "Signed in."
                        } else {
                            "Not signed in: run `gh auth login`."
                        })
                        .trim_start_matches(['✓', 'X', ' '])
                        .to_owned();
                    first
                }
                Err(_) => "The GitHub CLI (gh) isn't installed.".to_owned(),
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let text = task.await;
            this.update(cx, |this, cx| {
                this.gh = Some(text);
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn source_label(&self, key: &str) -> Option<SharedString> {
        let source = self.snapshot.as_ref()?.sources.get(key)?;
        Some(match source.layer.as_str() {
            "user" => "your settings".into(),
            "project" => "project (icarus.json)".into(),
            "local" | "project_local" => "project, this machine".into(),
            "preset" => "preset".into(),
            "default" | "defaults" => return None,
            other => other.to_owned().into(),
        })
    }

    fn set_here(&self, key: &str) -> bool {
        let Some(source) = self.snapshot.as_ref().and_then(|s| s.sources.get(key)) else {
            return false;
        };
        match self.scope {
            Scope::User => source.layer == "user",
            Scope::Project => source.layer == "project",
            Scope::Local => source.layer == "local" || source.layer == "project_local",
        }
    }

    fn render_row(&self, field: &Field, cx: &mut Context<Self>) -> AnyElement {
        let value = self
            .snapshot
            .as_ref()
            .and_then(|s| lookup(&s.resolved, field.key))
            .cloned();
        let key = field.key;
        let weak = cx.entity().downgrade();
        let control: AnyElement = match field.control {
            Control::Toggle => {
                let on = value.as_ref().and_then(Value::as_bool).unwrap_or(false);
                Switch::new(SharedString::from(format!("switch-{key}")), on.into())
                    .on_click(move |state, window, cx| {
                        let on = *state == ToggleState::Selected;
                        weak.update(cx, |view, cx| view.write(key, Some(json!(on)), window, cx))
                            .log_err();
                    })
                    .into_any_element()
            }
            Control::Choice(options) => {
                let current: SharedString = value
                    .as_ref()
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    .to_owned()
                    .into();
                PopoverMenu::new(SharedString::from(format!("choice-{key}")))
                    .trigger(
                        ui::Button::new(
                            SharedString::from(format!("choice-button-{key}")),
                            current,
                        )
                        .label_size(LabelSize::Small)
                        .end_icon(ui::Icon::new(IconName::ChevronDown).size(ui::IconSize::XSmall)),
                    )
                    .menu(move |window, cx| {
                        let weak = weak.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for option in options {
                                let weak = weak.clone();
                                menu = menu.entry(*option, None, move |window, cx| {
                                    weak.update(cx, |view, cx| {
                                        view.write(key, Some(json!(option)), window, cx)
                                    })
                                    .log_err();
                                });
                            }
                            menu
                        }))
                    })
                    .into_any_element()
            }
            Control::Text | Control::Integer => match self.editors.get(key) {
                Some(editor) => div()
                    .w(px(260.))
                    .px_1p5()
                    .py_0p5()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(editor.clone())
                    .into_any_element(),
                None => div().into_any_element(),
            },
        };
        let reset = self.set_here(key).then(|| {
            let weak = cx.entity().downgrade();
            IconButton::new(
                SharedString::from(format!("reset-{key}")),
                IconName::RotateCcw,
            )
            .icon_size(ui::IconSize::XSmall)
            .tooltip(Tooltip::text("Remove from this layer"))
            .on_click(move |_, window, cx| {
                weak.update(cx, |view, cx| view.write(key, None, window, cx))
                    .log_err();
            })
        });
        h_flex()
            .w_full()
            .py_1p5()
            .gap_4()
            .justify_between()
            .child(
                v_flex().min_w_0().child(Label::new(field.label)).child(
                    Label::new(field.help)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_none()
                    .children(self.source_label(key).map(|source| {
                        Label::new(source)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                    }))
                    .children(reset)
                    .child(control),
            )
            .into_any_element()
    }

    fn section(title: &'static str, cx: &App) -> impl IntoElement {
        div()
            .pt_5()
            .pb_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Label::new(title).size(LabelSize::Small).color(Color::Muted))
    }

    fn render_statuses(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let statuses = self.statuses();
        let count = statuses.len();
        let weak = cx.entity().downgrade();
        v_flex()
            .children(statuses.into_iter().enumerate().map(|(index, status)| {
                let id = status.id.clone();
                let icon = status_icon(
                    &status.id,
                    status.color.as_deref(),
                    status.icon.as_deref(),
                    cx,
                );
                let row = |suffix: &str| SharedString::from(format!("status-{suffix}-{id}"));
                let (up, down, remove, rename) =
                    (weak.clone(), weak.clone(), weak.clone(), weak.clone());
                let (id_up, id_down, id_remove, id_rename) =
                    (id.clone(), id.clone(), id.clone(), id.clone());
                let label_text = status.label.clone();
                let colors = weak.clone();
                let icons = weak.clone();
                let (id_color, id_icon) = (id.clone(), id.clone());
                h_flex()
                    .py_1()
                    .gap_2()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(icon)
                            .child(Label::new(status.label.clone()))
                            .child(
                                Label::new(status.id)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                PopoverMenu::new(row("color"))
                                    .trigger(
                                        IconButton::new(row("color-button"), IconName::Circle)
                                            .icon_size(ui::IconSize::XSmall)
                                            .tooltip(Tooltip::text("Colour")),
                                    )
                                    .menu(move |window, cx| {
                                        let weak = colors.clone();
                                        let id = id_color.clone();
                                        Some(ContextMenu::build(
                                            window,
                                            cx,
                                            move |mut menu, _, _| {
                                                for color in STATUS_COLORS {
                                                    let weak = weak.clone();
                                                    let id = id.clone();
                                                    menu = menu.custom_entry(
                                                        move |_, cx| {
                                                            h_flex()
                                                                .gap_2()
                                                                .child(
                                                                    div()
                                                                        .size_2()
                                                                        .rounded_full()
                                                                        .bg(swatch_color(color.id)
                                                                            .color(cx)),
                                                                )
                                                                .child(Label::new(color.label))
                                                                .into_any_element()
                                                        },
                                                        move |window, cx| {
                                                            weak.update(cx, |view, cx| {
                                                                view.restyle_status(
                                                                    &id,
                                                                    Some(color.id),
                                                                    None,
                                                                    window,
                                                                    cx,
                                                                )
                                                            })
                                                            .log_err();
                                                        },
                                                    );
                                                }
                                                menu
                                            },
                                        ))
                                    }),
                            )
                            .child(
                                PopoverMenu::new(row("icon"))
                                    .trigger(
                                        IconButton::new(row("icon-button"), IconName::Sparkle)
                                            .icon_size(ui::IconSize::XSmall)
                                            .tooltip(Tooltip::text("Icon")),
                                    )
                                    .menu(move |window, cx| {
                                        let weak = icons.clone();
                                        let id = id_icon.clone();
                                        Some(ContextMenu::build(
                                            window,
                                            cx,
                                            move |mut menu, _, _| {
                                                for icon in STATUS_ICONS {
                                                    let weak = weak.clone();
                                                    let id = id.clone();
                                                    menu = menu.entry(
                                                        icon.label,
                                                        None,
                                                        move |window, cx| {
                                                            weak.update(cx, |view, cx| {
                                                                view.restyle_status(
                                                                    &id,
                                                                    None,
                                                                    Some(icon.id),
                                                                    window,
                                                                    cx,
                                                                )
                                                            })
                                                            .log_err();
                                                        },
                                                    );
                                                }
                                                menu
                                            },
                                        ))
                                    }),
                            )
                            .child(
                                IconButton::new(row("rename"), IconName::Pencil)
                                    .icon_size(ui::IconSize::XSmall)
                                    .tooltip(Tooltip::text("Rename"))
                                    .on_click(move |_, window, cx| {
                                        let label = label_text.clone();
                                        let id = id_rename.clone();
                                        rename
                                            .update(cx, |view, cx| {
                                                view.renaming = Some(id);
                                                view.status_editor.update(cx, |editor, cx| {
                                                    editor.set_text(label, window, cx)
                                                });
                                                window.focus(
                                                    &view.status_editor.focus_handle(cx),
                                                    cx,
                                                );
                                                cx.notify();
                                            })
                                            .log_err();
                                    }),
                            )
                            .child(
                                IconButton::new(row("up"), IconName::ArrowUp)
                                    .icon_size(ui::IconSize::XSmall)
                                    .disabled(index == 0)
                                    .on_click(move |_, window, cx| {
                                        up.update(cx, |view, cx| {
                                            view.move_status(&id_up, -1, window, cx)
                                        })
                                        .log_err();
                                    }),
                            )
                            .child(
                                IconButton::new(row("down"), IconName::ArrowDown)
                                    .icon_size(ui::IconSize::XSmall)
                                    .disabled(index + 1 == count)
                                    .on_click(move |_, window, cx| {
                                        down.update(cx, |view, cx| {
                                            view.move_status(&id_down, 1, window, cx)
                                        })
                                        .log_err();
                                    }),
                            )
                            .child(
                                IconButton::new(row("remove"), IconName::Trash)
                                    .icon_size(ui::IconSize::XSmall)
                                    .tooltip(Tooltip::text("Remove"))
                                    .on_click(move |_, window, cx| {
                                        remove
                                            .update(cx, |view, cx| {
                                                view.remove_status(&id_remove, window, cx)
                                            })
                                            .log_err();
                                    }),
                            ),
                    )
            }))
            .child(
                h_flex()
                    .pt_1()
                    .gap_2()
                    .child(
                        div()
                            .w(px(260.))
                            .px_1p5()
                            .py_0p5()
                            .rounded_sm()
                            .border_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(self.status_editor.clone()),
                    )
                    .child(
                        ui::Button::new(
                            "status-add",
                            if self.renaming.is_some() {
                                "Rename"
                            } else {
                                "Add status"
                            },
                        )
                        .label_size(LabelSize::Small)
                        .on_click(
                            cx.listener(|view, _, window, cx| {
                                view.add_or_rename_status(window, cx)
                            }),
                        ),
                    ),
            )
    }

    fn render_conditions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let statuses = self.statuses();
        let mapping = self
            .snapshot
            .as_ref()
            .and_then(|s| lookup(&s.resolved, STATUS_BY_CONDITION))
            .cloned()
            .unwrap_or(Value::Null);
        let weak = cx.entity().downgrade();
        v_flex().children(CONDITIONS.iter().map(|(condition, label)| {
            let target = mapping
                .get(*condition)
                .and_then(Value::as_str)
                .map(str::to_owned);
            let shown: SharedString = target
                .as_ref()
                .map(|id| {
                    statuses
                        .iter()
                        .find(|s| &s.id == id)
                        .map_or_else(|| id.clone(), |s| s.label.clone())
                })
                .unwrap_or_else(|| "Don't move".to_owned())
                .into();
            let weak = weak.clone();
            let statuses = statuses.clone();
            h_flex()
                .py_1()
                .justify_between()
                .child(Label::new(*label))
                .child(
                    PopoverMenu::new(SharedString::from(format!("condition-{condition}")))
                        .trigger(
                            ui::Button::new(
                                SharedString::from(format!("condition-button-{condition}")),
                                shown,
                            )
                            .label_size(LabelSize::Small)
                            .end_icon(
                                ui::Icon::new(IconName::ChevronDown).size(ui::IconSize::XSmall),
                            ),
                        )
                        .menu(move |window, cx| {
                            let weak = weak.clone();
                            let statuses = statuses.clone();
                            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                let key = format!("{STATUS_BY_CONDITION}.{condition}");
                                {
                                    let weak = weak.clone();
                                    let key = key.clone();
                                    menu = menu.entry("Don't move", None, move |window, cx| {
                                        weak.update(cx, |view, cx| {
                                            view.write(&key, None, window, cx)
                                        })
                                        .log_err();
                                    });
                                }
                                for status in &statuses {
                                    let weak = weak.clone();
                                    let key = key.clone();
                                    let id = status.id.clone();
                                    menu = menu.entry(
                                        status.label.clone(),
                                        None,
                                        move |window, cx| {
                                            weak.update(cx, |view, cx| {
                                                view.write(&key, Some(json!(id)), window, cx)
                                            })
                                            .log_err();
                                        },
                                    );
                                }
                                menu
                            }))
                        }),
                )
        }))
    }

    fn render_kinds(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let kinds = self
            .snapshot
            .as_ref()
            .map(|s| s.kinds.clone())
            .unwrap_or_default();
        let weak = cx.entity().downgrade();
        v_flex().children(kinds.into_iter().map(|(kind, on)| {
            let weak = weak.clone();
            let label: SharedString = kind.replace('_', " ").into();
            h_flex()
                .py_1()
                .justify_between()
                .child(Label::new(label))
                .child(
                    Switch::new(SharedString::from(format!("kind-{kind}")), on.into()).on_click(
                        move |state, window, cx| {
                            let key = format!("notifications.{kind}");
                            let on = *state == ToggleState::Selected;
                            weak.update(cx, |view, cx| {
                                view.write(&key, Some(json!(on)), window, cx)
                            })
                            .log_err();
                        },
                    ),
                )
        }))
    }

    fn render_cards(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let shown: Vec<String> = self
            .snapshot
            .as_ref()
            .and_then(|s| lookup(&s.resolved, "workspace.cardFields"))
            .and_then(Value::as_array)
            .map(|f| {
                f.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let weak = cx.entity().downgrade();
        v_flex().children(CARD_FIELDS.iter().map(|field| {
            let weak = weak.clone();
            let on = shown.iter().any(|f| f == field.id);
            h_flex()
                .py_1()
                .justify_between()
                .child(Label::new(field.label))
                .child(
                    Switch::new(SharedString::from(format!("card-{}", field.id)), on.into())
                        .on_click(move |_, window, cx| {
                            weak.update(cx, |view, cx| {
                                view.toggle_card_field(field.id, window, cx)
                            })
                            .log_err();
                        }),
                )
        }))
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_project = self.project.is_some();
        let scope = self.scope;
        let rows = |fields: &'static [Field], this: &Self, cx: &mut Context<Self>| {
            fields
                .iter()
                .map(|field| this.render_row(field, cx))
                .collect::<Vec<_>>()
        };
        let agents = rows(AGENTS, self, cx);
        let automation = rows(AUTOMATION, self, cx);
        let integrations = rows(INTEGRATIONS, self, cx);
        let notifications = rows(NOTIFICATIONS, self, cx);
        let ports = rows(PORTS, self, cx);
        let updates = rows(UPDATES, self, cx);
        let layouts = self
            .snapshot
            .as_ref()
            .map(|s| s.layouts.join(", "))
            .unwrap_or_default();
        div()
            .id("icarus-settings")
            .key_context("IcarusSettings")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .mx_auto()
                    .max_w(px(760.))
                    .w_full()
                    .px_6()
                    .py_5()
                    .child(
                        h_flex()
                            .justify_between()
                            .child(Headline::new("Icarus settings").size(HeadlineSize::Small))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        ToggleButtonGroup::single_row(
                                            "icarus-settings-scope",
                                            [
                                                ToggleButtonSimple::new(
                                                    "You",
                                                    cx.listener(|view, _, _, cx| view.set_scope(Scope::User, cx)),
                                                ),
                                                ToggleButtonSimple::new(
                                                    "Project",
                                                    cx.listener(move |view, _, _, cx| {
                                                        if has_project {
                                                            view.set_scope(Scope::Project, cx)
                                                        }
                                                    }),
                                                ),
                                                ToggleButtonSimple::new(
                                                    "Local",
                                                    cx.listener(move |view, _, _, cx| {
                                                        if has_project {
                                                            view.set_scope(Scope::Local, cx)
                                                        }
                                                    }),
                                                ),
                                            ],
                                        )
                                        .selected_index(match scope {
                                            Scope::User => 0,
                                            Scope::Project => 1,
                                            Scope::Local => 2,
                                        })
                                        .style(ui::ToggleButtonGroupStyle::Outlined),
                                    )
                                    .child(
                                        ui::Button::new("icarus-settings-json", "Edit in JSON")
                                            .label_size(LabelSize::Small)
                                            .on_click(cx.listener(|view, _, window, cx| view.open_layer_file(window, cx))),
                                    ),
                            ),
                    )
                    .child(
                        Label::new(match scope {
                            Scope::User => "Changes go to your settings (~/.config/icarus/icarus.json).",
                            Scope::Project => "Changes go to the project's icarus.json, shared with everyone.",
                            Scope::Local => "Changes go to the project's icarus.local.json, on this machine only.",
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .when_some(self.error.clone(), |this, error| {
                        this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                    })
                    .when(self.loading.is_some() && self.snapshot.is_none(), |this| {
                        this.child(Label::new("Loading…").color(Color::Muted))
                    })
                    .child(Self::section("STATUSES", cx))
                    .child(self.render_statuses(cx))
                    .child(Self::section("AUTOMATION", cx))
                    .children(automation)
                    .child(
                        Label::new("Where a pull request's state moves its card:")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(self.render_conditions(cx))
                    .child(Self::section("AGENTS", cx))
                    .children(agents)
                    .child(Self::section("INTEGRATIONS", cx))
                    .child(
                        h_flex()
                            .py_1p5()
                            .justify_between()
                            .child(
                                v_flex().child(Label::new("GitHub")).child(
                                    Label::new(self.gh.clone().unwrap_or_else(|| {
                                        "Uses your GitHub CLI login (gh).".to_owned()
                                    }))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                ),
                            )
                            .child(
                                ui::Button::new("icarus-settings-gh", "Check")
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|view, _, window, cx| view.check_gh(window, cx))),
                            ),
                    )
                    .children(integrations)
                    .child(Self::section("NOTIFICATIONS", cx))
                    .children(notifications)
                    .child(self.render_kinds(cx))
                    .child(Self::section("WORKSPACE CARDS", cx))
                    .child(self.render_cards(cx))
                    .child(Self::section("PORTS AND PREVIEWS", cx))
                    .children(ports)
                    .child(Self::section("LAYOUTS", cx))
                    .child(
                        h_flex()
                            .py_1p5()
                            .justify_between()
                            .child(
                                v_flex().child(Label::new("Named tab layouts")).child(
                                    Label::new(if layouts.is_empty() { "None".to_owned() } else { layouts })
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                            )
                            .child(
                                ui::Button::new("icarus-settings-layouts", "Edit in JSON")
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|view, _, window, cx| view.open_layer_file(window, cx))),
                            ),
                    )
                    .child(Self::section("UPDATES", cx))
                    .children(updates),
            )
    }
}

impl Item for SettingsView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Icarus Settings".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<ui::Icon> {
        Some(ui::Icon::new(IconName::Settings))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

pub fn register(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenSettings, window, cx| {
            let existing = workspace.items_of_type::<SettingsView>(cx).next();
            if let Some(existing) = existing {
                workspace.activate_item(&existing, true, true, window, cx);
                return;
            }
            let root = workspace
                .root_paths(cx)
                .into_iter()
                .next()
                .map(|path| path.to_path_buf());
            let weak = workspace.weak_handle();
            let view = cx.new(|cx| SettingsView::new(weak, root, window, cx));
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::tests::{calls_to, stub_rpc};
    use gpui::TestAppContext;

    fn resolved() -> Value {
        json!({
            "workspace": {
                "statuses": [
                    { "id": "todo", "label": "Todo" },
                    { "id": "review", "label": "Review", "color": "blue" },
                ],
                "cardFields": ["pr"],
            },
            "rules": {
                "enabled": true,
                "armed": false,
                "interval_ms": 60000,
                "orca_compat": { "enabled": true, "statusByCondition": { "conflicts": "review" } },
            },
            "agents": { "default": "claude" },
            "notifications": { "enabled": true },
        })
    }

    #[gpui::test]
    async fn the_settings_view_reads_layers_and_writes_to_the_chosen_one(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let calls = cx.update(|cx| {
            stub_rpc(cx, |method, params| match method {
                "config.get" => Ok(resolved()),
                "notification.kinds" => {
                    Ok(json!({ "kinds": { "agent_turn_done": true, "ci_failed": false } }))
                }
                "layout.list" => Ok(json!({ "default": {}, "review": {} })),
                "config.paths" => {
                    Ok(json!({ "user": "/cfg/icarus.json", "project": null, "local": null }))
                }
                "config.explain" if params["key"] == "rules.armed" => {
                    Ok(json!({ "from": { "layer": "user", "path": "/cfg/icarus.json" } }))
                }
                "config.explain" => Ok(json!({ "from": { "layer": "default" } })),
                _ => Ok(json!({})),
            })
        });
        let window = cx.add_empty_window();
        let view = window.update(|window, cx| {
            cx.new(|cx| SettingsView::new(WeakEntity::new_invalid(), None, window, cx))
        });
        window.run_until_parked();
        view.read_with(window, |view, _| {
            let snapshot = view.snapshot().expect("loaded");
            assert_eq!(snapshot.layouts, ["default", "review"]);
            assert_eq!(snapshot.kinds.len(), 2);
            assert_eq!(view.statuses().len(), 2);
            assert!(
                view.set_here("rules.armed"),
                "armed comes from the user layer"
            );
            assert!(!view.set_here("rules.enabled"));
            assert_eq!(
                view.source_label("rules.enabled"),
                None,
                "defaults show no source"
            );
        });

        view.update_in(window, |view, window, cx| {
            view.write("rules.armed", Some(json!(true)), window, cx);
        });
        window.run_until_parked();
        assert_eq!(
            calls_to(&calls, "config.set")[0],
            json!({ "key": "rules.armed", "layer": "user", "value": true })
        );

        view.update_in(window, |view, window, cx| {
            view.status_editor
                .update(cx, |editor, cx| editor.set_text("On hold", window, cx));
            view.add_or_rename_status(window, cx);
        });
        window.run_until_parked();
        let statuses = calls_to(&calls, "config.set")
            .into_iter()
            .find(|c| c["key"] == "workspace.statuses")
            .expect("statuses written");
        assert_eq!(
            statuses["value"][2],
            json!({ "id": "on-hold", "label": "On hold" })
        );
        assert_eq!(
            statuses["value"][1]["color"], "blue",
            "other statuses keep their style"
        );

        view.update_in(window, |view, window, cx| {
            view.toggle_card_field("pr", window, cx);
            view.project = Some("p-1".into());
            view.set_scope(Scope::Project, cx);
            view.write(
                &format!("{STATUS_BY_CONDITION}.draft"),
                Some(json!("todo")),
                window,
                cx,
            );
            view.write(
                &format!("{STATUS_BY_CONDITION}.conflicts"),
                None,
                window,
                cx,
            );
        });
        window.run_until_parked();
        let sets = calls_to(&calls, "config.set");
        assert!(
            sets.iter()
                .any(|c| c["key"] == "workspace.cardFields" && c["value"] == json!([]))
        );
        assert!(sets.iter().any(|c| {
            c["key"] == "rules.orca_compat.statusByCondition.draft"
                && c["layer"] == "project"
                && c["project"] == "p-1"
        }));
        assert_eq!(
            calls_to(&calls, "config.unset")[0],
            json!({ "key": "rules.orca_compat.statusByCondition.conflicts", "layer": "project", "project": "p-1" })
        );
    }

    #[test]
    fn lookup_walks_dotted_keys() {
        let value = json!({ "a": { "b": { "c": 3 } }, "n": null });
        assert_eq!(lookup(&value, "a.b.c"), Some(&json!(3)));
        assert_eq!(lookup(&value, "a.x"), None);
        assert_eq!(lookup(&value, "n"), None);
    }

    #[test]
    fn status_ids_come_from_their_names() {
        assert_eq!(slug("PM approval"), "pm-approval");
        assert_eq!(slug("  On hold! "), "on-hold");
    }
}
