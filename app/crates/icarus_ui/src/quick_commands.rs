//! Quick commands (features.md §2.15): saved terminal commands from the `quickCommands` setting,
//! run the way Orca does: a command already running in its terminal is stopped (Ctrl-C), otherwise
//! it's typed into an idle terminal of the workspace, otherwise a new terminal tab runs it.
//! The title bar's Run button repeats the last one; its menu searches, runs, adds, edits and deletes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use collections::HashSet;
use editor::Editor;
use gpui::{
    App, ClipboardItem, Context, DismissEvent, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, Global, PromptLevel, Render, Task, TaskExt as _, WeakEntity, Window,
};
use gpui_tokio::Tokio;
use icarus_client::Client;
use picker::{Picker, PickerDelegate};
use project::Project;
use serde::Deserialize;
use serde_json::{Value, json};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use ui::{
    Checkbox, ListItem, ListItemSpacing, PopoverMenu, PopoverMenuHandle, ToggleState, Tooltip,
    prelude::*,
};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::lifecycle::{modal_frame, render_error, render_field};
use crate::palettes::{Palette, PaletteItem};

gpui::actions!(
    icarus,
    [
        /// Runs this workspace's most recent quick command again (or stops it while it runs).
        RunQuickCommand,
        /// Lists this workspace's quick commands to pick one to run.
        OpenQuickCommandsMenu,
    ]
);

const REFRESH: Duration = Duration::from_secs(5);

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct QuickCommand {
    pub id: String,
    pub label: String,
    pub command: String,
    #[serde(rename = "appendEnter", default = "default_true")]
    pub append_enter: bool,
    #[serde(default)]
    pub scope: Value,
}

impl QuickCommand {
    fn text(&self) -> String {
        // Lines are separate shell commands; one list keeps a foreground program from reading later ones.
        let command = self
            .command
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        if self.append_enter {
            format!("{command}\r")
        } else {
            command
        }
    }

    pub fn project_only(&self) -> bool {
        self.scope["type"].as_str() == Some("project")
    }
}

/// Orca's quick command search: label matches rank above command-line matches; exact, then
/// prefix, then word start, then anywhere. Returns the matching indices, best first.
pub fn search(commands: &[QuickCommand], query: &str) -> Vec<usize> {
    let query = normalize(query);
    if query.is_empty() {
        return (0..commands.len()).collect();
    }
    let mut ranked: Vec<(usize, usize)> = commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            let label = score(&query, &normalize(&command.label), 0);
            let body = score(&query, &normalize(&command.command), 400);
            label.into_iter().chain(body).min().map(|s| (s, index))
        })
        .collect();
    ranked.sort();
    ranked.into_iter().map(|(_, index)| index).collect()
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn score(query: &str, candidate: &str, base: usize) -> Option<usize> {
    if candidate.is_empty() {
        None
    } else if candidate == query {
        Some(base)
    } else if candidate.starts_with(query) {
        Some(base + 50)
    } else if let Some(index) = candidate.find(&format!(" {query}")) {
        Some(base + 100 + index)
    } else {
        candidate.find(query).map(|index| base + 200 + index)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ServerTerminal {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub lost: bool,
    #[serde(default)]
    pub busy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Interrupt(String),
    Send(String),
    NewTerminal,
}

/// Orca's order: stop this command where it runs; else type it into an idle terminal shown in
/// this window, preferring one it ran in before; else open a new terminal for it.
pub fn decide(
    command: &QuickCommand,
    workspace_id: &str,
    terminals: &[ServerTerminal],
    shown: &HashSet<String>,
) -> Decision {
    let candidates: Vec<&ServerTerminal> = terminals
        .iter()
        .filter(|t| t.workspace.as_deref() == Some(workspace_id) && t.running && !t.lost)
        .filter(|t| shown.contains(&t.id))
        .collect();
    let own = |t: &&ServerTerminal| t.label.as_deref() == Some(command.label.as_str());
    if let Some(running) = candidates.iter().find(|t| own(t) && t.busy) {
        return Decision::Interrupt(running.id.clone());
    }
    candidates
        .iter()
        .filter(|t| !t.busy)
        .min_by_key(|t| !own(t))
        .map(|t| Decision::Send(t.id.clone()))
        .unwrap_or(Decision::NewTerminal)
}

/// The quick command each workspace ran last, so ⌘R and the Run button repeat it.
#[derive(Default)]
struct RecentCommands(HashMap<String, String>);

impl Global for RecentCommands {}

fn remember(workspace_id: &str, command_id: &str, cx: &mut App) {
    cx.default_global::<RecentCommands>()
        .0
        .insert(workspace_id.to_owned(), command_id.to_owned());
}

/// The workspace's last command if it still exists, else its first one.
pub fn most_recent<'a>(
    commands: &'a [QuickCommand],
    recent_id: Option<&str>,
) -> Option<&'a QuickCommand> {
    recent_id
        .and_then(|id| commands.iter().find(|command| command.id == id))
        .or_else(|| commands.first())
}

pub struct Target {
    client: Option<Arc<Client>>,
    workspace_id: String,
    path: PathBuf,
    /// The canonical remote of the workspace's project, which project-only commands are scoped to.
    remote: Option<String>,
    commands: Vec<QuickCommand>,
}

impl Target {
    /// A target that never contacts a server, for visual tests.
    pub fn preview(workspace_id: &str, remote: Option<&str>, commands: Vec<QuickCommand>) -> Self {
        Self {
            client: None,
            workspace_id: workspace_id.to_owned(),
            path: PathBuf::new(),
            remote: remote.map(str::to_owned),
            commands,
        }
    }

    fn recent<'a>(&'a self, cx: &'a App) -> Option<&'a QuickCommand> {
        let recent = cx
            .try_global::<RecentCommands>()
            .and_then(|recent| recent.0.get(&self.workspace_id))
            .map(String::as_str);
        most_recent(&self.commands, recent)
    }
}

#[derive(Deserialize)]
struct ListReply {
    commands: Vec<QuickCommand>,
    remote: Option<String>,
}

/// The Icarus workspace behind this window and its quick commands.
async fn load_target(client: Arc<Client>, roots: Vec<PathBuf>) -> Result<Option<Target>> {
    let workspaces = client.request("workspace.list", json!({})).await?;
    let found = workspaces.as_array().into_iter().flatten().find_map(|w| {
        let path = PathBuf::from(w["path"].as_str()?);
        roots
            .iter()
            .any(|root| root.starts_with(&path))
            .then(|| Some((w["id"].as_str()?.to_string(), path)))
            .flatten()
    });
    let Some((workspace_id, path)) = found else {
        return Ok(None);
    };
    let reply = client
        .request("quick_command.list", json!({ "workspace": workspace_id }))
        .await?;
    let reply: ListReply = serde_json::from_value(reply)?;
    Ok(Some(Target {
        client: Some(client),
        workspace_id,
        path,
        remote: reply.remote,
        commands: reply.commands,
    }))
}

fn shown_terminals(workspace: &Workspace, cx: &App) -> HashSet<String> {
    workspace
        .items_of_type::<TerminalView>(cx)
        .filter_map(|view| {
            view.read(cx)
                .terminal()
                .read(cx)
                .external_id()
                .map(str::to_string)
        })
        .collect()
}

fn activate_terminal(
    id: &str,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let view = workspace
        .items_of_type::<TerminalView>(cx)
        .find(|view| view.read(cx).terminal().read(cx).external_id() == Some(id));
    if let Some(view) = view {
        workspace.activate_item(&view, true, true, window, cx);
    }
}

/// Runs `command` in the window's Icarus workspace with Orca's semantics.
fn run(
    target: Arc<Target>,
    command: QuickCommand,
    workspace: &Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(client) = target.client.clone() else {
        return;
    };
    remember(&target.workspace_id, &command.id, cx);
    // `workspace` is the entity being updated here, so it's read through the borrow, not the handle.
    let shown = shown_terminals(workspace, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let terminals = Tokio::spawn_result(cx, {
            let client = client.clone();
            async move { client.request("terminal.list", json!({})).await }
        })
        .await?;
        let terminals: Vec<ServerTerminal> = serde_json::from_value(terminals)?;
        let decision = decide(&command, &target.workspace_id, &terminals, &shown);
        match decision {
            Decision::Interrupt(id) => {
                Tokio::spawn_result(cx, async move {
                    client
                        .request("terminal.write", json!({ "id": id, "text": "\u{3}" }))
                        .await
                })
                .await?;
            }
            Decision::Send(id) => {
                let text = command.text();
                let label = command.label.clone();
                let sent = id.clone();
                Tokio::spawn_result(cx, async move {
                    client
                        .request("terminal.write", json!({ "id": sent, "text": text }))
                        .await?;
                    client
                        .request("terminal.set_label", json!({ "id": sent, "label": label }))
                        .await
                })
                .await?;
                workspace.update_in(cx, |workspace, window, cx| {
                    activate_terminal(&id, workspace, window, cx)
                })?;
            }
            Decision::NewTerminal => {
                let text = command.text();
                let label = command.label.clone();
                let path = target.path.clone();
                let workspace_id = target.workspace_id.clone();
                let id = Tokio::spawn_result(cx, async move {
                    let created = client
                        .request(
                            "terminal.create",
                            json!({ "cwd": path, "workspace": workspace_id, "label": label }),
                        )
                        .await?;
                    let id = created["id"]
                        .as_str()
                        .context("the server returned no terminal id")?
                        .to_string();
                    client
                        .request("terminal.write", json!({ "id": id, "text": text }))
                        .await?;
                    Ok(id)
                })
                .await?;
                let cwd = target.path.clone();
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        TerminalPanel::add_center_terminal(
                            workspace,
                            window,
                            cx,
                            move |project, cx| {
                                project.create_terminal_shell_reattaching(Some(cwd), Some(id), cx)
                            },
                        )
                    })?
                    .await?;
            }
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn run_in_window(target: Arc<Target>, command: QuickCommand, window: &mut Window, cx: &mut App) {
    let Some(workspace) = Workspace::for_window(window, cx) else {
        return;
    };
    workspace.update(cx, |workspace, cx| {
        run(target, command, workspace, window, cx)
    });
}

/// Loads the window's quick commands, then hands them to `then`.
fn with_target(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    then: impl FnOnce(Arc<Target>, &mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    let Some(client) = crate::routing::client_for_project(workspace.project().read(cx), cx) else {
        log::warn!("quick commands need the Icarus server");
        return;
    };
    let roots: Vec<PathBuf> = workspace
        .root_paths(cx)
        .into_iter()
        .map(|root| Path::to_path_buf(&root))
        .collect();
    cx.spawn_in(window, async move |workspace, cx| {
        let target = Tokio::spawn_result(cx, load_target(client, roots))
            .await?
            .context("this window isn't an Icarus workspace")?;
        let target = Arc::new(target);
        workspace.update_in(cx, |workspace, window, cx| {
            then(target, workspace, window, cx)
        })
    })
    .detach_and_log_err(cx);
}

fn project_paths(project: &WeakEntity<Project>, cx: &App) -> Vec<PathBuf> {
    project
        .upgrade()
        .map(|project| {
            project
                .read(cx)
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect()
        })
        .unwrap_or_default()
}

/// Each window's button, so ⌃P opens the menu of the window it was pressed in.
#[derive(Default)]
/// Keyed by project: a window holds one title bar, and so one button, per workspace.
struct QuickCommandsButtons(HashMap<EntityId, WeakEntity<QuickCommandsButton>>);

impl Global for QuickCommandsButtons {}

/// The title bar's "▶ Run" split button and its quick commands menu.
pub struct QuickCommandsButton {
    project: Option<WeakEntity<Project>>,
    target: Option<Arc<Target>>,
    menu: PopoverMenuHandle<Picker<QuickCommandsDelegate>>,
    picker: Option<WeakEntity<Picker<QuickCommandsDelegate>>>,
    _poll: Task<()>,
}

impl QuickCommandsButton {
    pub fn new(project: Entity<Project>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let this = cx.entity().downgrade();
        let buttons = &mut cx.default_global::<QuickCommandsButtons>().0;
        buttons.retain(|_, button| button.upgrade().is_some());
        buttons.insert(project.entity_id(), this);
        let mut button = Self {
            project: Some(project.downgrade()),
            target: None,
            menu: PopoverMenuHandle::default(),
            picker: None,
            _poll: Task::ready(()),
        };
        if crate::enabled(cx) {
            button._poll = cx.spawn(async move |this, cx| {
                loop {
                    if this.update(cx, |this, cx| this.reload(cx)).is_err() {
                        return;
                    }
                    cx.background_executor().timer(REFRESH).await;
                }
            });
        }
        button
    }

    /// A button that never contacts a server and lists `target`'s commands, for visual tests.
    pub fn preview(target: Target) -> Self {
        Self {
            project: None,
            target: Some(Arc::new(target)),
            menu: PopoverMenuHandle::default(),
            picker: None,
            _poll: Task::ready(()),
        }
    }

    pub fn menu_handle(&self) -> PopoverMenuHandle<Picker<QuickCommandsDelegate>> {
        self.menu.clone()
    }

    pub fn commands(&self) -> &[QuickCommand] {
        self.target
            .as_ref()
            .map_or(&[], |target| target.commands.as_slice())
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
            return;
        };
        let Some(client) = crate::routing::client_for_weak_project(&project, cx) else {
            return;
        };
        let roots = project_paths(&project, cx);
        let load = Tokio::spawn_result(cx, load_target(client, roots));
        cx.spawn(async move |this, cx| {
            let target = load.await.log_err().flatten().map(Arc::new);
            this.update(cx, |this, cx| this.set_target(target, cx))
        })
        .detach_and_log_err(cx);
    }

    fn set_target(&mut self, target: Option<Arc<Target>>, cx: &mut Context<Self>) {
        let changed = match (&self.target, &target) {
            (Some(old), Some(new)) => {
                old.workspace_id != new.workspace_id
                    || old.commands != new.commands
                    || old.remote != new.remote
            }
            (None, None) => false,
            _ => true,
        };
        if !changed {
            return;
        }
        self.target = target.clone();
        if let (Some(picker), Some(target)) =
            (self.picker.as_ref().and_then(|p| p.upgrade()), target)
        {
            picker.update(cx, |picker, cx| {
                picker.delegate.target = target;
                picker.delegate.refilter();
                cx.notify();
            });
        }
        cx.notify();
    }

    fn run_recent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            return;
        };
        if let Some(command) = target.recent(cx).cloned() {
            run_in_window(target, command, window, cx);
            cx.notify();
        }
    }

    fn picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Picker<QuickCommandsDelegate>>> {
        let target = self.target.clone()?;
        let delegate = QuickCommandsDelegate::new(cx.entity().downgrade(), target, cx);
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .initial_width(rems(24.))
                .show_scrollbar(true)
                .popover()
        });
        self.picker = Some(picker.downgrade());
        self.reload(cx);
        Some(picker)
    }

    fn open_editor(
        &mut self,
        existing: Option<QuickCommand>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let Some(workspace) = Workspace::for_window(window, cx) else {
            return;
        };
        let button = cx.entity().downgrade();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                QuickCommandEditor::new(button, target, existing, window, cx)
            });
        });
    }

    fn delete(&mut self, command: QuickCommand, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self
            .target
            .as_ref()
            .and_then(|target| target.client.clone())
        else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete \"{}\"?", command.label),
            Some("This quick command will be removed from your saved list."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await? != 0 {
                return Ok(());
            }
            Tokio::spawn_result(cx, async move {
                client
                    .request("quick_command.delete", json!({ "id": command.id }))
                    .await
            })
            .await?;
            this.update(cx, |this, cx| this.reload(cx))
        })
        .detach_and_log_err(cx);
    }
}

impl Render for QuickCommandsButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(target) = self.target.clone() else {
            return div().into_any_element();
        };
        let Some(recent) = target.recent(cx).cloned() else {
            return Button::new("icarus-quick-command-add", "Command")
                .start_icon(Icon::new(IconName::PlayFilled).size(IconSize::XSmall))
                .label_size(LabelSize::Small)
                .tooltip(Tooltip::text("Save a quick command for this project"))
                .on_click(cx.listener(|this, _, window, cx| this.open_editor(None, window, cx)))
                .into_any_element();
        };
        let this = cx.entity();
        h_flex()
            .child(
                Button::new("icarus-quick-command-run", recent.label.clone())
                    .start_icon(Icon::new(IconName::PlayFilled).size(IconSize::XSmall))
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text(format!("Run {}", recent.command)))
                    .on_click(cx.listener(|this, _, window, cx| this.run_recent(window, cx))),
            )
            .child(
                PopoverMenu::new("icarus-quick-commands-menu")
                    .menu(move |window, cx| this.update(cx, |this, cx| this.picker(window, cx)))
                    .with_handle(self.menu.clone())
                    .anchor(gpui::Anchor::TopRight)
                    .offset(gpui::Point {
                        x: px(0.),
                        y: px(2.),
                    })
                    .trigger_with_tooltip(
                        IconButton::new("icarus-quick-commands-more", IconName::ChevronDown)
                            .icon_size(IconSize::XSmall),
                        Tooltip::for_action_title("Quick commands", &OpenQuickCommandsMenu),
                    ),
            )
            .into_any_element()
    }
}

pub struct QuickCommandsDelegate {
    button: WeakEntity<QuickCommandsButton>,
    target: Arc<Target>,
    query: String,
    matches: Vec<usize>,
    selected_index: usize,
}

impl QuickCommandsDelegate {
    fn new(button: WeakEntity<QuickCommandsButton>, target: Arc<Target>, cx: &App) -> Self {
        let selected = target
            .recent(cx)
            .and_then(|recent| target.commands.iter().position(|c| c.id == recent.id))
            .unwrap_or_default();
        let mut delegate = Self {
            button,
            target,
            query: String::new(),
            matches: Vec::new(),
            selected_index: selected,
        };
        delegate.refilter();
        delegate
    }

    fn refilter(&mut self) {
        self.matches = search(&self.target.commands, &self.query);
        self.selected_index = self
            .selected_index
            .min(self.matches.len().saturating_sub(1));
    }

    fn command(&self, index: usize) -> Option<&QuickCommand> {
        self.target.commands.get(*self.matches.get(index)?)
    }

    /// The labels of the rows now shown, in order.
    pub fn visible_labels(&self) -> Vec<String> {
        (0..self.matches.len())
            .filter_map(|index| self.command(index).map(|command| command.label.clone()))
            .collect()
    }
}

impl PickerDelegate for QuickCommandsDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "icarus quick commands"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search quick commands…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No quick commands match".into())
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query;
        self.selected_index = 0;
        self.refilter();
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(command) = self.command(self.selected_index).cloned() else {
            return;
        };
        run_in_window(self.target.clone(), command, window, cx);
        self.button.update(cx, |_, cx| cx.notify()).log_err();
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let command = self.command(index)?.clone();
        let copy_text = command.command.clone();
        let edited = command.clone();
        let deleted = command.clone();
        let edit_button = self.button.clone();
        let delete_button = self.button.clone();
        let picker = cx.entity().downgrade();
        let actions = h_flex()
            .gap_0p5()
            .child(
                IconButton::new(("quick-command-copy", index), IconName::Copy)
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text(format!("Copy {}", command.label)))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                    }),
            )
            .child(
                IconButton::new(("quick-command-edit", index), IconName::Pencil)
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text(format!("Edit {}", command.label)))
                    .on_click({
                        let picker = picker.clone();
                        move |_, window, cx| {
                            cx.stop_propagation();
                            picker.update(cx, |_, cx| cx.emit(DismissEvent)).log_err();
                            edit_button
                                .update(cx, |button, cx| {
                                    button.open_editor(Some(edited.clone()), window, cx)
                                })
                                .log_err();
                        }
                    }),
            )
            .child(
                IconButton::new(("quick-command-delete", index), IconName::Trash)
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text(format!("Remove {}", command.label)))
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        picker.update(cx, |_, cx| cx.emit(DismissEvent)).log_err();
                        delete_button
                            .update(cx, |button, cx| button.delete(deleted.clone(), window, cx))
                            .log_err();
                    }),
            );
        let item = ListItem::new(("quick-command", index))
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected)
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(
                        Icon::new(IconName::PlayFilled)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(Label::new(command.label.clone()).truncate())
                            .child(
                                Label::new(
                                    command
                                        .command
                                        .lines()
                                        .next()
                                        .unwrap_or_default()
                                        .to_owned(),
                                )
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .buffer_font(cx)
                                .truncate(),
                            ),
                    ),
            );
        Some(if selected {
            item.end_slot(actions)
        } else {
            item.end_slot(actions).show_end_slot_on_hover()
        })
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<gpui::AnyElement> {
        let button = self.button.clone();
        let picker = cx.entity().downgrade();
        Some(
            h_flex()
                .w_full()
                .p_1()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Button::new("icarus-quick-command-new", "Command")
                        .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                        .label_size(LabelSize::Small)
                        .color(Color::Muted)
                        .on_click(move |_, window, cx| {
                            picker.update(cx, |_, cx| cx.emit(DismissEvent)).log_err();
                            button
                                .update(cx, |button, cx| button.open_editor(None, window, cx))
                                .log_err();
                        }),
                )
                .into_any_element(),
        )
    }
}

/// Adds or edits one quick command; it saves to the user's settings through the server.
pub struct QuickCommandEditor {
    button: WeakEntity<QuickCommandsButton>,
    target: Arc<Target>,
    existing: Option<QuickCommand>,
    name: Entity<Editor>,
    command: Entity<Editor>,
    project_only: bool,
    saving: bool,
    error: Option<String>,
}

impl QuickCommandEditor {
    fn new(
        button: WeakEntity<QuickCommandsButton>,
        target: Arc<Target>,
        existing: Option<QuickCommand>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Name, e.g. Dev server", window, cx);
            if let Some(existing) = &existing {
                editor.set_text(existing.label.clone(), window, cx);
            }
            editor
        });
        let command = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 6, window, cx);
            editor.set_placeholder_text("Command, e.g. npm run dev", window, cx);
            if let Some(existing) = &existing {
                editor.set_text(existing.command.clone(), window, cx);
            }
            editor
        });
        window.focus(&name.focus_handle(cx), cx);
        let project_only = existing
            .as_ref()
            .map_or(target.remote.is_some(), QuickCommand::project_only);
        Self {
            button,
            target,
            existing,
            name,
            command,
            project_only,
            saving: false,
            error: None,
        }
    }

    /// The command as `quick_command.upsert` takes it.
    pub fn draft(&self, cx: &App) -> Value {
        let scope = match (&self.target.remote, self.project_only) {
            (Some(remote), true) => json!({ "type": "project", "remote": remote }),
            _ => json!({ "type": "global" }),
        };
        let mut draft = json!({
            "label": self.name.read(cx).text(cx).trim(),
            "command": self.command.read(cx).text(cx),
            "scope": scope,
            "appendEnter": self.existing.as_ref().is_none_or(|existing| existing.append_enter),
        });
        if let Some(existing) = &self.existing {
            draft["id"] = json!(existing.id);
        }
        draft
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.draft(cx);
        if self.saving {
            return;
        }
        if draft["label"].as_str().is_none_or(str::is_empty)
            || draft["command"]
                .as_str()
                .is_none_or(|c| c.trim().is_empty())
        {
            self.error = Some("A quick command needs a name and a command.".into());
            cx.notify();
            return;
        }
        let Some(client) = self.target.client.clone() else {
            return;
        };
        self.saving = true;
        self.error = None;
        cx.notify();
        let save = Tokio::spawn_result(cx, async move {
            client
                .request("quick_command.upsert", json!({ "command": draft }))
                .await
        });
        let button = self.button.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = save.await;
            this.update(cx, |this, cx| match result {
                Ok(saved) => {
                    if let Some(id) = saved["id"].as_str() {
                        remember(&this.target.workspace_id, id, cx);
                    }
                    button.update(cx, |button, cx| button.reload(cx)).log_err();
                    cx.emit(DismissEvent);
                }
                Err(error) => {
                    this.saving = false;
                    this.error = Some(format!("{error:#}"));
                    cx.notify();
                }
            })
        })
        .detach_and_log_err(cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl ModalView for QuickCommandEditor {}

impl EventEmitter<DismissEvent> for QuickCommandEditor {}

impl Focusable for QuickCommandEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for QuickCommandEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if self.existing.is_some() {
            "Edit quick command"
        } else {
            "Add quick command"
        };
        modal_frame("IcarusQuickCommandEditor", title, cx)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .child(render_field("Name", &self.name, cx))
            .child(render_field("Command", &self.command, cx))
            .when(self.target.remote.is_some(), |modal| {
                modal.child(
                    Checkbox::new(
                        "icarus-quick-command-project-only",
                        if self.project_only {
                            ToggleState::Selected
                        } else {
                            ToggleState::Unselected
                        },
                    )
                    .label("Only in this project")
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(
                        |this, state: &ToggleState, _, cx| {
                            this.project_only = *state == ToggleState::Selected;
                            cx.notify();
                        },
                    )),
                )
            })
            .when_some(self.error.as_deref(), |modal, error| {
                modal.child(render_error(error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("icarus-quick-command-cancel", "Cancel").on_click(cx.listener(
                            |this, _, window, cx| this.cancel(&menu::Cancel, window, cx),
                        )),
                    )
                    .child(
                        Button::new("icarus-quick-command-save", "Save")
                            .style(ButtonStyle::Filled)
                            .disabled(self.saving)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx)
                            })),
                    ),
            )
    }
}

pub(crate) fn register(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &RunQuickCommand, window, cx| {
            with_target(
                workspace,
                window,
                cx,
                |target, workspace, window, cx| match target.recent(cx).cloned() {
                    Some(command) => run(target, command, workspace, window, cx),
                    None => log::info!("no quick commands for this workspace"),
                },
            );
        });
        workspace.register_action(|workspace, _: &OpenQuickCommandsMenu, window, cx| {
            let button = cx
                .try_global::<QuickCommandsButtons>()
                .and_then(|buttons| buttons.0.get(&workspace.project().entity_id()))
                .and_then(WeakEntity::upgrade)
                .filter(|button| !button.read(cx).commands().is_empty());
            if let Some(button) = button {
                let menu = button.read(cx).menu_handle();
                menu.toggle(window, cx);
                return;
            }
            with_target(workspace, window, cx, |target, workspace, window, cx| {
                let items = target
                    .commands
                    .iter()
                    .map(|command| PaletteItem {
                        label: command.label.clone(),
                        detail: Some(command.command.clone()),
                        keywords: command.command.clone(),
                    })
                    .collect();
                let workspace_handle = cx.entity().downgrade();
                workspace.toggle_modal(window, cx, move |window, cx| {
                    Palette::new(
                        "Run a quick command…",
                        items,
                        move |index, window, cx| {
                            let Some(command) = target.commands.get(index).cloned() else {
                                return;
                            };
                            let target = target.clone();
                            workspace_handle
                                .update(cx, |workspace, cx| {
                                    run(target, command, workspace, window, cx)
                                })
                                .log_err();
                        },
                        window,
                        cx,
                    )
                });
            });
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick(label: &str) -> QuickCommand {
        QuickCommand {
            id: format!("id-{label}"),
            label: label.into(),
            command: format!("{label} cmd"),
            append_enter: true,
            scope: Value::Null,
        }
    }

    fn terminal(id: &str, label: Option<&str>, busy: bool) -> ServerTerminal {
        ServerTerminal {
            id: id.into(),
            label: label.map(str::to_string),
            workspace: Some("ws".into()),
            running: true,
            lost: false,
            busy,
        }
    }

    fn shown(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn a_running_command_is_stopped_where_it_runs() {
        let terminals = [terminal("a", Some("Run"), true), terminal("b", None, false)];
        assert_eq!(
            decide(&quick("Run"), "ws", &terminals, &shown(&["a", "b"])),
            Decision::Interrupt("a".into())
        );
    }

    #[test]
    fn an_idle_terminal_it_ran_in_wins_then_any_idle_one_then_a_new_tab() {
        let terminals = [
            terminal("busy", None, true),
            terminal("other", None, false),
            terminal("own", Some("Run"), false),
        ];
        assert_eq!(
            decide(
                &quick("Run"),
                "ws",
                &terminals,
                &shown(&["busy", "other", "own"])
            ),
            Decision::Send("own".into())
        );
        assert_eq!(
            decide(
                &quick("Test"),
                "ws",
                &terminals,
                &shown(&["busy", "other", "own"])
            ),
            Decision::Send("other".into())
        );
        assert_eq!(
            decide(&quick("Run"), "ws", &terminals, &shown(&["busy"])),
            Decision::NewTerminal,
            "terminals not shown in this window aren't used"
        );
        let elsewhere = ServerTerminal {
            workspace: Some("other-ws".into()),
            ..terminal("x", None, false)
        };
        assert_eq!(
            decide(&quick("Run"), "ws", &[elsewhere], &shown(&["x"])),
            Decision::NewTerminal
        );
    }

    #[test]
    fn the_server_reply_parses_and_multi_line_commands_run_as_one_list() {
        let reply: ListReply = serde_json::from_value(json!({
            "remote": "github.com/acme/app",
            "commands": [
                { "id": "p", "label": "Project", "scope": { "type": "project", "remote": "github.com/acme/app" }, "action": "terminal-command", "command": "echo p", "appendEnter": false },
                { "id": "g", "label": "Global", "scope": { "type": "global" }, "action": "terminal-command", "command": "cd app\n  make\n" },
            ]
        }))
        .expect("reply");
        assert_eq!(reply.remote.as_deref(), Some("github.com/acme/app"));
        assert_eq!(reply.commands[0].text(), "echo p");
        assert!(reply.commands[0].project_only());
        assert_eq!(reply.commands[1].text(), "cd app; make\r");
        assert!(!reply.commands[1].project_only());
    }

    #[test]
    fn search_ranks_names_before_command_lines_like_orca() {
        let mut preview = quick("Preview");
        preview.command = "mise run frontend-build".into();
        let mut run = quick("Run");
        run.command = "mise run fu".into();
        let mut backend = quick("Local Backend");
        backend.command = "uvx --from ./run".into();
        let commands = [preview, run, backend];
        let labels = |indices: Vec<usize>| -> Vec<&str> {
            indices
                .into_iter()
                .map(|i| commands[i].label.as_str())
                .collect()
        };
        assert_eq!(
            labels(search(&commands, "")),
            ["Preview", "Run", "Local Backend"]
        );
        assert_eq!(
            labels(search(&commands, "run")),
            ["Run", "Preview", "Local Backend"]
        );
        assert_eq!(labels(search(&commands, "back")), ["Local Backend"]);
        assert_eq!(labels(search(&commands, "  FRONTEND ")), ["Preview"]);
        assert!(search(&commands, "nothing").is_empty());
    }

    #[gpui::test]
    async fn the_menu_filters_and_new_commands_default_to_the_project(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, _, mut cx) = crate::tests::panel_window(Vec::new(), cx).await;
        let mut backend = quick("Local Backend");
        backend.command = "uvx --from ./backend".into();
        let target = Target::preview(
            "ws",
            Some("github.com/acme/app"),
            vec![quick("Run"), backend],
        );
        let button = cx.new(|_| QuickCommandsButton::preview(target));
        let picker = button
            .update_in(&mut cx, |button, window, cx| button.picker(window, cx))
            .expect("a menu for a workspace with commands");
        picker.update_in(&mut cx, |picker, window, cx| {
            picker.update_matches("back".into(), window, cx)
        });
        cx.run_until_parked();
        picker.read_with(&cx, |picker, _| {
            assert_eq!(picker.delegate.visible_labels(), ["Local Backend"])
        });

        button.update_in(&mut cx, |button, window, cx| {
            button.open_editor(None, window, cx)
        });
        let editor = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.active_modal::<QuickCommandEditor>(cx)
            })
            .expect("the add dialog opens");
        editor.update_in(&mut cx, |editor, window, cx| {
            editor
                .name
                .update(cx, |name, cx| name.set_text("Preview", window, cx));
            editor.command.update(cx, |command, cx| {
                command.set_text("mise run preview", window, cx)
            });
        });
        let draft = editor.read_with(&cx, |editor, cx| editor.draft(cx));
        assert_eq!(draft["label"], "Preview");
        assert_eq!(draft["command"], "mise run preview");
        assert_eq!(
            draft["scope"],
            json!({ "type": "project", "remote": "github.com/acme/app" })
        );
        assert!(draft.get("id").is_none(), "the server names a new command");
    }

    #[test]
    fn the_run_button_repeats_the_last_command_else_the_first() {
        let commands = [quick("Run"), quick("Setup")];
        assert_eq!(
            most_recent(&commands, None).map(|c| c.label.as_str()),
            Some("Run")
        );
        assert_eq!(
            most_recent(&commands, Some("id-Setup")).map(|c| c.label.as_str()),
            Some("Setup")
        );
        assert_eq!(
            most_recent(&commands, Some("deleted")).map(|c| c.label.as_str()),
            Some("Run")
        );
        assert!(most_recent(&[], None).is_none());
    }
}
