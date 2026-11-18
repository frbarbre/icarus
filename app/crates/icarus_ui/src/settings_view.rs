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
        /// Opens Icarus's settings at the Linear API key.
        ConnectLinear,
        /// Opens Icarus's settings at the agents: the default harness and Remote Control.
        OpenAgentSettings,
    ]
);

const OPEN_IN: &str = "sourceControl.openIn";

/// The scrolled blocks of the view, so a block can be scrolled into view.
const AGENTS_BLOCK: usize = 1;
const LINEAR_BLOCK: usize = 2;
const OPEN_IN_BLOCK: usize = 3;

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

const PROMPTS: &str = "prompts";
const SCRIPTS: &str = "automations.scripts";

/// One prompt template in the prompts editor.
struct PromptRow {
    id: String,
    template: Entity<Editor>,
    _subscription: Subscription,
}

/// One project's script path in the automations section.
struct ScriptRow {
    project_id: String,
    saved: String,
    path: Entity<Editor>,
    _subscription: Subscription,
}

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
    /// The block to scroll to once the settings have loaded.
    reveal: Option<usize>,
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

/// One line on a project's last run and its shadow comparison with the built-in rules.
pub fn run_summary(project: &Value) -> String {
    let last = &project["last_run"];
    let last = match last["started_ms"].as_i64() {
        None => "Never run".to_owned(),
        Some(started) => {
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
    };
    let shadow = &project["shadow"];
    let compared = shadow["compared"].as_i64().unwrap_or(0);
    if compared == 0 {
        return last;
    }
    let differed = shadow["differed"].as_i64().unwrap_or(0);
    let since = shadow["since_ms"]
        .as_i64()
        .map(|since| format!(" since {}", ago(since)))
        .unwrap_or_default();
    format!("{last} · shadow: {differed} of {compared} ticks differed from the rules{since}")
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
            AUTOMATIONS,
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
            reveal: None,
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
        self.sync_prompt_rows(window, cx);
        if let Some(block) = self.reveal.take() {
            self.scroll_handle.scroll_to_top_of_item(block);
        }
        cx.notify();
    }

    /// Scrolls to a block, now or once the settings have loaded.
    fn reveal_block(&mut self, block: usize, cx: &mut Context<Self>) {
        if self.snapshot.is_some() {
            self.scroll_handle.scroll_to_top_of_item(block);
            cx.notify();
        } else {
            self.reveal = Some(block);
        }
    }

    pub fn reveal_agents(&mut self, cx: &mut Context<Self>) {
        self.reveal_block(AGENTS_BLOCK, cx);
    }

    pub fn reveal_open_in(&mut self, cx: &mut Context<Self>) {
        self.reveal_block(OPEN_IN_BLOCK, cx);
    }

    pub fn reveal_linear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reveal_block(LINEAR_BLOCK, cx);
        window.focus(&self.linear_key.focus_handle(cx), cx);
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
                    editor.set_placeholder_text(
                        "Template, e.g. Fix the checks of #{{pr.number}}",
                        window,
                        cx,
                    );
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
        match crate::cards::without_field(&fields, id) {
            Some(hidden) => fields = hidden,
            None => fields.push(id.to_owned()),
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
                let on = value
                    .as_ref()
                    .and_then(Value::as_bool)
                    .unwrap_or(field.default_on);
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
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(Label::new("Linear API key"))
                            .child(
                                Label::new(linear_summary(self.linear.as_ref()))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_none()
                            .child(
                                div()
                                    .w(px(200.))
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(cx.theme().colors().border_variant)
                                    .child(self.linear_key.clone()),
                            )
                            .child(
                                ui::Button::new(
                                    "icarus-linear-save",
                                    if busy { "Checking…" } else { "Save" },
                                )
                                .label_size(LabelSize::Small)
                                .disabled(busy)
                                .on_click(cx.listener(
                                    |view, _, window, cx| view.save_linear_key(window, cx),
                                )),
                            )
                            .when(stored, |this| {
                                this.child(
                                    ui::Button::new("icarus-linear-remove", "Remove")
                                        .label_size(LabelSize::Small)
                                        .disabled(busy)
                                        .on_click(cx.listener(|view, _, window, cx| {
                                            view.remove_linear_key(window, cx)
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
        let field = |editor: &Entity<Editor>, cx: &App| {
            div()
                .w(px(260.))
                .px_1p5()
                .py_0p5()
                .rounded_sm()
                .border_1()
                .border_color(cx.theme().colors().border_variant)
                .child(editor.clone())
        };
        v_flex()
            .child(
                h_flex()
                    .py_1p5()
                    .gap_4()
                    .justify_between()
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(Label::new("Scripts per project"))
                            .child(
                                Label::new(match running {
                                    None => "Checking…".to_owned(),
                                    Some(true) => "A run is in progress. Paths are relative to ~/.config/icarus and saved in your settings.".to_owned(),
                                    Some(false) => "Paths are relative to ~/.config/icarus and saved in your settings.".to_owned(),
                                })
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    )
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
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.regenerate_types(window, cx)
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
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .child(Label::new(name))
                                    .child(
                                        Label::new(run_summary(project))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .flex_none()
                                    .children(row.map(|row| field(&row.path, cx)))
                                    .child(
                                        ui::Button::new(
                                            SharedString::from(format!("automation-run-{index}")),
                                            if running { "Running…" } else { "Run now (dry run)" },
                                        )
                                        .label_size(LabelSize::Small)
                                        .disabled(running || project["script"].is_null())
                                        .on_click(cx.listener(move |view, _, window, cx| {
                                            view.dry_run(project_id.clone(), window, cx)
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
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(Label::new("Prompt templates"))
                            .child(
                                Label::new(
                                    "Scripts start agents with these by id; {{path}} substitutes from \
                                     the automation scope, e.g. {{workspace.name}} or {{pr.number}}.",
                                )
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    )
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
                                            weak.update(cx, |view, cx| {
                                                view.preview_workspace = Some(id.clone());
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
                                    .child(
                                        div()
                                            .w(px(420.))
                                            .px_1p5()
                                            .py_0p5()
                                            .rounded_sm()
                                            .border_1()
                                            .border_color(cx.theme().colors().border_variant)
                                            .child(row.template.clone()),
                                    )
                                    .child(
                                        ui::Button::new(element_id("preview"), "Preview")
                                            .label_size(LabelSize::Small)
                                            .on_click(cx.listener(move |view, _, window, cx| {
                                                view.preview_prompt(id_preview.clone(), window, cx)
                                            })),
                                    )
                                    .child(
                                        IconButton::new(element_id("rename"), IconName::Pencil)
                                            .icon_size(ui::IconSize::XSmall)
                                            .tooltip(Tooltip::text("Rename"))
                                            .on_click(cx.listener(move |view, _, window, cx| {
                                                view.prompt_renaming = Some(id_rename.clone());
                                                let id = id_rename.clone();
                                                view.prompt_name.update(cx, |editor, cx| {
                                                    editor.set_text(id, window, cx)
                                                });
                                                window.focus(&view.prompt_name.focus_handle(cx), cx);
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        IconButton::new(element_id("remove"), IconName::Trash)
                                            .icon_size(ui::IconSize::XSmall)
                                            .tooltip(Tooltip::text("Remove"))
                                            .on_click(cx.listener(move |view, _, window, cx| {
                                                view.remove_prompt(&id_remove, window, cx)
                                            })),
                                    ),
                            ),
                    )
                    .children(preview.map(|preview| match preview {
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
                    }))
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
                            .child(self.prompt_name.clone()),
                    )
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
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.add_or_rename_prompt(window, cx)
                        })),
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
        let automations = rows(AUTOMATIONS, self, cx);
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
                    .child(Self::section("AUTOMATIONS", cx))
                    .children(automations)
                    .child(self.render_automation_projects(cx))
                    .child(Self::section("PROMPTS", cx))
                    .child(self.render_prompts(cx))
                    .child(
                        div()
                            .pt_6()
                            .child(Label::new("Built-in rules (being replaced by automation scripts)")),
                    )
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
                    .children(action_prompts),
            )
            .child(
                column()
                    .child(Self::section("AGENTS", cx))
                    .children(agents),
            )
            .child(
                column()
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
                    .child(self.render_linear(cx))
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

/// Shows the settings tab at the Linear API key field.
pub fn open_linear_settings(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open_settings(workspace, window, cx).update(cx, |view, cx| view.reveal_linear(window, cx));
}

pub fn open_agent_settings(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open_settings(workspace, window, cx).update(cx, |view, cx| view.reveal_agents(cx));
}

pub fn register(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenSettings, window, cx| {
            open_settings(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &CustomizeOpenInApps, window, cx| {
            open_settings(workspace, window, cx).update(cx, |view, cx| view.reveal_open_in(cx));
        });
        workspace.register_action(|workspace, _: &ConnectLinear, window, cx| {
            open_linear_settings(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenAgentSettings, window, cx| {
            open_agent_settings(workspace, window, cx);
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
            assert!(view.reveal.is_none(), "scrolled once the settings loaded");
            assert_eq!(view.open_in_rows.len(), 1);
        });
        cx.dispatch_action(CustomizeOpenInApps);
        cx.run_until_parked();
        assert_eq!(open(&mut cx).len(), 1, "the open tab is reused");
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
        let view = window.update(|window, cx| {
            cx.new(|cx| SettingsView::new(WeakEntity::new_invalid(), None, window, cx))
        });
        window.run_until_parked();
        view.read_with(window, |view, _| {
            assert!(
                linear_summary(view.linear_status()).starts_with("Not connected"),
                "{:?}",
                view.linear_status()
            );
        });

        let paste = |key: &'static str, window: &mut gpui::VisualTestContext| {
            view.update_in(window, |view, window, cx| {
                view.linear_key
                    .update(cx, |editor, cx| editor.set_text(key, window, cx));
                view.save_linear_key(window, cx);
            });
            window.run_until_parked();
        };
        paste("lin_api_wrong", window);
        view.read_with(window, |view, cx| {
            assert_eq!(
                view.linear_error.as_deref(),
                Some("Linear rejected this API key; nothing was saved")
            );
            assert_eq!(view.linear_key.read(cx).text(cx), "lin_api_wrong");
        });

        paste(" lin_api_good ", window);
        assert_eq!(
            calls_to(&calls, "linear.set_api_key").last(),
            Some(&json!({ "key": "lin_api_good" }))
        );
        view.read_with(window, |view, cx| {
            assert_eq!(view.linear_error, None);
            assert_eq!(
                view.linear_key.read(cx).text(cx),
                "",
                "the field is cleared"
            );
            assert_eq!(
                linear_summary(view.linear_status()),
                "Connected as Ada (stored in Keychain)"
            );
        });

        view.update_in(window, |view, window, cx| {
            view.remove_linear_key(window, cx)
        });
        window.run_until_parked();
        assert_eq!(calls_to(&calls, "linear.clear_api_key").len(), 1);
        view.read_with(window, |view, _| {
            assert!(linear_summary(view.linear_status()).starts_with("Not connected"));
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
        let project = json!({ "last_run": null, "shadow": { "compared": 0, "differed": 0, "since_ms": null } });
        assert_eq!(run_summary(&project), "Never run");
    }

    #[test]
    fn status_ids_come_from_their_names() {
        assert_eq!(slug("PM approval"), "pm-approval");
        assert_eq!(slug("  On hold! "), "on-hold");
    }
}
