//! Quick commands (features.md §2.15): saved terminal commands from the `quickCommands` setting,
//! run the way Orca does: a command already running in its terminal is stopped (Ctrl-C), otherwise
//! it's typed into an idle terminal of the workspace, otherwise a new terminal tab runs it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use collections::HashSet;
use gpui::{App, Context, TaskExt as _, Window};
use gpui_tokio::Tokio;
use icarus_client::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use util::ResultExt as _;
use workspace::Workspace;

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

#[derive(Debug, Clone, PartialEq)]
pub struct QuickCommand {
    pub id: String,
    pub label: String,
    pub command: String,
    pub append_enter: bool,
}

impl QuickCommand {
    fn text(&self) -> String {
        if self.append_enter {
            format!("{}\r", self.command)
        } else {
            self.command.clone()
        }
    }
}

/// The commands for a project: its own first (scope remote matches), then global ones.
pub fn commands_for(setting: &Value, project_remote: Option<&str>) -> Vec<QuickCommand> {
    let mut project = Vec::new();
    let mut global = Vec::new();
    for entry in setting.as_array().into_iter().flatten() {
        let action = entry["action"].as_str().unwrap_or("terminal-command");
        let command = entry["command"].as_str().unwrap_or_default();
        if action != "terminal-command" || command.trim().is_empty() {
            continue;
        }
        let parsed = QuickCommand {
            id: entry["id"].as_str().unwrap_or(command).to_string(),
            label: entry["label"]
                .as_str()
                .unwrap_or(command)
                .trim()
                .to_string(),
            command: command.to_string(),
            append_enter: entry["appendEnter"].as_bool().unwrap_or(true),
        };
        match entry["scope"]["type"].as_str() {
            None | Some("global") => global.push(parsed),
            Some(_) => {
                let remote = entry["scope"]["remote"].as_str();
                if remote.is_some()
                    && project_remote.is_some_and(|project| {
                        remote.is_some_and(|r| r.eq_ignore_ascii_case(project))
                    })
                {
                    project.push(parsed);
                }
            }
        }
    }
    project.extend(global);
    project
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

/// The quick command each workspace ran last, so ⌘R repeats it.
#[derive(Default)]
struct RecentCommands(HashMap<String, String>);

impl gpui::Global for RecentCommands {}

struct Target {
    client: Arc<Client>,
    workspace_id: String,
    path: PathBuf,
    commands: Vec<QuickCommand>,
}

/// The Icarus workspace behind this window and its quick commands.
async fn load_target(client: Arc<Client>, roots: Vec<PathBuf>) -> Result<Target> {
    let workspaces = client.request("workspace.list", json!({})).await?;
    let (workspace_id, project_id, path) = workspaces
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|w| {
            let path = PathBuf::from(w["path"].as_str()?);
            roots
                .iter()
                .any(|root| root.starts_with(&path))
                .then(|| {
                    Some((
                        w["id"].as_str()?.to_string(),
                        w["project_id"].as_str()?.to_string(),
                        path,
                    ))
                })
                .flatten()
        })
        .context("this window isn't an Icarus workspace")?;
    let projects = client.request("project.list", json!({})).await?;
    let remote = projects
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["id"] == project_id.as_str())
        .and_then(|p| p["remote"].as_str().map(str::to_string));
    let setting = client
        .request(
            "config.get",
            json!({ "key": "quickCommands", "project": project_id }),
        )
        .await?;
    Ok(Target {
        client,
        workspace_id,
        path,
        commands: commands_for(&setting, remote.as_deref()),
    })
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
    cx.default_global::<RecentCommands>()
        .0
        .insert(target.workspace_id.clone(), command.id.clone());
    // `workspace` is the entity being updated here, so it's read through the borrow, not the handle.
    let shown = shown_terminals(workspace, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let client = target.client.clone();
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
        let target = Arc::new(Tokio::spawn_result(cx, load_target(client, roots)).await?);
        workspace.update_in(cx, |workspace, window, cx| {
            then(target, workspace, window, cx)
        })
    })
    .detach_and_log_err(cx);
}

pub(crate) fn register(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &RunQuickCommand, window, cx| {
            with_target(workspace, window, cx, |target, workspace, window, cx| {
                let recent = cx
                    .try_global::<RecentCommands>()
                    .and_then(|recent| recent.0.get(&target.workspace_id).cloned());
                let command = recent
                    .and_then(|id| target.commands.iter().find(|c| c.id == id).cloned())
                    .or_else(|| target.commands.first().cloned());
                match command {
                    Some(command) => run(target, command, workspace, window, cx),
                    None => log::info!("no quick commands for this workspace"),
                }
            });
        });
        workspace.register_action(|workspace, _: &OpenQuickCommandsMenu, window, cx| {
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
    fn project_commands_come_first_and_other_projects_are_left_out() {
        let setting = serde_json::json!([
            { "id": "g", "label": "Global", "scope": { "type": "global" }, "action": "terminal-command", "command": "echo g" },
            { "id": "p", "label": "Project", "scope": { "type": "project", "remote": "github.com/acme/app" }, "action": "terminal-command", "command": "echo p", "appendEnter": false },
            { "id": "o", "label": "Other", "scope": { "type": "project", "remote": "github.com/acme/other" }, "action": "terminal-command", "command": "echo o" },
            { "id": "a", "label": "Agent", "action": "agent-prompt", "command": "do it" },
        ]);
        let commands = commands_for(&setting, Some("github.com/Acme/App"));
        let ids: Vec<&str> = commands.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["p", "g"]);
        assert_eq!(commands[0].text(), "echo p");
        assert_eq!(commands[1].text(), "echo g\r");
    }
}
