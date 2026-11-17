//! Quick commands (features.md §2.15): saved terminal commands in the user setting `quickCommands`,
//! in Orca's shape. A command is global or scoped to a project by its canonical remote, which is
//! how Orca's settings export carries them, so imported commands keep working here.

use std::sync::Arc;

use icarus_protocol::RpcError;
use icarus_settings::layers::{self, Layer, SettingsPaths};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::{Server, params, rpc};
use crate::settings::to_json;

const KEY: &str = "quickCommands";
const MAX_LABEL: usize = 80;
const MAX_COMMAND: usize = 4000;

fn is_terminal_command(entry: &Value) -> bool {
    entry["action"].as_str().unwrap_or("terminal-command") == "terminal-command"
        && entry["command"]
            .as_str()
            .is_some_and(|command| !command.trim().is_empty())
}

fn scope_remote(entry: &Value) -> Option<&str> {
    match entry["scope"]["type"].as_str() {
        None | Some("global") => None,
        Some(_) => Some(entry["scope"]["remote"].as_str().unwrap_or_default()),
    }
}

/// The commands a workspace of the project with `remote` can run: its project's first, then global ones.
pub fn commands_for(commands: &[Value], remote: Option<&str>) -> Vec<Value> {
    let (mut project, mut global) = (Vec::new(), Vec::new());
    for entry in commands.iter().filter(|entry| is_terminal_command(entry)) {
        match scope_remote(entry) {
            None => global.push(entry.clone()),
            Some(scoped) => {
                if remote
                    .is_some_and(|remote| !scoped.is_empty() && scoped.eq_ignore_ascii_case(remote))
                {
                    project.push(entry.clone());
                }
            }
        }
    }
    project.extend(global);
    project
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Orca's normalization of one edited command; a missing id gets a fresh one.
pub fn normalize(command: &Value) -> Result<Value, String> {
    let label = clip(
        command["label"].as_str().unwrap_or_default().trim(),
        MAX_LABEL,
    );
    let text = clip(
        command["command"].as_str().unwrap_or_default().trim_end(),
        MAX_COMMAND,
    );
    if label.is_empty() || text.trim().is_empty() {
        return Err("a quick command needs a name and a command".into());
    }
    let id = command["id"]
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("quick-command-{}", uuid::Uuid::new_v4()));
    let scope = match scope_remote(command) {
        Some(remote) if !remote.trim().is_empty() => {
            json!({ "type": "project", "remote": remote.trim() })
        }
        _ => json!({ "type": "global" }),
    };
    Ok(json!({
        "id": id,
        "label": label,
        "scope": scope,
        "action": "terminal-command",
        "command": text,
        "appendEnter": command["appendEnter"].as_bool().unwrap_or(true),
    }))
}

/// Replaces the command with the same id in place, or appends it.
pub fn upsert(commands: &mut Vec<Value>, command: Value) {
    match commands
        .iter_mut()
        .find(|entry| entry["id"] == command["id"])
    {
        Some(entry) => *entry = command,
        None => commands.push(command),
    }
}

impl Server {
    fn user_quick_commands(&self) -> Result<Vec<Value>, RpcError> {
        let Some(path) = SettingsPaths::discover(None).user else {
            return Ok(Vec::new());
        };
        let doc = layers::read_layer(Layer::User, &path)
            .map_err(|e| rpc("settings_invalid", format!("{e:?}")))?;
        Ok(doc
            .as_ref()
            .and_then(|doc| doc.get(KEY))
            .map(to_json)
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default())
    }

    fn write_quick_commands(&self, commands: Vec<Value>) -> Result<(), RpcError> {
        self.write_key(KEY, Some(Value::Array(commands)), None, None)
            .map(drop)
    }

    fn workspace_remote(&self, workspace: &str) -> Result<Option<String>, RpcError> {
        let workspace = self
            .store
            .workspace(workspace)
            .map_err(|e| rpc("internal", e))?
            .ok_or_else(|| rpc("not_found", format!("workspace {workspace}")))?;
        Ok(self
            .store
            .project(&workspace.project_id)
            .map_err(|e| rpc("internal", e))?
            .and_then(|project| project.remote))
    }

    pub(crate) async fn quick_commands_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        match method {
            "quick_command.list" => {
                #[derive(Deserialize)]
                struct P {
                    workspace: Option<String>,
                }
                let p: P = params(p)?;
                let commands = self.user_quick_commands()?;
                let remote = match p.workspace.as_deref() {
                    Some(workspace) => self.workspace_remote(workspace)?,
                    None => None,
                };
                let listed = match &p.workspace {
                    Some(_) => commands_for(&commands, remote.as_deref()),
                    None => commands,
                };
                Ok(json!({ "commands": listed, "remote": remote }))
            }
            "quick_command.upsert" => {
                #[derive(Deserialize)]
                struct P {
                    command: Value,
                }
                let p: P = params(p)?;
                let command = normalize(&p.command).map_err(|e| rpc("invalid_params", e))?;
                let mut commands = self.user_quick_commands()?;
                upsert(&mut commands, command.clone());
                self.write_quick_commands(commands)?;
                Ok(command)
            }
            "quick_command.delete" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                }
                let p: P = params(p)?;
                let mut commands = self.user_quick_commands()?;
                let before = commands.len();
                commands.retain(|entry| entry["id"].as_str() != Some(p.id.as_str()));
                let deleted = commands.len() != before;
                if deleted {
                    self.write_quick_commands(commands)?;
                }
                Ok(json!({ "deleted": deleted }))
            }
            other => Err(rpc("unknown_method", other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, scope: Value, command: &str) -> Value {
        json!({ "id": id, "label": id, "scope": scope, "action": "terminal-command", "command": command, "appendEnter": true })
    }

    #[test]
    fn a_workspace_lists_its_projects_commands_first_then_global_ones() {
        let commands = [
            entry("g", json!({ "type": "global" }), "echo g"),
            entry(
                "p",
                json!({ "type": "project", "remote": "github.com/acme/app" }),
                "echo p",
            ),
            entry(
                "o",
                json!({ "type": "project", "remote": "github.com/acme/other" }),
                "echo o",
            ),
            entry("empty", json!({ "type": "global" }), "  "),
            json!({ "id": "agent", "label": "Agent", "action": "agent-prompt", "agent": "claude", "prompt": "hi" }),
        ];
        let ids = |listed: Vec<Value>| -> Vec<String> {
            listed
                .iter()
                .map(|c| c["id"].as_str().unwrap_or_default().to_owned())
                .collect()
        };
        assert_eq!(
            ids(commands_for(&commands, Some("github.com/Acme/App"))),
            ["p", "g"]
        );
        assert_eq!(ids(commands_for(&commands, None)), ["g"]);
    }

    #[test]
    fn edits_are_normalized_like_orca_and_replace_by_id() {
        let added = normalize(&json!({ "label": "  Run ", "command": "mise run fu\n", "scope": { "type": "project", "remote": "github.com/acme/app" } }))
            .expect("valid");
        assert!(
            added["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("quick-command-"))
        );
        assert_eq!(added["label"], "Run");
        assert_eq!(added["command"], "mise run fu");
        assert_eq!(added["appendEnter"], true);
        assert_eq!(added["scope"]["remote"], "github.com/acme/app");
        assert!(normalize(&json!({ "label": "x", "command": " " })).is_err());
        assert_eq!(
            normalize(&json!({ "id": "a", "label": "x", "command": "y" })).expect("valid")["scope"],
            json!({ "type": "global" })
        );

        let mut commands = vec![
            entry("a", json!({ "type": "global" }), "one"),
            entry("b", json!({ "type": "global" }), "two"),
        ];
        upsert(
            &mut commands,
            entry("a", json!({ "type": "global" }), "changed"),
        );
        upsert(
            &mut commands,
            entry("c", json!({ "type": "global" }), "three"),
        );
        let texts: Vec<&str> = commands
            .iter()
            .filter_map(|c| c["command"].as_str())
            .collect();
        assert_eq!(texts, ["changed", "two", "three"]);
    }
}
