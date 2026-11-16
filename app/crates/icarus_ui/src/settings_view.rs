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
    ParentElement, Render, ScrollHandle, SharedString, Styled, Subscription, Task, WeakEntity,
    Window, actions, div,
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
        OpenSettings,
        /// Opens Icarus's settings at the apps in Source Control's "Open in" menu.
        CustomizeOpenInApps,
    ]
);

const OPEN_IN: &str = "sourceControl.openIn";

/// The scrolled blocks of the view, so a block can be scrolled into view.
const OPEN_IN_BLOCK: usize = 1;

/// One app in the "Open in" list editor.
struct OpenInRow {
    id: String,
    label: Entity<Editor>,
    command: Entity<Editor>,
    _subscriptions: [Subscription; 2],
}

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
    /// A prompt template: several lines, blank means the built-in one.
    Prompt,
    Integer,
    /// One of the user's status columns, or none.
    Status,
}

struct Field {
    key: &'static str,
    label: &'static str,
    help: &'static str,
    control: Control,
    placeholder: Option<&'static str>,
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
        placeholder: None,
    }
}

impl Field {
    const fn placeholder(self, placeholder: &'static str) -> Self {
        Field {
            placeholder: Some(placeholder),
            ..self
        }
    }
}

const NEW_WORKSPACE_STATUS: &str = "rules.pr.newWorkspaceStatus";

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
        "rules.pr.enabled",
        "Move cards by pull request state",
        "The status rules below.",
        Control::Toggle,
    ),
];

const RULE_OPTIONS: &[Field] = &[
    field(
        NEW_WORKSPACE_STATUS,
        "New workspaces start in",
        "Where a new workspace goes before it has a pull request; existing ones keep their column.",
        Control::Status,
    ),
    field(
        "rules.pr.holdStatus",
        "Hold workspaces in",
        "The rules never move, remove or update a workspace in this column.",
        Control::Status,
    ),
    field(
        "rules.pr.mergingCheckName",
        "Merge gate check name",
        "The check run or commit status that decides \"Merge gate passed\".",
        Control::Text,
    )
    .placeholder("Reviews satisfied"),
    field(
        "rules.pr.pmApprovalTeam",
        "PM approval team",
        "As org/team-slug; empty skips the PM approval column.",
        Control::Text,
    )
    .placeholder("org/team-slug"),
    field(
        "rules.pr.reviewEnteredCommand",
        "Command when your PR enters Review",
        "Runs once in the workspace's folder each time it moves into the review column.",
        Control::Text,
    )
    .placeholder("Run nothing"),
    field(
        "rules.pr.onResolved",
        "When your PR is merged or closed",
        "Delete removes the workspace once its pull request is done.",
        Control::Choice(&["none", "delete"]),
    ),
    field(
        "rules.pr.onReviewed",
        "When you have reviewed",
        "Delete removes a review workspace once your review is in.",
        Control::Choice(&["none", "delete"]),
    ),
    field(
        "rules.pr.resolveConflictsWithAgent",
        "Resolve conflicts with an agent",
        "Start an agent when your PR enters the conflicts column.",
        Control::Toggle,
    ),
    field(
        "rules.pr.conflictPromptTemplate",
        "Conflict prompt",
        "What that agent is asked; blank uses the built-in prompt.",
        Control::Prompt,
    ),
    field(
        "rules.pr.reviewInbox.enabled",
        "Review inbox",
        "Create a review workspace for each review request.",
        Control::Toggle,
    ),
    field(
        "rules.pr.reviewInbox.agent",
        "Review agent",
        "Runs reviews, conflict fixes and action prompts.",
        Control::Choice(&["claude", "codex"]),
    ),
    field(
        "rules.pr.reviewInbox.view",
        "Review agent view",
        "How a review workspace shows its agent.",
        Control::Choice(&["chat", "terminal", "default"]),
    ),
    field(
        "rules.pr.reviewInbox.promptTemplate",
        "Review prompt",
        "The first look at a pull request; blank uses the built-in prompt.",
        Control::Prompt,
    ),
    field(
        "rules.pr.reviewInbox.rereviewPromptTemplate",
        "Re-review prompt",
        "After new commits; blank uses the built-in prompt.",
        Control::Prompt,
    ),
];

/// Orca's top bar action prompts (`actionPrompts`), used by the next-action button.
const ACTION_PROMPTS: &[Field] = &[
    field(
        "rules.pr.actionPrompts.commitAndPush",
        "Commit & push",
        "Blank uses the built-in prompt.",
        Control::Prompt,
    ),
    field(
        "rules.pr.actionPrompts.createPullRequest",
        "Create PR",
        "Blank uses the built-in prompt.",
        Control::Prompt,
    ),
    field(
        "rules.pr.actionPrompts.readyForReview",
        "Ready for review",
        "Blank uses the built-in prompt.",
        Control::Prompt,
    ),
    field(
        "rules.pr.actionPrompts.reviewPullRequest",
        "Review",
        "Blank uses the built-in prompt.",
        Control::Prompt,
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

const STATUS_BY_CONDITION: &str = "rules.pr.statusByCondition";

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
    open_in_rows: Vec<OpenInRow>,
    scroll_handle: ScrollHandle,
    /// Scroll to the "Open in" apps once the settings have loaded.
    reveal_open_in: bool,
    _subscriptions: Vec<Subscription>,
}

#[derive(Default)]
struct OpenSettingsViews(Vec<(gpui::AnyWindowHandle, WeakEntity<SettingsView>)>);

impl gpui::Global for OpenSettingsViews {}

/// Handles the server's `config.changed`: settings were written somewhere (CLI, an agent, an editor).
pub(crate) fn config_changed(cx: &mut App) {
    let views = cx
        .try_global::<OpenSettingsViews>()
        .map(|views| views.0.clone())
        .unwrap_or_default();
    for (handle, view) in views {
        handle
            .update(cx, |_, window, cx| {
                view.update(cx, |view, cx| view.reload(window, cx))
            })
            .ok();
    }
    if cx.has_global::<OpenSettingsViews>() {
        cx.update_global::<OpenSettingsViews, _>(|views, _| {
            views.0.retain(|(_, view)| view.upgrade().is_some());
        });
    }
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
        for field in [
            AUTOMATION,
            RULE_OPTIONS,
            ACTION_PROMPTS,
            INTEGRATIONS,
            PORTS,
        ]
        .into_iter()
        .flatten()
        .filter(|f| {
            matches!(
                f.control,
                Control::Text | Control::Prompt | Control::Integer
            )
        }) {
            let prompt = field.control == Control::Prompt;
            let editor = cx.new(|cx| {
                let mut editor = if prompt {
                    let mut editor = Editor::auto_height(1, 8, window, cx);
                    editor.set_soft_wrap();
                    editor
                } else {
                    Editor::single_line(window, cx)
                };
                editor.set_placeholder_text(
                    field
                        .placeholder
                        .unwrap_or(if prompt { "Built-in prompt" } else { "Not set" }),
                    window,
                    cx,
                );
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
            open_in_rows: Vec::new(),
            scroll_handle: ScrollHandle::new(),
            reveal_open_in: false,
            _subscriptions: subscriptions,
        };
        let handle = window.window_handle();
        let weak = cx.weak_entity();
        cx.default_global::<OpenSettingsViews>()
            .0
            .push((handle, weak));
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
            RULE_OPTIONS,
            ACTION_PROMPTS,
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
            OPEN_IN,
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
            // A reload from a change elsewhere must not overwrite what is being typed.
            if editor.focus_handle(cx).is_focused(window) {
                continue;
            }
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
        self.sync_open_in_rows(window, cx);
        if std::mem::take(&mut self.reveal_open_in) {
            self.scroll_handle.scroll_to_top_of_item(OPEN_IN_BLOCK);
        }
        cx.notify();
    }

    /// Scrolls to the "Open in" apps, now or once the settings have loaded.
    pub fn reveal_open_in(&mut self, cx: &mut Context<Self>) {
        if self.snapshot.is_some() {
            self.scroll_handle.scroll_to_top_of_item(OPEN_IN_BLOCK);
            cx.notify();
        } else {
            self.reveal_open_in = true;
        }
    }

    /// The apps as saved (unknown fields kept), or Orca's default while none are.
    pub fn open_in_apps(&self) -> Vec<Value> {
        match self
            .snapshot
            .as_ref()
            .and_then(|snapshot| lookup(&snapshot.resolved, OPEN_IN))
        {
            Some(Value::Array(apps)) => {
                apps.iter().filter(|app| app.is_object()).cloned().collect()
            }
            _ => crate::source_control_settings::default_open_in()
                .into_iter()
                .map(|app| json!({ "id": app.id, "label": app.label, "command": app.command }))
                .collect(),
        }
    }

    fn open_in_editor(
        text: &str,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<Editor>, Subscription) {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(placeholder, window, cx);
            editor.set_text(text, window, cx);
            editor
        });
        let subscription = cx.subscribe_in(&editor, window, |this, _, event, window, cx| {
            if matches!(event, EditorEvent::Blurred) {
                this.commit_open_in(window, cx);
            }
        });
        (editor, subscription)
    }

    /// Rebuilds the rows when the saved apps changed, unless one is being typed in.
    fn sync_open_in_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typing = self.open_in_rows.iter().any(|row| {
            row.label.focus_handle(cx).is_focused(window)
                || row.command.focus_handle(cx).is_focused(window)
        });
        if typing {
            return;
        }
        let text = |app: &Value, key: &str| app[key].as_str().unwrap_or_default().to_owned();
        let apps = self.open_in_apps();
        let current: Vec<(String, String, String)> = self
            .open_in_rows
            .iter()
            .map(|row| {
                (
                    row.id.clone(),
                    row.label.read(cx).text(cx),
                    row.command.read(cx).text(cx),
                )
            })
            .collect();
        let saved: Vec<(String, String, String)> = apps
            .iter()
            .map(|app| (text(app, "id"), text(app, "label"), text(app, "command")))
            .collect();
        if current == saved {
            return;
        }
        self.open_in_rows = saved
            .into_iter()
            .map(|(id, label, command)| {
                let (label, label_subscription) =
                    Self::open_in_editor(&label, "Menu label", window, cx);
                let (command, command_subscription) =
                    Self::open_in_editor(&command, "Command, e.g. code", window, cx);
                OpenInRow {
                    id,
                    label,
                    command,
                    _subscriptions: [label_subscription, command_subscription],
                }
            })
            .collect();
    }

    fn write_open_in(&mut self, apps: Vec<Value>, window: &mut Window, cx: &mut Context<Self>) {
        if apps == self.open_in_apps() {
            return;
        }
        // The Source Control panel reads the app's own user settings, so this list lives there.
        self.write_to(Scope::User, OPEN_IN, Some(Value::Array(apps)), window, cx);
    }

    /// Saves what the rows say, keeping fields of each app that the editor doesn't show.
    pub fn commit_open_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let saved = self.open_in_apps();
        let apps: Vec<Value> = self
            .open_in_rows
            .iter()
            .map(|row| {
                let mut app = saved
                    .iter()
                    .find(|app| app["id"].as_str() == Some(row.id.as_str()))
                    .cloned()
                    .unwrap_or_else(|| json!({ "id": row.id }));
                app["label"] = json!(row.label.read(cx).text(cx).trim());
                app["command"] = json!(row.command.read(cx).text(cx).trim());
                app
            })
            .collect();
        self.write_open_in(apps, window, cx);
    }

    pub fn add_open_in_app(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut apps = self.open_in_apps();
        let id = (1..)
            .map(|n| format!("custom-{n}"))
            .find(|id| !apps.iter().any(|app| app["id"].as_str() == Some(id)))
            .unwrap_or_default();
        apps.push(json!({ "id": id, "label": "New app", "command": "" }));
        self.write_open_in(apps, window, cx);
    }

    pub fn move_open_in_app(
        &mut self,
        index: usize,
        up: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut apps = self.open_in_apps();
        let Some(other) = (if up {
            index.checked_sub(1)
        } else {
            Some(index + 1)
        })
        .filter(|other| *other < apps.len() && index < apps.len()) else {
            return;
        };
        apps.swap(index, other);
        self.write_open_in(apps, window, cx);
    }

    pub fn remove_open_in_app(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut apps = self.open_in_apps();
        if index < apps.len() {
            apps.remove(index);
            self.write_open_in(apps, window, cx);
        }
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
        self.write_to(self.scope, key, value, window, cx);
    }

    fn write_to(
        &mut self,
        scope: Scope,
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
        let mut params = json!({ "key": key, "layer": scope.layer() });
        if let Some(value) = value {
            params["value"] = value;
        }
        if scope != Scope::User {
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

    /// A status column setting; a new-workspace column also stamps when it took effect, as Orca does,
    /// so only workspaces created afterwards start there.
    pub fn choose_status(
        &mut self,
        key: &str,
        status: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if key == NEW_WORKSPACE_STATUS {
            let since = status.as_ref().map(|_| {
                json!(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|elapsed| elapsed.as_millis() as u64)
                        .unwrap_or_default()
                )
            });
            self.write(&format!("{NEW_WORKSPACE_STATUS}Since"), since, window, cx);
        }
        self.write(key, status.map(|status| json!(status)), window, cx);
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
                    // Each choice lists the built-in value first.
                    .or_else(|| options.first().copied())
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
            Control::Status => {
                let statuses = self.statuses();
                let current: SharedString = value
                    .as_ref()
                    .and_then(Value::as_str)
                    .map(|id| {
                        statuses
                            .iter()
                            .find(|status| status.id == id)
                            .map_or_else(|| id.to_owned(), |status| status.label.clone())
                    })
                    .unwrap_or_else(|| "Do nothing".to_owned())
                    .into();
                PopoverMenu::new(SharedString::from(format!("status-choice-{key}")))
                    .trigger(
                        ui::Button::new(
                            SharedString::from(format!("status-choice-button-{key}")),
                            current,
                        )
                        .label_size(LabelSize::Small)
                        .end_icon(ui::Icon::new(IconName::ChevronDown).size(ui::IconSize::XSmall)),
                    )
                    .menu(move |window, cx| {
                        let weak = weak.clone();
                        let statuses = statuses.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            let choices = std::iter::once((None, "Do nothing".to_owned())).chain(
                                statuses
                                    .iter()
                                    .map(|status| (Some(status.id.clone()), status.label.clone())),
                            );
                            for (id, label) in choices {
                                let weak = weak.clone();
                                menu = menu.entry(label, None, move |window, cx| {
                                    weak.update(cx, |view, cx| {
                                        view.choose_status(key, id.clone(), window, cx)
                                    })
                                    .log_err();
                                });
                            }
                            menu
                        }))
                    })
                    .into_any_element()
            }
            Control::Text | Control::Prompt | Control::Integer => match self.editors.get(key) {
                Some(editor) => div()
                    .w(px(if field.control == Control::Prompt {
                        320.
                    } else {
                        260.
                    }))
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

    fn render_open_in(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.open_in_rows.len();
        let field = |editor: &Entity<Editor>, width: f32, cx: &App| {
            div()
                .w(px(width))
                .px_1p5()
                .py_0p5()
                .rounded_sm()
                .border_1()
                .border_color(cx.theme().colors().border_variant)
                .child(editor.clone())
        };
        let reset = self.set_here(OPEN_IN).then(|| {
            ui::Button::new("open-in-reset", "Use the default")
                .label_size(LabelSize::Small)
                .on_click(cx.listener(|view, _, window, cx| {
                    view.write_to(Scope::User, OPEN_IN, None, window, cx)
                }))
        });
        v_flex()
            .child(
                h_flex()
                    .py_1p5()
                    .gap_4()
                    .justify_between()
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(Label::new("Open in apps"))
                            .child(
                                Label::new(
                                    "A changed file's \"Open in\" menu. A plain command gets the file's \
                                     path; a command with spaces runs through the shell. Saved in your settings.",
                                )
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_none()
                            .children(reset)
                            .child(
                                ui::Button::new("open-in-add", "Add app")
                                    .label_size(LabelSize::Small)
                                    .start_icon(
                                        ui::Icon::new(IconName::Plus).size(ui::IconSize::XSmall),
                                    )
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.add_open_in_app(window, cx)
                                    })),
                            ),
                    ),
            )
            .when(count == 0, |this| {
                this.child(
                    Label::new("No apps: the menu offers only Customize apps…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .children(self.open_in_rows.iter().enumerate().map(|(index, row)| {
                let id = |suffix: &str| SharedString::from(format!("open-in-{suffix}-{index}"));
                h_flex()
                    .py_1()
                    .gap_2()
                    .child(field(&row.label, 160., cx))
                    .child(field(&row.command, 240., cx))
                    .child(div().flex_1())
                    .child(
                        IconButton::new(id("up"), IconName::ArrowUp)
                            .icon_size(ui::IconSize::XSmall)
                            .disabled(index == 0)
                            .tooltip(Tooltip::text("Move up"))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.move_open_in_app(index, true, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new(id("down"), IconName::ArrowDown)
                            .icon_size(ui::IconSize::XSmall)
                            .disabled(index + 1 == count)
                            .tooltip(Tooltip::text("Move down"))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.move_open_in_app(index, false, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new(id("remove"), IconName::Trash)
                            .icon_size(ui::IconSize::XSmall)
                            .tooltip(Tooltip::text("Remove app"))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.remove_open_in_app(index, window, cx)
                            })),
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
        let rule_options = rows(RULE_OPTIONS, self, cx);
        let action_prompts = rows(ACTION_PROMPTS, self, cx);
        let column = || v_flex().mx_auto().max_w(px(760.)).w_full().px_6();
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
            .track_scroll(&self.scroll_handle)
            .bg(cx.theme().colors().editor_background)
            .child(
                column()
                    .pt_5()
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
                    .child(Self::section("PULL REQUEST RULES", cx))
                    .children(rule_options)
                    .child(Self::section("TOP BAR ACTION PROMPTS", cx))
                    .child(
                        Label::new(
                            "The next-action button's prompts. They can use {{branch}}, {{baseRef}}, \
                             {{worktreePath}}, {{prNumber}}, {{prUrl}}, {{prTitle}}, {{draftMode}}, \
                             {{templateRule}} and {{prInstructions}}.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .children(action_prompts)
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
                    ),
            )
            .child(
                column()
                    .child(Self::section("SOURCE CONTROL", cx))
                    .child(self.render_open_in(cx)),
            )
            .child(
                column()
                    .pb_5()
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

/// Shows the settings tab, reusing an open one.
fn open_settings(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<SettingsView> {
    let existing = workspace.items_of_type::<SettingsView>(cx).next();
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return existing;
    }
    let root = workspace
        .root_paths(cx)
        .into_iter()
        .next()
        .map(|path| path.to_path_buf());
    let weak = workspace.weak_handle();
    let view = cx.new(|cx| SettingsView::new(weak, root, window, cx));
    workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
    view
}

pub fn register(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenSettings, window, cx| {
            open_settings(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &CustomizeOpenInApps, window, cx| {
            open_settings(workspace, window, cx).update(cx, |view, cx| view.reveal_open_in(cx));
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
                "pr": { "enabled": true, "statusByCondition": { "conflicts": "review" } },
            },
            "agents": { "default": "claude" },
            "notifications": { "enabled": true },
        })
    }

    #[gpui::test]
    fn a_settings_change_before_any_settings_tab_opened_is_ignored(cx: &mut TestAppContext) {
        cx.update(config_changed);
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
            c["key"] == "rules.pr.statusByCondition.draft"
                && c["layer"] == "project"
                && c["project"] == "p-1"
        }));
        assert_eq!(
            calls_to(&calls, "config.unset")[0],
            json!({ "key": "rules.pr.statusByCondition.conflicts", "layer": "project", "project": "p-1" })
        );
    }

    #[gpui::test]
    async fn the_pull_request_rule_options_load_and_save_from_the_tab(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let calls = cx.update(|cx| {
            stub_rpc(cx, |method, _| match method {
                "config.get" => {
                    let mut value = resolved();
                    value["rules"]["pr"]["onResolved"] = json!("delete");
                    value["rules"]["pr"]["reviewInbox"] =
                        json!({ "enabled": true, "agent": "claude", "promptTemplate": "Review {pr_url}\nCarefully." });
                    Ok(value)
                }
                "config.explain" => Ok(json!({ "from": { "layer": "user" } })),
                _ => Ok(json!({})),
            })
        });
        let window = cx.add_empty_window();
        let view = window.update(|window, cx| {
            cx.new(|cx| SettingsView::new(WeakEntity::new_invalid(), None, window, cx))
        });
        window.run_until_parked();
        let explained: Vec<Value> = calls_to(&calls, "config.explain")
            .into_iter()
            .map(|call| call["key"].clone())
            .collect();
        for field in RULE_OPTIONS {
            assert!(
                explained.contains(&json!(field.key)),
                "{} shows where it comes from",
                field.key
            );
        }
        let prompt = view.read_with(window, |view, cx| {
            assert!(view.set_here("rules.pr.onResolved"));
            let editor = view.editors["rules.pr.reviewInbox.promptTemplate"].clone();
            assert_eq!(editor.read(cx).text(cx), "Review {pr_url}\nCarefully.");
            assert_eq!(
                view.editors["rules.pr.conflictPromptTemplate"]
                    .read(cx)
                    .text(cx),
                "",
                "blank shows the built-in placeholder"
            );
            editor
        });

        view.update_in(window, |view, window, cx| {
            view.write("rules.pr.onReviewed", Some(json!("delete")), window, cx);
            view.write(
                "rules.pr.resolveConflictsWithAgent",
                Some(json!(true)),
                window,
                cx,
            );
            prompt.update(cx, |editor, cx| {
                editor.set_text("Look closer.\nTwice.", window, cx)
            });
            view.commit_text(
                "rules.pr.reviewInbox.promptTemplate",
                "Look closer.\nTwice.",
                false,
                window,
                cx,
            );
        });
        window.run_until_parked();
        let sets = calls_to(&calls, "config.set");
        for (key, value) in [
            ("rules.pr.onReviewed", json!("delete")),
            ("rules.pr.resolveConflictsWithAgent", json!(true)),
            (
                "rules.pr.reviewInbox.promptTemplate",
                json!("Look closer.\nTwice."),
            ),
        ] {
            assert!(
                sets.iter()
                    .any(|call| call["key"] == key && call["value"] == value),
                "{key} saved: {sets:?}"
            );
        }
    }

    #[gpui::test]
    async fn a_settings_change_elsewhere_reloads_the_view_but_not_mid_edit(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let current = Arc::new(std::sync::Mutex::new(resolved()));
        cx.update(|cx| {
            let current = current.clone();
            stub_rpc(cx, move |method, _| match method {
                "config.get" => Ok(current.lock().expect("lock").clone()),
                "config.explain" => Ok(json!({ "from": { "layer": "default" } })),
                _ => Ok(json!({})),
            })
        });
        // Rendered, so the field's blur event fires.
        let (view, window) = cx.add_window_view(|window, cx| {
            SettingsView::new(WeakEntity::new_invalid(), None, window, cx)
        });
        window.run_until_parked();
        view.read_with(window, |view, _| assert_eq!(view.statuses().len(), 2));

        let add_status = |label: &str| {
            current.lock().expect("lock")["workspace"]["statuses"]
                .as_array_mut()
                .expect("statuses")
                .push(json!({ "id": label.to_lowercase(), "label": label }));
        };
        // The app handles events outside any window update, as here.
        let changed = |window: &mut gpui::VisualTestContext| {
            let app: &mut TestAppContext = window;
            app.update(config_changed);
        };
        add_status("Shipped");
        changed(window);
        window.run_until_parked();
        view.read_with(window, |view, _| assert_eq!(view.statuses().len(), 3));

        let editor = view.read_with(window, |view, _| {
            view.editors.values().next().expect("a text field").clone()
        });
        window.update(|window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
            editor.update(cx, |editor, cx| editor.set_text("half typed", window, cx));
        });
        add_status("Archived");
        changed(window);
        window.run_until_parked();
        view.read_with(window, |view, _| assert_eq!(view.statuses().len(), 4));
        window.update(|_, cx| {
            assert_eq!(
                editor.read(cx).text(cx),
                "half typed",
                "the field being typed in keeps its text"
            )
        });
    }

    fn init_settings(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    #[gpui::test]
    async fn the_remaining_orca_rule_options_have_controls(cx: &mut TestAppContext) {
        init_settings(cx);
        let calls = cx.update(|cx| {
            stub_rpc(cx, |method, _| match method {
                "config.get" => {
                    let mut value = resolved();
                    value["rules"]["pr"]["mergingCheckName"] = json!("Merge gate");
                    value["rules"]["pr"]["actionPrompts"] =
                        json!({ "readyForReview": "Polish #{{prNumber}}." });
                    Ok(value)
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
        for key in [
            NEW_WORKSPACE_STATUS,
            "rules.pr.holdStatus",
            "rules.pr.pmApprovalTeam",
            "rules.pr.actionPrompts.commitAndPush",
        ] {
            assert!(
                calls_to(&calls, "config.explain")
                    .iter()
                    .any(|call| call["key"] == key),
                "{key} shows where it comes from"
            );
        }
        view.read_with(window, |view, cx| {
            let text = |key: &str| view.editors[key].read(cx).text(cx);
            assert_eq!(text("rules.pr.mergingCheckName"), "Merge gate");
            assert_eq!(
                text("rules.pr.actionPrompts.readyForReview"),
                "Polish #{{prNumber}}."
            );
            assert_eq!(
                text("rules.pr.actionPrompts.reviewPullRequest"),
                "",
                "blank is the built-in prompt"
            );
        });

        view.update_in(window, |view, window, cx| {
            view.choose_status(NEW_WORKSPACE_STATUS, Some("todo".into()), window, cx);
            view.choose_status("rules.pr.holdStatus", Some("review".into()), window, cx);
            view.commit_text("rules.pr.pmApprovalTeam", "acme/pm", false, window, cx);
            view.commit_text(
                "rules.pr.reviewEnteredCommand",
                "mise run stop",
                false,
                window,
                cx,
            );
            view.commit_text(
                "rules.pr.actionPrompts.commitAndPush",
                "Commit it.\nPush it.",
                false,
                window,
                cx,
            );
        });
        window.run_until_parked();
        let sets = calls_to(&calls, "config.set");
        let set = |key: &str| {
            sets.iter()
                .find(|call| call["key"] == key)
                .map(|call| call["value"].clone())
        };
        assert_eq!(set(NEW_WORKSPACE_STATUS), Some(json!("todo")));
        assert!(
            set("rules.pr.newWorkspaceStatusSince")
                .is_some_and(|since| since.as_u64().is_some_and(|since| since > 0)),
            "Orca stamps when the column took effect"
        );
        assert_eq!(set("rules.pr.holdStatus"), Some(json!("review")));
        assert_eq!(set("rules.pr.pmApprovalTeam"), Some(json!("acme/pm")));
        assert_eq!(
            set("rules.pr.reviewEnteredCommand"),
            Some(json!("mise run stop"))
        );
        assert_eq!(
            set("rules.pr.actionPrompts.commitAndPush"),
            Some(json!("Commit it.\nPush it."))
        );

        view.update_in(window, |view, window, cx| {
            view.choose_status(NEW_WORKSPACE_STATUS, None, window, cx)
        });
        window.run_until_parked();
        let unset: Vec<Value> = calls_to(&calls, "config.unset")
            .into_iter()
            .map(|call| call["key"].clone())
            .collect();
        assert!(unset.contains(&json!(NEW_WORKSPACE_STATUS)));
        assert!(unset.contains(&json!("rules.pr.newWorkspaceStatusSince")));
    }

    #[gpui::test]
    async fn the_open_in_apps_are_edited_as_a_list_in_the_user_settings(cx: &mut TestAppContext) {
        init_settings(cx);
        let current = Arc::new(std::sync::Mutex::new(resolved()));
        let calls = cx.update(|cx| {
            let current = current.clone();
            stub_rpc(cx, move |method, params| match method {
                "config.get" => Ok(current.lock().expect("lock").clone()),
                "config.set" if params["key"] == OPEN_IN => {
                    current.lock().expect("lock")["sourceControl"] =
                        json!({ "openIn": params["value"].clone() });
                    Ok(json!({}))
                }
                "config.explain" => Ok(json!({ "from": { "layer": "default" } })),
                _ => Ok(json!({})),
            })
        });
        let (view, window) = cx.add_window_view(|window, cx| {
            SettingsView::new(WeakEntity::new_invalid(), None, window, cx)
        });
        window.run_until_parked();
        let rows = |view: &Entity<SettingsView>, window: &mut gpui::VisualTestContext| {
            view.read_with(window, |view, cx| {
                view.open_in_rows
                    .iter()
                    .map(|row| {
                        format!(
                            "{}={}",
                            row.label.read(cx).text(cx),
                            row.command.read(cx).text(cx)
                        )
                    })
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(rows(&view, window), ["VS Code=code"], "Orca's default");

        view.update_in(window, |view, window, cx| {
            view.project = Some("p-1".into());
            view.set_scope(Scope::Project, cx);
            view.add_open_in_app(window, cx);
        });
        window.run_until_parked();
        assert_eq!(rows(&view, window), ["VS Code=code", "New app="]);
        let saved = calls_to(&calls, "config.set");
        assert_eq!(
            saved[0]["layer"], "user",
            "the panel reads the user settings"
        );
        assert_eq!(saved[0]["project"], Value::Null);

        current.lock().expect("lock")["sourceControl"]["openIn"][0]["icon"] = json!("vscode");
        {
            let app: &mut TestAppContext = window;
            app.update(config_changed);
        }
        window.run_until_parked();
        view.update_in(window, |view, window, cx| {
            let row = &view.open_in_rows[1];
            let (label, command) = (row.label.clone(), row.command.clone());
            label.update(cx, |editor, cx| editor.set_text("Xcode", window, cx));
            command.update(cx, |editor, cx| {
                editor.set_text(" open -a Xcode ", window, cx)
            });
            view.commit_open_in(window, cx);
        });
        window.run_until_parked();
        assert_eq!(rows(&view, window), ["VS Code=code", "Xcode=open -a Xcode"]);
        let apps = view.read_with(window, |view, _| view.open_in_apps());
        assert_eq!(
            apps[0]["icon"], "vscode",
            "fields the editor doesn't show stay"
        );
        assert_eq!(apps[1]["id"], "custom-1");

        view.update_in(window, |view, window, cx| {
            view.move_open_in_app(1, true, window, cx)
        });
        window.run_until_parked();
        assert_eq!(rows(&view, window), ["Xcode=open -a Xcode", "VS Code=code"]);
        view.update_in(window, |view, window, cx| {
            view.remove_open_in_app(1, window, cx)
        });
        window.run_until_parked();
        assert_eq!(rows(&view, window), ["Xcode=open -a Xcode"]);
        let parsed = crate::source_control_settings::SourceControlSettings::from_settings(
            &current.lock().expect("lock")["sourceControl"],
        );
        assert_eq!(
            parsed.open_in[0].command, "open -a Xcode",
            "the menu reads it"
        );
    }

    #[gpui::test]
    async fn customize_apps_opens_the_settings_tab_at_the_open_in_apps(cx: &mut TestAppContext) {
        init_settings(cx);
        cx.update(|cx| {
            register(cx);
            stub_rpc(cx, |method, _| match method {
                "config.get" => Ok(resolved()),
                "config.explain" => Ok(json!({ "from": { "layer": "default" } })),
                _ => Ok(json!({})),
            });
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "a.txt": "a" })).await;
        let project = project::Project::test(fs, [std::path::Path::new("/project")], cx).await;
        let window = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = window
            .read_with(cx, |multi, _| multi.workspace().clone())
            .expect("workspace");
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        cx.dispatch_action(CustomizeOpenInApps);
        cx.run_until_parked();
        let open = |cx: &mut gpui::VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .items_of_type::<SettingsView>(cx)
                    .collect::<Vec<_>>()
            })
        };
        let views = open(&mut cx);
        assert_eq!(views.len(), 1);
        views[0].read_with(&cx, |view, _| {
            assert!(view.snapshot().is_some());
            assert!(!view.reveal_open_in, "scrolled once the settings loaded");
            assert_eq!(view.open_in_rows.len(), 1);
        });
        cx.dispatch_action(CustomizeOpenInApps);
        cx.run_until_parked();
        assert_eq!(open(&mut cx).len(), 1, "the open tab is reused");
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
