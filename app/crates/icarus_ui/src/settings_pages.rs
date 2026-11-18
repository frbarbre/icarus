//! Icarus's settings as pages of the app's settings window. Everything goes through the server's
//! `config.*` RPCs, so the user, project and project-local layers stay authoritative and each row
//! says which layer its value comes from. Complex values open the layer's JSON file instead.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, AnyWindowHandle, App, Context, Entity, Focusable, IntoElement, ParentElement,
    Render, SharedString, Styled, Subscription, Task, WeakEntity, Window, WindowHandle, actions,
    div,
};
use icarus_client::Client;
use serde_json::{Value, json};
use settings_ui::ExtensionSection;
use ui::{
    ContextMenu, IconButton, IconName, Label, LabelSize, PopoverMenu, Switch, ToggleButtonGroup,
    ToggleButtonSimple, ToggleState, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{MultiWorkspace, OpenOptions, Workspace};

use crate::cards::CARD_FIELDS;
use crate::model::StatusColumn;
use crate::review::{app_client, request};
use crate::status_style::{STATUS_COLORS, STATUS_ICONS, status_icon, swatch_color};

actions!(
    icarus,
    [
        /// Opens Icarus's settings: statuses, automations and prompts.
        OpenSettings,
        /// Opens Icarus's settings at the apps in Source Control's "Open in" menu.
        CustomizeOpenInApps,
        /// Opens Icarus's settings at the Linear API key.
        ConnectLinear,
        /// Opens Icarus's settings at the agents: the default harness and Remote Control.
        OpenAgentSettings,
    ]
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Statuses,
    Scripts,
    Prompts,
    Cards,
    Ports,
    Layouts,
    OpenIn,
    Agents,
    Integrations,
    Notifications,
    Updates,
}

pub const PAGES: &[(&str, &[Section])] = &[
    (
        "Automations",
        &[Section::Statuses, Section::Scripts, Section::Prompts],
    ),
    (
        "Workspaces",
        &[
            Section::Cards,
            Section::Ports,
            Section::Layouts,
            Section::OpenIn,
        ],
    ),
    (
        "Icarus",
        &[
            Section::Agents,
            Section::Integrations,
            Section::Notifications,
            Section::Updates,
        ],
    ),
];

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Section::Statuses => "Statuses",
            Section::Scripts => "Scripts",
            Section::Prompts => "Prompts",
            Section::Cards => "Workspace Cards",
            Section::Ports => "Ports and Previews",
            Section::Layouts => "Layouts",
            Section::OpenIn => "Open In Apps",
            Section::Agents => "Agents",
            Section::Integrations => "Integrations",
            Section::Notifications => "Notifications",
            Section::Updates => "Updates",
        }
    }

    fn page(self) -> &'static str {
        PAGES
            .iter()
            .find(|(_, sections)| sections.contains(&self))
            .map_or("Automations", |(page, _)| page)
    }

    fn starts_page(self) -> bool {
        PAGES
            .iter()
            .any(|(_, sections)| sections.first() == Some(&self))
    }
}

const OPEN_IN: &str = "sourceControl.openIn";
const PROMPTS: &str = "prompts";
// The next-action button reads these; listed even when unset so they stay editable.
const NEXT_ACTION_PROMPTS: [&str; 4] = [
    "commitAndPush",
    "createPullRequest",
    "readyForReview",
    "reviewPullRequest",
];
const SCRIPTS: &str = "automations.scripts";

struct OpenInRow {
    id: String,
    label: Entity<Editor>,
    command: Entity<Editor>,
    _subscriptions: [Subscription; 2],
}

struct PromptRow {
    id: String,
    template: Entity<Editor>,
    _subscription: Subscription,
}

struct ScriptRow {
    project_id: String,
    saved: String,
    path: Entity<Editor>,
    _subscription: Subscription,
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
    /// What a toggle shows when no layer sets it.
    default_on: bool,
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
        default_on: false,
    }
}

impl Field {
    const fn placeholder(self, placeholder: &'static str) -> Self {
        Field {
            placeholder: Some(placeholder),
            ..self
        }
    }

    const fn on_by_default(self) -> Self {
        Field {
            default_on: true,
            ..self
        }
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

const AUTOMATIONS: &[Field] = &[
    field(
        "automations.enabled",
        "Run automation scripts",
        "Kill switch: off stops every automation run.",
        Control::Toggle,
    )
    .on_by_default(),
    field(
        "automations.armed",
        "Apply script actions",
        "Until on, every run is a dry run.",
        Control::Toggle,
    ),
    field(
        "automations.interval_ms",
        "Tick interval (ms)",
        "Time between scheduled runs.",
        Control::Integer,
    )
    .placeholder("60000"),
    field(
        "automations.max_agents_per_run",
        "Agents per run",
        "Agents one run may start; actions beyond it fail.",
        Control::Integer,
    )
    .placeholder("3"),
    field(
        "automations.hold_status",
        "Hold column",
        "Workspaces in this column get no actions unless the action says force.",
        Control::Status,
    ),
];

const INTEGRATIONS: &[Field] = &[
    field(
        "linear.api_key_command",
        "Linear key command",
        "For headless servers: prints your Linear API key, and wins over a saved key.",
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

#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub layer: String,
    pub path: Option<String>,
}

/// What the pages show, loaded in one go.
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

pub struct IcarusSettings {
    workspace_window: Option<WindowHandle<MultiWorkspace>>,
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
    /// The server's `linear.status`; never holds the key.
    linear: Option<Value>,
    linear_key: Entity<Editor>,
    linear_busy: bool,
    linear_error: Option<String>,
    /// The server's `automation.status`.
    automation: Option<Value>,
    automation_error: Option<String>,
    /// The outcome of the last button pressed, by project id (or `schema`).
    automation_notes: HashMap<String, String>,
    automation_busy: Option<String>,
    script_rows: Vec<ScriptRow>,
    prompt_rows: Vec<PromptRow>,
    prompt_name: Entity<Editor>,
    prompt_renaming: Option<String>,
    /// The previewed prompt's id and its rendering or error.
    prompt_preview: Option<(String, Result<String, String>)>,
    /// Workspaces to preview prompts against, as (id, name).
    workspaces: Vec<(String, String)>,
    preview_workspace: Option<String>,
    _subscriptions: Vec<Subscription>,
}

/// One line on where the Linear key comes from and whose it is.
pub fn linear_summary(status: Option<&Value>) -> String {
    let Some(status) = status else {
        return "Checking…".to_owned();
    };
    let viewer = status["viewer"].as_str();
    let source = status["source"].as_str();
    if status["rejected"].as_bool() == Some(true) {
        return match source {
            Some("stored") => "Linear rejected the saved key; paste a new one.".to_owned(),
            Some("env") => "Linear rejected LINEAR_API_KEY on the server.".to_owned(),
            _ => "Linear rejected the key from the key command.".to_owned(),
        };
    }
    if status["configured"].as_bool() != Some(true) {
        return "Not connected. Paste a personal API key from Linear (Settings → Security & access)."
            .to_owned();
    }
    let who = viewer.map_or_else(
        || "Connected".to_owned(),
        |name| format!("Connected as {name}"),
    );
    match source {
        Some("stored") => match status["store"].as_str() {
            Some("Keychain") => format!("{who} (stored in Keychain)"),
            _ => format!("{who} (stored on the server)"),
        },
        Some("env") => format!("{who} (LINEAR_API_KEY on the server)"),
        Some("command") => format!("{who} (from the key command)"),
        _ => who,
    }
}

/// One line on an `automation.run` reply for a single project.
pub fn dry_run_summary(reply: &Value) -> String {
    let Some(run) = reply["runs"].as_array().and_then(|runs| runs.first()) else {
        return "Nothing ran: the project has no script.".to_owned();
    };
    if let Some(error) = run["error"].as_str() {
        return format!("Dry run failed: {error}");
    }
    let results = run["results"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let errors = results
        .iter()
        .filter(|result| result["ok"].as_bool() == Some(false))
        .count();
    format!(
        "Dry run: {} action{}, {errors} error{}",
        results.len(),
        if results.len() == 1 { "" } else { "s" },
        if errors == 1 { "" } else { "s" },
    )
}

fn ago(since_ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default();
    let minutes = (now - since_ms).max(0) / 60_000;
    match minutes {
        0 => "just now".to_owned(),
        1..=59 => format!("{minutes} min ago"),
        60..=1439 => format!("{} h ago", minutes / 60),
        _ => format!("{} d ago", minutes / 1440),
    }
}

/// One line on a project's last run.
pub fn run_summary(project: &Value) -> String {
    let last = &project["last_run"];
    let Some(started) = last["started_ms"].as_i64() else {
        return "Never run".to_owned();
    };
    let outcome = if last["failed"].as_bool() == Some(true) {
        "failed"
    } else if last["acted"].as_bool() == Some(true) {
        "acted"
    } else {
        "no changes"
    };
    let dry = if last["dry_run"].as_bool() == Some(true) {
        ", dry run"
    } else {
        ""
    };
    format!("Last run {}: {outcome}{dry}", ago(started))
}

#[derive(Default)]
struct LiveSettings(Vec<(AnyWindowHandle, WeakEntity<IcarusSettings>)>);

impl gpui::Global for LiveSettings {}

/// Handles the server's `config.changed`: settings were written somewhere (CLI, an agent, an editor).
pub(crate) fn config_changed(cx: &mut App) {
    let open = cx
        .try_global::<LiveSettings>()
        .map(|open| open.0.clone())
        .unwrap_or_default();
    for (handle, settings) in open {
        handle
            .update(cx, |_, window, cx| {
                settings.update(cx, |settings, cx| settings.reload(window, cx))
            })
            .ok();
    }
    if cx.has_global::<LiveSettings>() {
        cx.update_global::<LiveSettings, _>(|open, _| {
            open.0.retain(|(_, settings)| settings.upgrade().is_some());
        });
    }
}

impl IcarusSettings {
    pub fn new(
        workspace_window: Option<WindowHandle<MultiWorkspace>>,
        root: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut editors = HashMap::new();
        let mut subscriptions = Vec::new();
        for field in [AUTOMATIONS, INTEGRATIONS, PORTS]
            .into_iter()
            .flatten()
            .filter(|field| matches!(field.control, Control::Text | Control::Integer))
        {
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(field.placeholder.unwrap_or("Not set"), window, cx);
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
        let linear_key = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_masked(true, cx);
            editor.set_placeholder_text("lin_api_…", window, cx);
            editor
        });
        let prompt_name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("New prompt id", window, cx);
            editor
        });
        let mut settings = Self {
            workspace_window,
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
            linear: None,
            linear_key,
            linear_busy: false,
            linear_error: None,
            automation: None,
            automation_error: None,
            automation_notes: HashMap::new(),
            automation_busy: None,
            script_rows: Vec::new(),
            prompt_rows: Vec::new(),
            prompt_name,
            prompt_renaming: None,
            prompt_preview: None,
            workspaces: Vec::new(),
            preview_workspace: None,
            _subscriptions: subscriptions,
        };
        let handle = window.window_handle();
        let weak = cx.weak_entity();
        cx.default_global::<LiveSettings>().0.push((handle, weak));
        settings.reload(window, cx);
        settings
    }

    /// The pages filled from a fixed snapshot, for screenshots and tests.
    pub fn preview(snapshot: Snapshot, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut settings = Self::new(None, None, window, cx);
        settings.loading = None;
        settings.apply(snapshot, window, cx);
        settings
    }

    pub fn scope(&self) -> Scope {
        self.scope
    }

    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    pub fn linear_status(&self) -> Option<&Value> {
        self.linear.as_ref()
    }

    fn load_linear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task) = request(self.client.clone(), "linear.status", json!({}), cx) else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let status = task.await;
            this.update(cx, |this, cx| {
                match status {
                    Ok(status) => this.linear = Some(status),
                    Err(error) => {
                        this.linear_error = Some(crate::changes_tab::git_error_text(&error))
                    }
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    pub fn save_linear_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.linear_key.read(cx).text(cx).trim().to_owned();
        if key.is_empty() {
            self.linear_error = Some("Paste a Linear API key first.".into());
            cx.notify();
            return;
        }
        self.run_linear("linear.set_api_key", json!({ "key": key }), window, cx);
    }

    pub fn remove_linear_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_linear("linear.clear_api_key", json!({}), window, cx);
    }

    fn run_linear(
        &mut self,
        method: &'static str,
        params: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = request(self.client.clone(), method, params, cx) else {
            self.linear_error = Some("Not connected to the Icarus server.".into());
            cx.notify();
            return;
        };
        self.linear_busy = true;
        self.linear_error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.linear_busy = false;
                match result {
                    Ok(status) => {
                        this.linear = Some(status);
                        this.linear_key
                            .update(cx, |editor, cx| editor.set_text("", window, cx));
                    }
                    Err(error) => {
                        this.linear_error = Some(crate::changes_tab::git_error_text(&error))
                    }
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_linear(window, cx);
        self.load_automation(window, cx);
        let client = self.client.clone();
        let root = self.root.clone();
        let keys: Vec<&'static str> = [
            AGENTS,
            AUTOMATIONS,
            INTEGRATIONS,
            NOTIFICATIONS,
            PORTS,
            UPDATES,
        ]
        .into_iter()
        .flatten()
        .map(|field| field.key)
        .chain(["workspace.statuses", "workspace.cardFields", OPEN_IN])
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
        self.sync_prompt_rows(window, cx);
        cx.notify();
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

    fn load_automation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let status = request(self.client.clone(), "automation.status", json!({}), cx);
        let workspaces = request(self.client.clone(), "workspace.list", json!({}), cx);
        cx.spawn_in(window, async move |this, cx| {
            let status = match status {
                Some(task) => Some(task.await),
                None => None,
            };
            let workspaces = match workspaces {
                Some(task) => task.await.ok(),
                None => None,
            };
            this.update_in(cx, |this, window, cx| {
                match status {
                    Some(Ok(status)) => {
                        this.automation = Some(status);
                        this.automation_error = None;
                    }
                    Some(Err(error)) => this.automation_error = Some(format!("{error:#}")),
                    None => {}
                }
                if let Some(list) = workspaces.as_ref().and_then(Value::as_array) {
                    this.workspaces = list
                        .iter()
                        .filter_map(|workspace| {
                            Some((
                                workspace["id"].as_str()?.to_owned(),
                                workspace["name"].as_str().unwrap_or_default().to_owned(),
                            ))
                        })
                        .collect();
                }
                this.sync_script_rows(window, cx);
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn automation_projects(&self) -> Vec<Value> {
        self.automation
            .as_ref()
            .and_then(|status| status["projects"].as_array())
            .cloned()
            .unwrap_or_default()
    }

    fn sync_script_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typing = self
            .script_rows
            .iter()
            .any(|row| row.path.focus_handle(cx).is_focused(window));
        if typing {
            return;
        }
        let saved: Vec<(String, String, String)> = self
            .automation_projects()
            .iter()
            .filter_map(|project| {
                Some((
                    project["id"].as_str()?.to_owned(),
                    project["project"].as_str().unwrap_or_default().to_owned(),
                    project["script"].as_str().unwrap_or_default().to_owned(),
                ))
            })
            .collect();
        let current: Vec<(&str, &str)> = self
            .script_rows
            .iter()
            .map(|row| (row.project_id.as_str(), row.saved.as_str()))
            .collect();
        let incoming: Vec<(&str, &str)> = saved
            .iter()
            .map(|(id, _, script)| (id.as_str(), script.as_str()))
            .collect();
        if current == incoming {
            return;
        }
        self.script_rows = saved
            .into_iter()
            .map(|(project_id, name, script)| {
                let path = cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text("No script", window, cx);
                    editor.set_text(script.as_str(), window, cx);
                    editor
                });
                let subscription = {
                    let project_id = project_id.clone();
                    cx.subscribe_in(&path, window, move |this, editor, event, window, cx| {
                        if matches!(event, EditorEvent::Blurred) {
                            let text = editor.read(cx).text(cx);
                            this.commit_script(&project_id, &name, text.trim(), window, cx);
                        }
                    })
                };
                ScriptRow {
                    project_id,
                    saved: script,
                    path,
                    _subscription: subscription,
                }
            })
            .collect();
    }

    fn commit_script(
        &mut self,
        project_id: &str,
        name: &str,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let unchanged = self
            .script_rows
            .iter()
            .any(|row| row.project_id == project_id && row.saved == text);
        if unchanged {
            return;
        }
        // A dot in the project name would split the settings key; the server also takes the id.
        let project = if name.is_empty() || name.contains('.') {
            project_id
        } else {
            name
        };
        let value = (!text.is_empty()).then(|| json!(text));
        // Script paths are relative to ~/.config/icarus, so they belong in the user's settings.
        self.write_to(
            Scope::User,
            &format!("{SCRIPTS}.{project}"),
            value,
            window,
            cx,
        );
    }

    fn run_automation(
        &mut self,
        note: String,
        method: &'static str,
        params: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = request(self.client.clone(), method, params, cx) else {
            self.automation_error = Some("Not connected to the Icarus server.".into());
            cx.notify();
            return;
        };
        self.automation_busy = Some(note.clone());
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.automation_busy = None;
                let text = match (method, result) {
                    (_, Err(error)) => format!("Failed: {error:#}"),
                    ("automation.run", Ok(reply)) => dry_run_summary(&reply),
                    (_, Ok(_)) => "Types regenerated.".to_owned(),
                };
                this.automation_notes.insert(note, text);
                this.load_automation(window, cx);
            })
            .log_err();
        })
        .detach();
    }

    pub fn dry_run(&mut self, project_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let params = json!({ "project": project_id, "dry_run": true });
        self.run_automation(project_id, "automation.run", params, window, cx);
    }

    pub fn regenerate_types(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_automation("schema".into(), "automation.schema", json!({}), window, cx);
    }

    fn prompts(&self) -> Vec<(String, String)> {
        let mut prompts: Vec<(String, String)> = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| lookup(&snapshot.resolved, PROMPTS))
            .and_then(Value::as_object)
            .map(|prompts| {
                prompts
                    .iter()
                    .map(|(id, template)| {
                        (id.clone(), template.as_str().unwrap_or_default().to_owned())
                    })
                    .collect()
            })
            .unwrap_or_default();
        for id in NEXT_ACTION_PROMPTS {
            if !prompts.iter().any(|(saved, _)| saved == id) {
                prompts.push((id.to_owned(), String::new()));
            }
        }
        prompts.sort();
        prompts
    }

    fn sync_prompt_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typing = self
            .prompt_rows
            .iter()
            .any(|row| row.template.focus_handle(cx).is_focused(window));
        if typing {
            return;
        }
        let saved = self.prompts();
        let current: Vec<(String, String)> = self
            .prompt_rows
            .iter()
            .map(|row| (row.id.clone(), row.template.read(cx).text(cx)))
            .collect();
        if current == saved {
            return;
        }
        self.prompt_rows = saved
            .into_iter()
            .map(|(id, text)| {
                let template = cx.new(|cx| {
                    let mut editor = Editor::auto_height(1, 8, window, cx);
                    editor.set_soft_wrap();
                    let placeholder = if NEXT_ACTION_PROMPTS.contains(&id.as_str()) {
                        "The built-in prompt for the next-action button's step"
                    } else {
                        "Template, e.g. Fix the checks of #{{pr.number}}"
                    };
                    editor.set_placeholder_text(placeholder, window, cx);
                    editor.set_text(text, window, cx);
                    editor
                });
                let subscription = {
                    let id = id.clone();
                    cx.subscribe_in(&template, window, move |this, editor, event, window, cx| {
                        if matches!(event, EditorEvent::Blurred) {
                            let text = editor.read(cx).text(cx);
                            this.commit_prompt(&id, text, window, cx);
                        }
                    })
                };
                PromptRow {
                    id,
                    template,
                    _subscription: subscription,
                }
            })
            .collect();
    }

    fn commit_prompt(
        &mut self,
        id: &str,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let unchanged = self
            .prompts()
            .iter()
            .any(|(saved_id, saved)| saved_id == id && *saved == text);
        if !unchanged {
            self.write(&format!("{PROMPTS}.{id}"), Some(json!(text)), window, cx);
        }
    }

    pub fn add_or_rename_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = slug(self.prompt_name.read(cx).text(cx).trim());
        if id.is_empty() {
            return;
        }
        let prompts = self.prompts();
        let renaming = self.prompt_renaming.take();
        if renaming.as_deref() == Some(id.as_str()) {
            self.prompt_name
                .update(cx, |editor, cx| editor.set_text("", window, cx));
            return;
        }
        if prompts.iter().any(|(existing, _)| *existing == id) {
            self.error = Some(format!("A prompt named \"{id}\" already exists."));
            cx.notify();
            return;
        }
        let template = renaming
            .as_ref()
            .and_then(|old| prompts.iter().find(|(existing, _)| existing == old))
            .map(|(_, template)| template.clone())
            .unwrap_or_default();
        self.prompt_name
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.write(
            &format!("{PROMPTS}.{id}"),
            Some(json!(template)),
            window,
            cx,
        );
        if let Some(old) = renaming {
            self.write(&format!("{PROMPTS}.{old}"), None, window, cx);
        }
    }

    fn remove_prompt(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.write(&format!("{PROMPTS}.{id}"), None, window, cx);
    }

    /// Renders what the editor holds now, so a preview doesn't wait for the save.
    pub fn preview_prompt(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self
            .preview_workspace
            .clone()
            .or_else(|| self.workspaces.first().map(|(id, _)| id.clone()))
        else {
            self.prompt_preview = Some((id, Err("No workspace to preview against.".into())));
            cx.notify();
            return;
        };
        let mut params = json!({ "id": id, "workspace": workspace });
        if let Some(row) = self.prompt_rows.iter().find(|row| row.id == id) {
            params["template"] = json!(row.template.read(cx).text(cx));
        }
        let Some(task) = request(self.client.clone(), "prompt.render", params, cx) else {
            self.prompt_preview = Some((id, Err("Not connected to the Icarus server.".into())));
            cx.notify();
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.prompt_preview = Some((
                    id,
                    result
                        .map(|reply| reply["prompt"].as_str().unwrap_or_default().to_owned())
                        .map_err(|error| format!("{error:#}")),
                ));
                cx.notify();
            })
            .log_err();
        })
        .detach();
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
        match crate::cards::without_field(&fields, id) {
            Some(hidden) => fields = hidden,
            None => fields.push(id.to_owned()),
        }
        self.write("workspace.cardFields", Some(json!(fields)), window, cx);
    }

    fn open_layer_file(&mut self, cx: &mut Context<Self>) {
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
        let Some(workspace_window) = self.workspace_window else {
            self.error = Some(format!("Open {} in an editor.", path.display()));
            cx.notify();
            return;
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).log_err();
        }
        if !path.exists() {
            std::fs::write(&path, "{\n}\n").log_err();
        }
        workspace_window
            .update(cx, |multi, window, cx| {
                window.activate_window();
                multi.workspace().update(cx, |workspace, cx| {
                    workspace
                        .open_abs_path(path, OpenOptions::default(), window, cx)
                        .detach_and_log_err(cx);
                });
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
                    text.lines()
                        .map(str::trim)
                        .find(|l| l.contains("Logged in") || l.contains("not logged"))
                        .unwrap_or(if output.status.success() {
                            "Signed in."
                        } else {
                            "Not signed in: run `gh auth login`."
                        })
                        .trim_start_matches(['✓', 'X', ' '])
                        .to_owned()
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

    fn text_field(editor: &Entity<Editor>, width: f32, cx: &App) -> impl IntoElement {
        div()
            .w(px(width))
            .px_1p5()
            .py_0p5()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .child(editor.clone())
    }

    fn describe(label: impl Into<SharedString>, help: impl Into<SharedString>) -> impl IntoElement {
        v_flex().min_w_0().child(Label::new(label.into())).child(
            Label::new(help.into())
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
    }

    fn render_rows(&self, fields: &'static [Field], cx: &mut Context<Self>) -> Vec<AnyElement> {
        fields
            .iter()
            .map(|field| self.render_row(field, cx))
            .collect()
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
                let on = value
                    .as_ref()
                    .and_then(Value::as_bool)
                    .unwrap_or(field.default_on);
                Switch::new(SharedString::from(format!("switch-{key}")), on.into())
                    .on_click(move |state, window, cx| {
                        let on = *state == ToggleState::Selected;
                        weak.update(cx, |settings, cx| {
                            settings.write(key, Some(json!(on)), window, cx)
                        })
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
                                    weak.update(cx, |settings, cx| {
                                        settings.write(key, Some(json!(option)), window, cx)
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
                                    weak.update(cx, |settings, cx| {
                                        settings.write(
                                            key,
                                            id.clone().map(|id| json!(id)),
                                            window,
                                            cx,
                                        )
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
                Some(editor) => Self::text_field(editor, 260., cx).into_any_element(),
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
                weak.update(cx, |settings, cx| settings.write(key, None, window, cx))
                    .log_err();
            })
        });
        h_flex()
            .w_full()
            .py_1p5()
            .gap_4()
            .justify_between()
            .child(Self::describe(field.label, field.help))
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

    fn render_scope(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_project = self.project.is_some();
        let scope = self.scope;
        v_flex()
            .pb_2()
            .child(
                h_flex()
                    .gap_4()
                    .justify_between()
                    .child(Self::describe(
                        "Icarus settings layer",
                        match scope {
                            Scope::User => {
                                "Changes go to your settings (~/.config/icarus/icarus.json)."
                            }
                            Scope::Project => {
                                "Changes go to the project's icarus.json, shared with everyone."
                            }
                            Scope::Local => {
                                "Changes go to the project's icarus.local.json, on this machine only."
                            }
                        },
                    ))
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_none()
                            .child(
                                ToggleButtonGroup::single_row(
                                    "icarus-settings-scope",
                                    [
                                        ToggleButtonSimple::new(
                                            "You",
                                            cx.listener(|settings, _, _, cx| {
                                                settings.set_scope(Scope::User, cx)
                                            }),
                                        ),
                                        ToggleButtonSimple::new(
                                            "Project",
                                            cx.listener(move |settings, _, _, cx| {
                                                if has_project {
                                                    settings.set_scope(Scope::Project, cx)
                                                }
                                            }),
                                        ),
                                        ToggleButtonSimple::new(
                                            "Local",
                                            cx.listener(move |settings, _, _, cx| {
                                                if has_project {
                                                    settings.set_scope(Scope::Local, cx)
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
                                    .on_click(cx.listener(|settings, _, _, cx| {
                                        settings.open_layer_file(cx)
                                    })),
                            ),
                    ),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .when(self.loading.is_some() && self.snapshot.is_none(), |this| {
                this.child(Label::new("Loading…").color(Color::Muted))
            })
    }

    fn render_section(&self, section: Section, cx: &mut Context<Self>) -> AnyElement {
        match section {
            Section::Statuses => self.render_statuses(cx).into_any_element(),
            Section::Scripts => v_flex()
                .children(self.render_rows(AUTOMATIONS, cx))
                .child(self.render_automation_projects(cx))
                .into_any_element(),
            Section::Prompts => self.render_prompts(cx).into_any_element(),
            Section::Cards => self.render_cards(cx).into_any_element(),
            Section::Ports => v_flex()
                .children(self.render_rows(PORTS, cx))
                .into_any_element(),
            Section::Layouts => self.render_layouts(cx).into_any_element(),
            Section::OpenIn => self.render_open_in(cx).into_any_element(),
            Section::Agents => v_flex()
                .children(self.render_rows(AGENTS, cx))
                .into_any_element(),
            Section::Integrations => v_flex()
                .child(
                    h_flex()
                        .py_1p5()
                        .gap_4()
                        .justify_between()
                        .child(Self::describe(
                            "GitHub",
                            self.gh
                                .clone()
                                .unwrap_or_else(|| "Uses your GitHub CLI login (gh).".to_owned()),
                        ))
                        .child(
                            ui::Button::new("icarus-settings-gh", "Check")
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|settings, _, window, cx| {
                                    settings.check_gh(window, cx)
                                })),
                        ),
                )
                .child(self.render_linear(cx))
                .children(self.render_rows(INTEGRATIONS, cx))
                .into_any_element(),
            Section::Notifications => v_flex()
                .children(self.render_rows(NOTIFICATIONS, cx))
                .child(self.render_kinds(cx))
                .into_any_element(),
            Section::Updates => v_flex()
                .children(self.render_rows(UPDATES, cx))
                .into_any_element(),
        }
    }

    fn render_layouts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let layouts = self
            .snapshot
            .as_ref()
            .map(|s| s.layouts.join(", "))
            .filter(|layouts| !layouts.is_empty())
            .unwrap_or_else(|| "None".to_owned());
        h_flex()
            .py_1p5()
            .gap_4()
            .justify_between()
            .child(Self::describe("Named tab layouts", layouts))
            .child(
                ui::Button::new("icarus-settings-layouts", "Edit in JSON")
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|settings, _, _, cx| settings.open_layer_file(cx))),
            )
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
                                                            weak.update(cx, |settings, cx| {
                                                                settings.restyle_status(
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
                                                            weak.update(cx, |settings, cx| {
                                                                settings.restyle_status(
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
                                            .update(cx, |settings, cx| {
                                                settings.renaming = Some(id);
                                                settings.status_editor.update(cx, |editor, cx| {
                                                    editor.set_text(label, window, cx)
                                                });
                                                window.focus(
                                                    &settings.status_editor.focus_handle(cx),
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
                                        up.update(cx, |settings, cx| {
                                            settings.move_status(&id_up, -1, window, cx)
                                        })
                                        .log_err();
                                    }),
                            )
                            .child(
                                IconButton::new(row("down"), IconName::ArrowDown)
                                    .icon_size(ui::IconSize::XSmall)
                                    .disabled(index + 1 == count)
                                    .on_click(move |_, window, cx| {
                                        down.update(cx, |settings, cx| {
                                            settings.move_status(&id_down, 1, window, cx)
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
                                            .update(cx, |settings, cx| {
                                                settings.remove_status(&id_remove, window, cx)
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
                    .child(Self::text_field(&self.status_editor, 260., cx))
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
                        .on_click(cx.listener(
                            |settings, _, window, cx| settings.add_or_rename_status(window, cx),
                        )),
                    ),
            )
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
                            weak.update(cx, |settings, cx| {
                                settings.write(&key, Some(json!(on)), window, cx)
                            })
                            .log_err();
                        },
                    ),
                )
        }))
    }

    fn render_open_in(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.open_in_rows.len();
        let reset = self.set_here(OPEN_IN).then(|| {
            ui::Button::new("open-in-reset", "Use the default")
                .label_size(LabelSize::Small)
                .on_click(cx.listener(|settings, _, window, cx| {
                    settings.write_to(Scope::User, OPEN_IN, None, window, cx)
                }))
        });
        v_flex()
            .child(
                h_flex()
                    .py_1p5()
                    .gap_4()
                    .justify_between()
                    .child(Self::describe(
                        "Open in apps",
                        "A changed file's \"Open in\" menu. A plain command gets the file's \
                         path; a command with spaces runs through the shell. Saved in your settings.",
                    ))
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
                                    .on_click(cx.listener(|settings, _, window, cx| {
                                        settings.add_open_in_app(window, cx)
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
                    .child(Self::text_field(&row.label, 160., cx))
                    .child(Self::text_field(&row.command, 240., cx))
                    .child(div().flex_1())
                    .child(
                        IconButton::new(id("up"), IconName::ArrowUp)
                            .icon_size(ui::IconSize::XSmall)
                            .disabled(index == 0)
                            .tooltip(Tooltip::text("Move up"))
                            .on_click(cx.listener(move |settings, _, window, cx| {
                                settings.move_open_in_app(index, true, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new(id("down"), IconName::ArrowDown)
                            .icon_size(ui::IconSize::XSmall)
                            .disabled(index + 1 == count)
                            .tooltip(Tooltip::text("Move down"))
                            .on_click(cx.listener(move |settings, _, window, cx| {
                                settings.move_open_in_app(index, false, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new(id("remove"), IconName::Trash)
                            .icon_size(ui::IconSize::XSmall)
                            .tooltip(Tooltip::text("Remove app"))
                            .on_click(cx.listener(move |settings, _, window, cx| {
                                settings.remove_open_in_app(index, window, cx)
                            })),
                    )
            }))
    }

    fn render_linear(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let stored = self
            .linear
            .as_ref()
            .is_some_and(|status| status["stored"].as_bool() == Some(true));
        let busy = self.linear_busy;
        v_flex()
            .py_1p5()
            .child(
                h_flex()
                    .w_full()
                    .gap_4()
                    .justify_between()
                    .child(Self::describe(
                        "Linear API key",
                        linear_summary(self.linear.as_ref()),
                    ))
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_none()
                            .child(Self::text_field(&self.linear_key, 200., cx))
                            .child(
                                ui::Button::new(
                                    "icarus-linear-save",
                                    if busy { "Checking…" } else { "Save" },
                                )
                                .label_size(LabelSize::Small)
                                .disabled(busy)
                                .on_click(cx.listener(
                                    |settings, _, window, cx| settings.save_linear_key(window, cx),
                                )),
                            )
                            .when(stored, |this| {
                                this.child(
                                    ui::Button::new("icarus-linear-remove", "Remove")
                                        .label_size(LabelSize::Small)
                                        .disabled(busy)
                                        .on_click(cx.listener(|settings, _, window, cx| {
                                            settings.remove_linear_key(window, cx)
                                        })),
                                )
                            }),
                    ),
            )
            .when_some(self.linear_error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
    }

    fn render_automation_projects(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let projects = self.automation_projects();
        let running = self
            .automation
            .as_ref()
            .map(|status| status["running"].as_bool() == Some(true));
        let busy = self.automation_busy.clone();
        v_flex()
            .child(
                h_flex()
                    .py_1p5()
                    .gap_4()
                    .justify_between()
                    .child(Self::describe(
                        "Scripts per project",
                        match running {
                            None => "Checking…",
                            Some(true) => {
                                "A run is in progress. Paths are relative to ~/.config/icarus and saved in your settings."
                            }
                            Some(false) => {
                                "Paths are relative to ~/.config/icarus and saved in your settings."
                            }
                        },
                    ))
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_none()
                            .children(self.automation_notes.get("schema").map(|note| {
                                Label::new(note.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                            }))
                            .child(
                                ui::Button::new("automation-schema", "Regenerate types")
                                    .label_size(LabelSize::Small)
                                    .disabled(busy.as_deref() == Some("schema"))
                                    .on_click(cx.listener(|settings, _, window, cx| {
                                        settings.regenerate_types(window, cx)
                                    })),
                            ),
                    ),
            )
            .when_some(self.automation_error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .when(self.automation.is_some() && projects.is_empty(), |this| {
                this.child(
                    Label::new("No projects yet.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .children(projects.iter().enumerate().map(|(index, project)| {
                let project_id = project["id"].as_str().unwrap_or_default().to_owned();
                let name = project["project"].as_str().unwrap_or("Project").to_owned();
                let missing = project["script"].is_string()
                    && project["script_exists"].as_bool() != Some(true);
                let row = self
                    .script_rows
                    .iter()
                    .find(|row| row.project_id == project_id);
                let note = self.automation_notes.get(&project_id).cloned();
                let running = busy.as_deref() == Some(project_id.as_str());
                v_flex()
                    .py_1()
                    .child(
                        h_flex()
                            .gap_4()
                            .justify_between()
                            .child(Self::describe(name, run_summary(project)))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .flex_none()
                                    .children(row.map(|row| Self::text_field(&row.path, 260., cx)))
                                    .child(
                                        ui::Button::new(
                                            SharedString::from(format!("automation-run-{index}")),
                                            if running { "Running…" } else { "Run now (dry run)" },
                                        )
                                        .label_size(LabelSize::Small)
                                        .disabled(running || project["script"].is_null())
                                        .on_click(cx.listener(move |settings, _, window, cx| {
                                            settings.dry_run(project_id.clone(), window, cx)
                                        })),
                                    ),
                            ),
                    )
                    .when(missing, |this| {
                        this.child(
                            Label::new("The script file doesn't exist.")
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        )
                    })
                    .children(note.map(|note| {
                        Label::new(note).size(LabelSize::Small).color(Color::Muted)
                    }))
            }))
    }

    fn render_prompts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = self
            .preview_workspace
            .as_ref()
            .and_then(|id| self.workspaces.iter().find(|(existing, _)| existing == id))
            .or_else(|| self.workspaces.first())
            .map_or_else(|| "No workspaces".to_owned(), |(_, name)| name.clone());
        let workspaces = self.workspaces.clone();
        let weak = cx.entity().downgrade();
        v_flex()
            .child(
                h_flex()
                    .py_1p5()
                    .gap_4()
                    .justify_between()
                    .child(Self::describe(
                        "Prompt templates",
                        "Scripts start agents with these by id; {{path}} substitutes from \
                         the automation scope, e.g. {{workspace.name}} or {{pr.number}}. \
                         commitAndPush, createPullRequest, readyForReview and reviewPullRequest \
                         are the next-action button's steps (empty uses the built-in prompt); \
                         they use {{branch}}, {{baseRef}}, {{worktreePath}}, {{prNumber}}, \
                         {{prUrl}}, {{prTitle}}, {{draftMode}}, {{templateRule}} and {{prInstructions}}.",
                    ))
                    .child(
                        PopoverMenu::new("prompt-preview-workspace")
                            .trigger(
                                ui::Button::new("prompt-preview-workspace-button", workspace)
                                    .label_size(LabelSize::Small)
                                    .end_icon(
                                        ui::Icon::new(IconName::ChevronDown)
                                            .size(ui::IconSize::XSmall),
                                    ),
                            )
                            .menu(move |window, cx| {
                                let weak = weak.clone();
                                let workspaces = workspaces.clone();
                                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                    for (id, name) in workspaces {
                                        let weak = weak.clone();
                                        menu = menu.entry(name, None, move |_, cx| {
                                            weak.update(cx, |settings, cx| {
                                                settings.preview_workspace = Some(id.clone());
                                                cx.notify();
                                            })
                                            .log_err();
                                        });
                                    }
                                    menu
                                }))
                            }),
                    ),
            )
            .when(self.prompt_rows.is_empty(), |this| {
                this.child(
                    Label::new("No prompts.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .children(self.prompt_rows.iter().map(|row| {
                let id = row.id.clone();
                let element_id = |suffix: &str| SharedString::from(format!("prompt-{suffix}-{id}"));
                let preview = self
                    .prompt_preview
                    .as_ref()
                    .filter(|(previewed, _)| *previewed == id)
                    .map(|(_, result)| result.clone());
                let (id_preview, id_rename, id_remove) = (id.clone(), id.clone(), id.clone());
                v_flex()
                    .py_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_start()
                            .justify_between()
                            .child(Label::new(id.clone()).size(LabelSize::Small))
                            .child(
                                h_flex()
                                    .gap_0p5()
                                    .flex_none()
                                    .child(Self::text_field(&row.template, 420., cx))
                                    .child(
                                        ui::Button::new(element_id("preview"), "Preview")
                                            .label_size(LabelSize::Small)
                                            .on_click(cx.listener(
                                                move |settings, _, window, cx| {
                                                    settings.preview_prompt(
                                                        id_preview.clone(),
                                                        window,
                                                        cx,
                                                    )
                                                },
                                            )),
                                    )
                                    .child(
                                        IconButton::new(element_id("rename"), IconName::Pencil)
                                            .icon_size(ui::IconSize::XSmall)
                                            .tooltip(Tooltip::text("Rename"))
                                            .on_click(cx.listener(
                                                move |settings, _, window, cx| {
                                                    settings.prompt_renaming =
                                                        Some(id_rename.clone());
                                                    let id = id_rename.clone();
                                                    settings.prompt_name.update(
                                                        cx,
                                                        |editor, cx| {
                                                            editor.set_text(id, window, cx)
                                                        },
                                                    );
                                                    window.focus(
                                                        &settings.prompt_name.focus_handle(cx),
                                                        cx,
                                                    );
                                                    cx.notify();
                                                },
                                            )),
                                    )
                                    .child(
                                        IconButton::new(element_id("remove"), IconName::Trash)
                                            .icon_size(ui::IconSize::XSmall)
                                            .tooltip(Tooltip::text("Remove"))
                                            .on_click(cx.listener(
                                                move |settings, _, window, cx| {
                                                    settings.remove_prompt(&id_remove, window, cx)
                                                },
                                            )),
                                    ),
                            ),
                    )
                    .children(preview.map(|preview| {
                        match preview {
                            Ok(text) => div()
                                .mt_1()
                                .p_2()
                                .rounded_sm()
                                .bg(cx.theme().colors().surface_background)
                                .child(Label::new(text).size(LabelSize::Small))
                                .into_any_element(),
                            Err(error) => Label::new(error)
                                .size(LabelSize::Small)
                                .color(Color::Error)
                                .into_any_element(),
                        }
                    }))
            }))
            .child(
                h_flex()
                    .pt_1()
                    .gap_2()
                    .child(Self::text_field(&self.prompt_name, 260., cx))
                    .child(
                        ui::Button::new(
                            "prompt-add",
                            if self.prompt_renaming.is_some() {
                                "Rename"
                            } else {
                                "Add prompt"
                            },
                        )
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(
                            |settings, _, window, cx| settings.add_or_rename_prompt(window, cx),
                        )),
                    ),
            )
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
            let on = crate::cards::without_field(&shown, field.id).is_some();
            h_flex()
                .py_1()
                .justify_between()
                .child(Label::new(field.label))
                .child(
                    Switch::new(SharedString::from(format!("card-{}", field.id)), on.into())
                        .on_click(move |_, window, cx| {
                            weak.update(cx, |settings, cx| {
                                settings.toggle_card_field(field.id, window, cx)
                            })
                            .log_err();
                        }),
                )
        }))
    }
}

pub struct SectionView {
    settings: Entity<IcarusSettings>,
    section: Section,
    _observation: Subscription,
}

impl SectionView {
    pub fn new(settings: Entity<IcarusSettings>, section: Section, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&settings, |_, _, cx| cx.notify());
        Self {
            settings,
            section,
            _observation: observation,
        }
    }
}

impl Render for SectionView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let section = self.section;
        self.settings.update(cx, |settings, cx| {
            v_flex()
                .w_full()
                .when(section.starts_page(), |this| {
                    this.child(settings.render_scope(cx))
                })
                .child(settings.render_section(section, cx))
        })
    }
}

pub struct PagePreview {
    sections: Vec<(Section, Entity<SectionView>)>,
}

impl PagePreview {
    pub fn new(
        page: &str,
        snapshot: Snapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = cx.new(|cx| IcarusSettings::preview(snapshot, window, cx));
        let sections = PAGES
            .iter()
            .find(|(title, _)| *title == page)
            .map(|(_, sections)| *sections)
            .unwrap_or_default()
            .iter()
            .map(|section| {
                let view = cx.new(|cx| SectionView::new(settings.clone(), *section, cx));
                (*section, view)
            })
            .collect();
        Self { sections }
    }
}

impl Render for PagePreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .px_8()
            .gap_4()
            .bg(cx.theme().colors().editor_background)
            .children(self.sections.iter().map(|(section, view)| {
                v_flex()
                    .child(
                        Label::new(section.title())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(view.clone())
            }))
    }
}

// One model per settings window, so its pages agree on the scope and the loaded values.
fn settings_for_window(
    workspace_window: Option<WindowHandle<MultiWorkspace>>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<IcarusSettings> {
    let handle = window.window_handle();
    let existing = cx.try_global::<LiveSettings>().and_then(|open| {
        open.0
            .iter()
            .find(|(window, _)| *window == handle)
            .and_then(|(_, settings)| settings.upgrade())
    });
    if let Some(existing) = existing {
        return existing;
    }
    let root = workspace_window
        .and_then(|handle| handle.read(cx).ok())
        .and_then(|multi| {
            multi
                .workspace()
                .read(cx)
                .root_paths(cx)
                .into_iter()
                .next()
                .map(|path| path.to_path_buf())
        });
    cx.new(|cx| IcarusSettings::new(workspace_window, root, window, cx))
}

fn build_page(
    sections: &'static [Section],
    workspace_window: Option<WindowHandle<MultiWorkspace>>,
    window: &mut Window,
    cx: &mut App,
) -> Vec<ExtensionSection> {
    let settings = settings_for_window(workspace_window, window, cx);
    sections
        .iter()
        .map(|section| ExtensionSection {
            title: section.title(),
            view: cx
                .new(|cx| SectionView::new(settings.clone(), *section, cx))
                .into(),
        })
        .collect()
}

pub fn open_section(section: Section, window: &mut Window, cx: &mut App) {
    settings_ui::open_settings_section(
        section.page(),
        Some(section.title()),
        window.window_handle().downcast::<MultiWorkspace>(),
        cx,
    );
}

pub fn open_linear_settings(window: &mut Window, cx: &mut App) {
    open_section(Section::Integrations, window, cx);
}

pub fn open_agent_settings(window: &mut Window, cx: &mut App) {
    open_section(Section::Agents, window, cx);
}

pub fn register(cx: &mut App) {
    for &(page, sections) in PAGES {
        settings_ui::register_extension_page(
            page,
            move |workspace_window, window, cx| build_page(sections, workspace_window, window, cx),
            cx,
        );
    }
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|_, _: &OpenSettings, window, cx| {
            settings_ui::open_settings_section(
                Section::Statuses.page(),
                None,
                window.window_handle().downcast::<MultiWorkspace>(),
                cx,
            );
        });
        workspace.register_action(|_, _: &CustomizeOpenInApps, window, cx| {
            open_section(Section::OpenIn, window, cx);
        });
        workspace.register_action(|_, _: &ConnectLinear, window, cx| {
            open_linear_settings(window, cx);
        });
        workspace.register_action(|_, _: &OpenAgentSettings, window, cx| {
            open_agent_settings(window, cx);
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
            "automations": { "enabled": true, "armed": false, "interval_ms": 60000 },
            "prompts": { "fix-ci": "Fix the checks of #{{pr.number}}" },
            "agents": { "default": "claude" },
            "notifications": { "enabled": true },
        })
    }

    fn init_settings(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    fn new_settings(window: &mut gpui::VisualTestContext) -> Entity<IcarusSettings> {
        window.update(|window, cx| cx.new(|cx| IcarusSettings::new(None, None, window, cx)))
    }

    #[gpui::test]
    fn a_settings_change_before_any_settings_window_opened_is_ignored(cx: &mut TestAppContext) {
        cx.update(config_changed);
    }

    #[gpui::test]
    async fn the_pages_read_layers_and_write_to_the_chosen_one(cx: &mut TestAppContext) {
        init_settings(cx);
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
                "config.explain" if params["key"] == "automations.armed" => {
                    Ok(json!({ "from": { "layer": "user", "path": "/cfg/icarus.json" } }))
                }
                "config.explain" => Ok(json!({ "from": { "layer": "default" } })),
                _ => Ok(json!({})),
            })
        });
        let window = cx.add_empty_window();
        let settings = new_settings(window);
        window.run_until_parked();
        settings.read_with(window, |settings, _| {
            let snapshot = settings.snapshot().expect("loaded");
            assert_eq!(snapshot.layouts, ["default", "review"]);
            assert_eq!(snapshot.kinds.len(), 2);
            assert_eq!(settings.statuses().len(), 2);
            assert_eq!(
                settings.prompts().len(),
                1 + NEXT_ACTION_PROMPTS.len(),
                "the next-action prompts are listed even when unset"
            );
            assert!(settings.set_here("automations.armed"));
            assert!(!settings.set_here("automations.enabled"));
            assert_eq!(
                settings.source_label("automations.enabled"),
                None,
                "defaults show no source"
            );
        });
        settings.update_in(window, |settings, window, cx| {
            settings.write("automations.armed", Some(json!(true)), window, cx);
        });
        window.run_until_parked();
        assert_eq!(
            calls_to(&calls, "config.set")[0],
            json!({ "key": "automations.armed", "layer": "user", "value": true })
        );

        settings.update_in(window, |settings, window, cx| {
            settings
                .status_editor
                .update(cx, |editor, cx| editor.set_text("On hold", window, cx));
            settings.add_or_rename_status(window, cx);
        });
        window.run_until_parked();
        let statuses = calls_to(&calls, "config.set")
            .into_iter()
            .find(|call| call["key"] == "workspace.statuses")
            .expect("statuses written");
        assert_eq!(
            statuses["value"][2],
            json!({ "id": "on-hold", "label": "On hold" })
        );
        assert_eq!(
            statuses["value"][1]["color"], "blue",
            "other statuses keep their style"
        );

        settings.update_in(window, |settings, window, cx| {
            settings.toggle_card_field("pr", window, cx);
            settings.project = Some("p-1".into());
            settings.set_scope(Scope::Project, cx);
            settings.write("automations.hold_status", Some(json!("todo")), window, cx);
            settings.write("automations.interval_ms", None, window, cx);
        });
        window.run_until_parked();
        let sets = calls_to(&calls, "config.set");
        assert!(
            sets.iter()
                .any(|call| call["key"] == "workspace.cardFields" && call["value"] == json!([]))
        );
        assert!(sets.iter().any(|call| {
            call["key"] == "automations.hold_status"
                && call["layer"] == "project"
                && call["project"] == "p-1"
        }));
        assert_eq!(
            calls_to(&calls, "config.unset")[0],
            json!({ "key": "automations.interval_ms", "layer": "project", "project": "p-1" })
        );
    }

    #[gpui::test]
    async fn the_automation_buttons_call_the_server(cx: &mut TestAppContext) {
        init_settings(cx);
        let calls = cx.update(|cx| {
            stub_rpc(cx, |method, _| match method {
                "config.get" => Ok(resolved()),
                "config.explain" => Ok(json!({ "from": { "layer": "default" } })),
                "automation.status" => Ok(json!({
                    "running": false,
                    "projects": [{ "id": "p-1", "project": "widgets", "script": "automations/widgets.ts", "script_exists": true, "last_run": null }],
                })),
                "workspace.list" => Ok(json!([{ "id": "ws-1", "name": "Fix login" }])),
                "automation.run" => {
                    Ok(json!({ "runs": [{ "error": null, "results": [{ "ok": true }] }] }))
                }
                "prompt.render" => Ok(json!({ "prompt": "Fix the checks of #7" })),
                _ => Ok(json!({})),
            })
        });
        let window = cx.add_empty_window();
        let settings = new_settings(window);
        window.run_until_parked();
        settings.read_with(window, |settings, cx| {
            assert_eq!(settings.script_rows.len(), 1);
            assert_eq!(
                settings.script_rows[0].path.read(cx).text(cx),
                "automations/widgets.ts"
            );
        });

        settings.update_in(window, |settings, window, cx| {
            settings.dry_run("p-1".into(), window, cx);
            settings.regenerate_types(window, cx);
            settings.preview_prompt("fix-ci".into(), window, cx);
        });
        window.run_until_parked();
        assert_eq!(
            calls_to(&calls, "automation.run"),
            [json!({ "project": "p-1", "dry_run": true })]
        );
        assert_eq!(calls_to(&calls, "automation.schema").len(), 1);
        assert_eq!(
            calls_to(&calls, "prompt.render")[0],
            json!({ "id": "fix-ci", "workspace": "ws-1", "template": "Fix the checks of #{{pr.number}}" })
        );
        settings.read_with(window, |settings, _| {
            assert_eq!(
                settings.automation_notes.get("p-1").map(String::as_str),
                Some("Dry run: 1 action, 0 errors")
            );
            assert_eq!(
                settings.automation_notes.get("schema").map(String::as_str),
                Some("Types regenerated.")
            );
            assert_eq!(
                settings.prompt_preview,
                Some(("fix-ci".into(), Ok("Fix the checks of #7".into())))
            );
        });

        settings.update_in(window, |settings, window, cx| {
            settings.commit_script("p-1", "widgets", "automations/other.ts", window, cx);
        });
        window.run_until_parked();
        assert!(calls_to(&calls, "config.set").iter().any(|call| {
            call["key"] == "automations.scripts.widgets"
                && call["layer"] == "user"
                && call["value"] == "automations/other.ts"
        }));
    }

    #[gpui::test]
    async fn a_settings_change_elsewhere_reloads_the_pages_but_not_mid_edit(
        cx: &mut TestAppContext,
    ) {
        init_settings(cx);
        let current = Arc::new(std::sync::Mutex::new(resolved()));
        cx.update(|cx| {
            let current = current.clone();
            stub_rpc(cx, move |method, _| match method {
                "config.get" => Ok(current.lock().expect("lock").clone()),
                "config.explain" => Ok(json!({ "from": { "layer": "default" } })),
                _ => Ok(json!({})),
            })
        });
        let window = cx.add_empty_window();
        let settings = new_settings(window);
        window.run_until_parked();
        settings.read_with(window, |settings, _| {
            assert_eq!(settings.statuses().len(), 2)
        });

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
        settings.read_with(window, |settings, _| {
            assert_eq!(settings.statuses().len(), 3)
        });

        let editor = settings.read_with(window, |settings, _| {
            settings
                .editors
                .values()
                .next()
                .expect("a text field")
                .clone()
        });
        window.update(|window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
            editor.update(cx, |editor, cx| editor.set_text("half typed", window, cx));
        });
        add_status("Archived");
        changed(window);
        window.run_until_parked();
        settings.read_with(window, |settings, _| {
            assert_eq!(settings.statuses().len(), 4)
        });
        window.update(|_, cx| {
            assert_eq!(
                editor.read(cx).text(cx),
                "half typed",
                "the field being typed in keeps its text"
            )
        });
    }

    #[gpui::test]
    async fn the_pages_of_one_settings_window_share_their_state(cx: &mut TestAppContext) {
        init_settings(cx);
        cx.update(|cx| {
            stub_rpc(cx, |method, _| match method {
                "config.get" => Ok(resolved()),
                _ => Ok(json!({})),
            });
        });
        let window = cx.add_empty_window();
        let pages = window.update(|window, cx| {
            PAGES
                .iter()
                .map(|(_, sections)| build_page(sections, None, window, cx))
                .collect::<Vec<_>>()
        });
        window.run_until_parked();
        let titles: Vec<Vec<&str>> = pages
            .iter()
            .map(|page| page.iter().map(|section| section.title).collect())
            .collect();
        assert_eq!(titles[0], ["Statuses", "Scripts", "Prompts"]);
        assert_eq!(Section::OpenIn.page(), "Workspaces");
        assert_eq!(Section::Integrations.page(), "Icarus");
        let live = window.update(|_, cx| {
            cx.global::<LiveSettings>()
                .0
                .iter()
                .filter(|(_, settings)| settings.upgrade().is_some())
                .count()
        });
        assert_eq!(live, 1);
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
        let window = cx.add_empty_window();
        let settings = new_settings(window);
        window.run_until_parked();
        let rows = |settings: &Entity<IcarusSettings>, window: &mut gpui::VisualTestContext| {
            settings.read_with(window, |settings, cx| {
                settings
                    .open_in_rows
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
        assert_eq!(rows(&settings, window), ["VS Code=code"], "Orca's default");

        settings.update_in(window, |settings, window, cx| {
            settings.project = Some("p-1".into());
            settings.set_scope(Scope::Project, cx);
            settings.add_open_in_app(window, cx);
        });
        window.run_until_parked();
        assert_eq!(rows(&settings, window), ["VS Code=code", "New app="]);
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
        settings.update_in(window, |settings, window, cx| {
            let row = &settings.open_in_rows[1];
            let (label, command) = (row.label.clone(), row.command.clone());
            label.update(cx, |editor, cx| editor.set_text("Xcode", window, cx));
            command.update(cx, |editor, cx| {
                editor.set_text(" open -a Xcode ", window, cx)
            });
            settings.commit_open_in(window, cx);
        });
        window.run_until_parked();
        assert_eq!(
            rows(&settings, window),
            ["VS Code=code", "Xcode=open -a Xcode"]
        );
        let apps = settings.read_with(window, |settings, _| settings.open_in_apps());
        assert_eq!(
            apps[0]["icon"], "vscode",
            "fields the editor doesn't show stay"
        );
        assert_eq!(apps[1]["id"], "custom-1");

        settings.update_in(window, |settings, window, cx| {
            settings.move_open_in_app(1, true, window, cx)
        });
        window.run_until_parked();
        assert_eq!(
            rows(&settings, window),
            ["Xcode=open -a Xcode", "VS Code=code"]
        );
        settings.update_in(window, |settings, window, cx| {
            settings.remove_open_in_app(1, window, cx)
        });
        window.run_until_parked();
        assert_eq!(rows(&settings, window), ["Xcode=open -a Xcode"]);
        let parsed = crate::source_control_settings::SourceControlSettings::from_settings(
            &current.lock().expect("lock")["sourceControl"],
        );
        assert_eq!(
            parsed.open_in[0].command, "open -a Xcode",
            "the menu reads it"
        );
    }

    #[gpui::test]
    async fn a_pasted_linear_key_is_saved_through_the_server_and_can_be_removed(
        cx: &mut TestAppContext,
    ) {
        init_settings(cx);
        let saved = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let calls = cx.update(|cx| {
            let saved = saved.clone();
            stub_rpc(cx, move |method, params| {
                let stored = json!({
                    "configured": true, "source": "stored", "stored": true,
                    "store": "Keychain", "viewer": "Ada", "rejected": false,
                });
                let missing = json!({
                    "configured": false, "source": null, "stored": false,
                    "store": "Keychain", "viewer": null, "rejected": false,
                });
                match method {
                    "config.get" => Ok(resolved()),
                    "linear.status" if saved.load(std::sync::atomic::Ordering::SeqCst) => {
                        Ok(stored)
                    }
                    "linear.status" => Ok(missing),
                    "linear.set_api_key" if params["key"] == "lin_api_wrong" => {
                        Err(anyhow::anyhow!(icarus_client::ClientError::Rpc(
                            icarus_protocol::RpcError::new(
                                "unauthorized",
                                "Linear rejected this API key; nothing was saved",
                            )
                        )))
                    }
                    "linear.set_api_key" => {
                        saved.store(true, std::sync::atomic::Ordering::SeqCst);
                        Ok(stored)
                    }
                    "linear.clear_api_key" => {
                        saved.store(false, std::sync::atomic::Ordering::SeqCst);
                        Ok(missing)
                    }
                    _ => Ok(json!({})),
                }
            })
        });
        let window = cx.add_empty_window();
        let settings = new_settings(window);
        window.run_until_parked();
        settings.read_with(window, |settings, _| {
            assert!(
                linear_summary(settings.linear_status()).starts_with("Not connected"),
                "{:?}",
                settings.linear_status()
            );
        });

        let paste = |key: &'static str, window: &mut gpui::VisualTestContext| {
            settings.update_in(window, |settings, window, cx| {
                settings
                    .linear_key
                    .update(cx, |editor, cx| editor.set_text(key, window, cx));
                settings.save_linear_key(window, cx);
            });
            window.run_until_parked();
        };
        paste("lin_api_wrong", window);
        settings.read_with(window, |settings, cx| {
            assert_eq!(
                settings.linear_error.as_deref(),
                Some("Linear rejected this API key; nothing was saved")
            );
            assert_eq!(settings.linear_key.read(cx).text(cx), "lin_api_wrong");
        });

        paste(" lin_api_good ", window);
        assert_eq!(
            calls_to(&calls, "linear.set_api_key").last(),
            Some(&json!({ "key": "lin_api_good" }))
        );
        settings.read_with(window, |settings, cx| {
            assert_eq!(settings.linear_error, None);
            assert_eq!(
                settings.linear_key.read(cx).text(cx),
                "",
                "the field is cleared"
            );
            assert_eq!(
                linear_summary(settings.linear_status()),
                "Connected as Ada (stored in Keychain)"
            );
        });

        settings.update_in(window, |settings, window, cx| {
            settings.remove_linear_key(window, cx)
        });
        window.run_until_parked();
        assert_eq!(calls_to(&calls, "linear.clear_api_key").len(), 1);
        settings.read_with(window, |settings, _| {
            assert!(linear_summary(settings.linear_status()).starts_with("Not connected"));
        });
    }

    #[test]
    fn the_linear_summary_names_where_the_key_comes_from() {
        assert_eq!(
            linear_summary(Some(
                &json!({ "configured": true, "source": "env", "viewer": "Ada" })
            )),
            "Connected as Ada (LINEAR_API_KEY on the server)"
        );
        assert_eq!(
            linear_summary(Some(&json!({ "configured": true, "source": "command" }))),
            "Connected (from the key command)"
        );
        assert_eq!(
            linear_summary(Some(
                &json!({ "configured": false, "source": "stored", "rejected": true })
            )),
            "Linear rejected the saved key; paste a new one."
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
    fn a_dry_run_counts_actions_and_errors() {
        let reply =
            json!({ "runs": [{ "error": null, "results": [{ "ok": true }, { "ok": false }] }] });
        assert_eq!(dry_run_summary(&reply), "Dry run: 2 actions, 1 error");
        let failed = json!({ "runs": [{ "error": "exit 1", "results": [] }] });
        assert_eq!(dry_run_summary(&failed), "Dry run failed: exit 1");
        assert_eq!(
            dry_run_summary(&json!({ "runs": [] })),
            "Nothing ran: the project has no script."
        );
    }

    #[test]
    fn a_project_without_runs_says_so() {
        assert_eq!(run_summary(&json!({ "last_run": null })), "Never run");
    }

    #[test]
    fn status_ids_come_from_their_names() {
        assert_eq!(slug("PM approval"), "pm-approval");
        assert_eq!(slug("  On hold! "), "on-hold");
    }
}
