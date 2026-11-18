use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Subcommand;
use icarus_client::Client;
use serde_json::{Value, json};

pub(crate) fn orca_flow() -> String {
    let mut config = icarus_settings::automation_template::orca_flow_defaults();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    // Only workspaces made from now on get the new-workspace column, not every existing one.
    config["newWorkspaceStatusSince"] = json!(now);
    icarus_settings::automation_template::orca_flow(&config)
}

#[derive(Subcommand)]
pub enum AutomationCommand {
    /// Whether automations are on and armed, each project's script, and its last run.
    Status,
    /// Run each project's script now (or one project's), against a fresh scope.
    Run {
        #[arg(long)]
        project: Option<String>,
        /// Record what the script decides without applying it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Send a custom event to a project's script, e.g. from the Linear "Open in coding tool" hook.
    Emit {
        /// Event name the script sees as `trigger.name`.
        name: String,
        #[arg(long)]
        project: Option<String>,
        /// The project owning this directory (a linked worktree counts).
        #[arg(long, conflicts_with = "project")]
        path: Option<PathBuf>,
        /// The workspace the event is about.
        #[arg(long)]
        workspace: Option<String>,
        /// JSON payload, as `trigger.payload`.
        #[arg(long)]
        payload: Option<String>,
        /// Environment variables added to the payload by name.
        #[arg(long = "env", value_name = "VAR", num_args = 1..)]
        env: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Recent runs, newest first.
    Runs {
        #[arg(long)]
        project: Option<String>,
        /// Only runs whose script or an action failed.
        #[arg(long)]
        failed: bool,
        /// Only runs that applied an action.
        #[arg(long)]
        acted: bool,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// One run: trigger, output, actions and their results; --scope adds what the script saw.
    Show {
        run: String,
        #[arg(long)]
        scope: bool,
    },
    /// Re-run a script against a recorded run's scope, as a dry run.
    Replay {
        run: String,
        /// Another script, e.g. a fix you are trying.
        #[arg(long)]
        script: Option<PathBuf>,
    },
    /// Regenerate icarus.d.ts and schema.json with the current status and prompt ids.
    Schema,
    /// Print a starter script: the built-in GitHub PR flow, with its options at the top.
    Template,
}

#[derive(Subcommand)]
pub enum PromptCommand {
    /// The prompt templates from settings.
    List {
        #[arg(long)]
        project: Option<String>,
    },
    /// Render a prompt against a workspace, to check its `{{path}}`s.
    Render {
        id: String,
        #[arg(long)]
        workspace: String,
    },
}

#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum TabCommand {
    /// Add a tab to a workspace's window; it waits for the window if none is open.
    Open {
        workspace: String,
        /// agent, browser, diff, file, terminal or explorer.
        #[arg(long = "type")]
        kind: String,
        #[arg(long)]
        url: Option<String>,
        /// File to open, or the diff's file.
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        line: Option<u32>,
        /// The diff's scope: branch, uncommitted or a commit.
        #[arg(long)]
        scope: Option<String>,
        /// The agent's first message.
        #[arg(long)]
        prompt: Option<String>,
        /// A prompt template from settings, rendered for the workspace.
        #[arg(long, conflicts_with = "prompt")]
        prompt_id: Option<String>,
        #[arg(long)]
        provider: Option<String>,
        /// The agent's permission mode, e.g. plan.
        #[arg(long)]
        mode: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// The terminal's command.
        #[arg(long)]
        command: Option<String>,
        /// Open it in a new pane split off the active one: right or down.
        #[arg(long, conflicts_with = "pane")]
        split: Option<String>,
        /// The pane to open it in: an id like p12, active, or a path like left, right or 0.1.
        #[arg(long)]
        pane: Option<String>,
        /// Its position in the pane; the end by default.
        #[arg(long)]
        index: Option<usize>,
        /// Keep the current tab in front.
        #[arg(long, conflicts_with = "focus")]
        background: bool,
        /// Move keyboard focus to the new tab and bring its window forward.
        #[arg(long)]
        focus: bool,
    },
    /// Each pane's tabs in the workspace's window, with ids for the other commands.
    List {
        workspace: String,
    },
    /// Bring a tab to the front of its pane and focus it.
    Focus {
        workspace: String,
        tab: String,
    },
    /// Move a tab to another pane, or to a position in its own.
    Move {
        workspace: String,
        tab: String,
        #[arg(long)]
        pane: String,
        #[arg(long)]
        index: Option<usize>,
    },
    Close {
        workspace: String,
        tab: String,
    },
}

#[derive(Subcommand)]
pub enum PaneCommand {
    /// Split a pane (the active one by default) and add an empty pane beside it.
    Split {
        workspace: String,
        /// right or down.
        #[arg(long)]
        direction: String,
        #[arg(long)]
        pane: Option<String>,
    },
    /// The workspace window's split tree and panes, with ids and paths.
    List { workspace: String },
    /// Close a pane and its tabs.
    Close { workspace: String, pane: String },
}

fn queued(v: &Value, done: &str) -> String {
    if v["delivered"].as_u64().unwrap_or(0) > 0 {
        done.into()
    } else {
        "queued: applies when the workspace's window opens".into()
    }
}

pub async fn run_pane(cli: &super::Cli, client: &Client, command: &PaneCommand) -> Result<()> {
    match command {
        PaneCommand::Split {
            workspace,
            direction,
            pane,
        } => {
            let value = client
                .request(
                    "pane.split",
                    json!({ "workspace": workspace, "direction": direction, "pane": pane }),
                )
                .await?;
            super::print(cli, &value, |v| queued(v, "split"));
        }
        PaneCommand::List { workspace } => {
            let value = client
                .request("pane.list", json!({ "workspace": workspace }))
                .await?;
            super::print(cli, &value, |v| {
                v["panes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|p| {
                        format!(
                            "{}{}\t{}\t{} tabs\t{}",
                            p["id"].as_str().unwrap_or_default(),
                            if p["active"] == true { "*" } else { "" },
                            p["path"].as_str().unwrap_or_default(),
                            p["tabs"],
                            p["active_tab"]["title"].as_str().unwrap_or_default(),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        PaneCommand::Close { workspace, pane } => {
            let value = client
                .request(
                    "pane.close",
                    json!({ "workspace": workspace, "pane": pane }),
                )
                .await?;
            super::print(cli, &value, |v| queued(v, "closed"));
        }
    }
    Ok(())
}

pub async fn run_tab(cli: &super::Cli, client: &Client, command: &TabCommand) -> Result<()> {
    let (method, params, done) = match command {
        TabCommand::Open { .. } => return open_tab(cli, client, command).await,
        TabCommand::List { workspace } => {
            let value = client
                .request("tab.list", json!({ "workspace": workspace }))
                .await?;
            super::print(cli, &value, |v| {
                let mut lines = Vec::new();
                for pane in v["panes"].as_array().into_iter().flatten() {
                    lines.push(format!(
                        "{}{} {}",
                        pane["id"].as_str().unwrap_or_default(),
                        if pane["active"] == true { "*" } else { "" },
                        pane["path"].as_str().unwrap_or_default(),
                    ));
                    for tab in pane["tabs"].as_array().into_iter().flatten() {
                        lines.push(format!(
                            "  {}{}\t{}\t{}",
                            tab["id"].as_str().unwrap_or_default(),
                            if tab["active"] == true { "*" } else { "" },
                            tab["type"].as_str().unwrap_or_default(),
                            tab["title"].as_str().unwrap_or_default(),
                        ));
                    }
                }
                lines.join("\n")
            });
            return Ok(());
        }
        TabCommand::Focus { workspace, tab } => (
            "tab.focus",
            json!({ "workspace": workspace, "tab": tab }),
            "focused",
        ),
        TabCommand::Move {
            workspace,
            tab,
            pane,
            index,
        } => (
            "tab.move",
            json!({ "workspace": workspace, "tab": tab, "pane": pane, "index": index }),
            "moved",
        ),
        TabCommand::Close { workspace, tab } => (
            "tab.close",
            json!({ "workspace": workspace, "tab": tab }),
            "closed",
        ),
    };
    let value = client.request(method, params).await?;
    super::print(cli, &value, |v| queued(v, done));
    Ok(())
}

async fn open_tab(cli: &super::Cli, client: &Client, command: &TabCommand) -> Result<()> {
    let TabCommand::Open {
        workspace,
        kind,
        url,
        path,
        line,
        scope,
        prompt,
        prompt_id,
        provider,
        mode,
        title,
        command,
        split,
        pane,
        index,
        background,
        focus,
    } = command
    else {
        return Ok(());
    };
    let prompt = match prompt_id {
        Some(id) => client
            .request("prompt.render", json!({ "id": id, "workspace": workspace }))
            .await?["prompt"]
            .as_str()
            .map(str::to_owned),
        None => prompt.clone(),
    };
    let value = client
        .request(
            "tab.open",
            json!({
                "workspace": workspace,
                "type": kind,
                "url": url,
                "path": path,
                "line": line,
                "scope": scope,
                "prompt": prompt,
                "provider": provider,
                "mode": mode,
                "title": title,
                "command": command,
                "split": split,
                "pane": pane,
                "index": index,
                "background": background,
                "focus": focus,
            }),
        )
        .await?;
    super::print(cli, &value, |v| queued(v, "opened"));
    Ok(())
}

fn payload(raw: Option<&str>, env: &[String]) -> Result<Value> {
    let mut value = match raw {
        Some(text) => serde_json::from_str(text).context("--payload is not JSON")?,
        None => json!({}),
    };
    if !env.is_empty() {
        if !value.is_object() {
            value = json!({ "value": value });
        }
        for name in env {
            value[name] = json!(std::env::var(name).ok());
        }
    }
    Ok(value)
}

fn when(ms: &Value) -> String {
    let Some(ms) = ms.as_u64() else {
        return "-".into();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    let secs = now.saturating_sub(ms) / 1000;
    match secs {
        0..60 => format!("{secs}s ago"),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

fn trigger(t: &Value) -> String {
    match t["kind"].as_str() {
        Some("event") => format!("event {}", t["name"].as_str().unwrap_or("?")),
        Some(kind) => kind.to_owned(),
        None => "?".into(),
    }
}

fn run_text(run: &Value) -> String {
    let mut lines = vec![format!(
        "{}  {}{}{}  {}ms",
        run["id"].as_str().unwrap_or("?"),
        trigger(&run["trigger"]),
        if run["dry_run"] == true {
            "  (dry run)"
        } else {
            ""
        },
        if run["failed"] == true {
            "  FAILED"
        } else {
            ""
        },
        run["duration_ms"],
    )];
    if let Some(error) = run["error"].as_str() {
        lines.push(format!("  error: {error}"));
    }
    for result in run["results"].as_array().into_iter().flatten() {
        let action = &result["action"];
        let mark = match result["ok"].as_bool() {
            Some(true) => "ok  ",
            Some(false) => "FAIL",
            None => "plan",
        };
        lines.push(format!(
            "  {mark} {} {}",
            action["method"].as_str().unwrap_or("?"),
            action["params"]
        ));
        if let Some(error) = result["error"].as_str() {
            lines.push(format!("       {error}"));
        }
    }
    for call in run["cli_calls"].as_array().into_iter().flatten() {
        let mark = match call["ok"].as_bool() {
            Some(true) => "ok  ",
            Some(false) => "FAIL",
            None => "held",
        };
        lines.push(format!(
            "  {mark} (icarus) {} {}",
            call["method"].as_str().unwrap_or("?"),
            call["params"]
        ));
        if let Some(error) = call["error"].as_str() {
            lines.push(format!("       {error}"));
        }
    }
    if let Some(stderr) = run["stderr"].as_str().filter(|s| !s.trim().is_empty()) {
        lines.push("  stderr:".into());
        lines.extend(stderr.lines().map(|l| format!("    {l}")));
    }
    lines.join("\n")
}

fn status_text(v: &Value) -> String {
    let mut lines = vec![format!(
        "automations {}, {}{}",
        if v["enabled"] == true { "on" } else { "off" },
        if v["armed"] == true {
            "armed"
        } else {
            "dry runs only (automations.armed is false)"
        },
        if v["running"] == true {
            ""
        } else {
            "; paused: no app window is open"
        },
    )];
    for p in v["projects"].as_array().into_iter().flatten() {
        let script = match (p["script"].as_str(), p["script_exists"] == true) {
            (None, _) => "no script".to_owned(),
            (Some(s), true) => s.to_owned(),
            (Some(s), false) => format!("{s} (missing)"),
        };
        let last = &p["last_run"];
        let last = if last.is_null() {
            "never ran".to_owned()
        } else {
            format!(
                "last run {} {}{}",
                last["id"].as_str().unwrap_or("?"),
                when(&last["started_ms"]),
                if last["failed"] == true {
                    " FAILED"
                } else {
                    ""
                }
            )
        };
        lines.push(format!(
            "{}: {script}; {last}",
            p["project"].as_str().unwrap_or("?")
        ));
    }
    lines.join("\n")
}

fn runs_text(v: &Value) -> String {
    let runs = v["runs"].as_array().cloned().unwrap_or_default();
    if runs.is_empty() {
        return "no runs".into();
    }
    runs.iter()
        .map(|r| {
            format!(
                "{}  {:>8}  {:<24} {} action(s){}{}{}",
                r["id"].as_str().unwrap_or("?"),
                when(&r["started_ms"]),
                trigger(&r["trigger"]),
                r["actions"],
                if r["dry_run"] == true { "  dry" } else { "" },
                if r["acted"] == true { "  acted" } else { "" },
                r["error"]
                    .as_str()
                    .map(|e| format!("  FAILED: {e}"))
                    .unwrap_or_else(|| if r["failed"] == true {
                        "  FAILED".into()
                    } else {
                        String::new()
                    }),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn show_text(v: &Value) -> String {
    let record = &v["record"];
    let mut run = json!({
        "id": v["id"],
        "trigger": v["trigger"],
        "dry_run": v["dry_run"],
        "failed": v["failed"],
        "duration_ms": v["duration_ms"],
        "error": record["error"],
        "results": record["results"],
        "cli_calls": record["cli_calls"],
        "stderr": record["stderr"],
    });
    if run["results"].as_array().is_none_or(Vec::is_empty) {
        run["results"] = json!(
            record["actions"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|a| json!({ "action": a }))
                .collect::<Vec<_>>()
        );
    }
    let mut text = format!("{}\n  started {}", run_text(&run), when(&v["started_ms"]));
    if let Some(buttons) = record["buttons"].as_object() {
        for (workspace, list) in buttons {
            let labels: Vec<&str> = list
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|b| b["label"].as_str())
                .collect();
            text.push_str(&format!("\n  buttons {workspace}: {}", labels.join(", ")));
        }
    }
    if !v["scope"].is_null() {
        text.push_str(&format!(
            "\n  scope:\n{}",
            serde_json::to_string_pretty(&v["scope"]).unwrap_or_default()
        ));
    }
    text
}

fn replay_text(v: &Value) -> String {
    let mut lines = vec![format!(
        "replayed {} with {}: {}",
        v["run"].as_str().unwrap_or("?"),
        v["script"].as_str().unwrap_or("?"),
        if v["same_as_recorded"] == true {
            "same actions as recorded"
        } else {
            "DIFFERENT actions from the recorded run"
        }
    )];
    if let Some(error) = v["error"].as_str() {
        lines.push(format!("  error: {error}"));
    }
    for action in v["actions"].as_array().into_iter().flatten() {
        lines.push(format!(
            "  {} {}",
            action["method"].as_str().unwrap_or("?"),
            action["params"]
        ));
    }
    lines.join("\n")
}

pub async fn run(cli: &super::Cli, client: &Client, command: &AutomationCommand) -> Result<()> {
    let long = Duration::from_secs(300);
    match command {
        AutomationCommand::Status => {
            let value = client.request("automation.status", json!({})).await?;
            super::print(cli, &value, status_text);
        }
        AutomationCommand::Run { project, dry_run } => {
            let value = client
                .request_timeout(
                    "automation.run",
                    json!({ "project": project, "dry_run": dry_run }),
                    long,
                )
                .await?;
            super::print(cli, &value, |v| {
                let runs = v["runs"].as_array().cloned().unwrap_or_default();
                if runs.is_empty() {
                    return "no project has a script (set automations.scripts.<project>)".into();
                }
                runs.iter().map(run_text).collect::<Vec<_>>().join("\n")
            });
        }
        AutomationCommand::Emit {
            name,
            project,
            path,
            workspace,
            payload: raw,
            env,
            dry_run,
        } => {
            let path = path
                .as_ref()
                .map(|p| std::path::absolute(p).unwrap_or_else(|_| p.clone()));
            let value = client
                .request_timeout(
                    "automation.emit",
                    json!({
                        "name": name,
                        "project": project,
                        "path": path,
                        "workspace": workspace,
                        "payload": payload(raw.as_deref(), env)?,
                        "dry_run": dry_run,
                    }),
                    long,
                )
                .await?;
            super::print(cli, &value, run_text);
        }
        AutomationCommand::Runs {
            project,
            failed,
            acted,
            limit,
        } => {
            let value = client
                .request(
                    "automation.runs",
                    json!({ "project": project, "failed": failed, "acted": acted, "limit": limit }),
                )
                .await?;
            super::print(cli, &value, runs_text);
        }
        AutomationCommand::Show { run, scope } => {
            let value = client
                .request("automation.show", json!({ "run": run, "scope": scope }))
                .await?;
            super::print(cli, &value, show_text);
        }
        AutomationCommand::Replay { run, script } => {
            let script = script
                .as_ref()
                .map(|p| std::path::absolute(p).unwrap_or_else(|_| p.clone()));
            let value = client
                .request_timeout(
                    "automation.replay",
                    json!({ "run": run, "script": script }),
                    long,
                )
                .await?;
            super::print(cli, &value, replay_text);
        }
        AutomationCommand::Template => unreachable!("printed without a server"),
        AutomationCommand::Schema => {
            let value = client.request("automation.schema", json!({})).await?;
            super::print(cli, &value, |v| {
                format!(
                    "wrote {}\nwrote {}",
                    v["types"].as_str().unwrap_or("?"),
                    v["schema"].as_str().unwrap_or("?")
                )
            });
        }
    }
    Ok(())
}

pub async fn run_prompt(cli: &super::Cli, client: &Client, command: &PromptCommand) -> Result<()> {
    match command {
        PromptCommand::List { project } => {
            let value = client
                .request("prompt.list", json!({ "project": project }))
                .await?;
            super::print(cli, &value, |v| {
                let Some(map) = v.as_object().filter(|m| !m.is_empty()) else {
                    return "no prompts (set prompts.<id> in settings)".into();
                };
                map.iter()
                    .map(|(id, t)| {
                        let first = t
                            .as_str()
                            .unwrap_or_default()
                            .lines()
                            .next()
                            .unwrap_or_default();
                        format!("{id}: {first}")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        PromptCommand::Render { id, workspace } => {
            let value = client
                .request_timeout(
                    "prompt.render",
                    json!({ "id": id, "workspace": workspace }),
                    Duration::from_secs(60),
                )
                .await?;
            super::print(cli, &value, |v| {
                v["prompt"].as_str().unwrap_or_default().to_owned()
            });
        }
    }
    Ok(())
}

pub fn button_text(v: &Value) -> String {
    let mut lines = Vec::new();
    let buttons = v["buttons"].as_array().cloned().unwrap_or_default();
    if buttons.is_empty() {
        lines.push("no button".into());
    }
    for (i, b) in buttons.iter().enumerate() {
        lines.push(format!(
            "{}{}{}",
            if i == 0 { "button: " } else { "  menu: " },
            b["label"].as_str().unwrap_or("?"),
            b["reason"]
                .as_str()
                .map(|r| format!("  ({r})"))
                .unwrap_or_default()
        ));
    }
    match v["decided_by"].as_str() {
        Some("script") => {
            lines.push(format!(
                "decided by run {} ({}, {})",
                v["run"].as_str().unwrap_or("?"),
                trigger(&v["trigger"]),
                when(&v["run_started_ms"])
            ));
            let then = &v["scope_then"];
            lines.push(format!("  status then: {}", then["status"]));
            if let Some(pr) = then["pr"].as_object() {
                for key in [
                    "number",
                    "state",
                    "is_draft",
                    "mergeable",
                    "merge_state_status",
                    "review_decision",
                    "checks_rollup",
                    "is_in_merge_queue",
                    "auto_merge",
                    "updated_at",
                ] {
                    lines.push(format!(
                        "  pr.{key}: {}",
                        pr.get(key).unwrap_or(&Value::Null)
                    ));
                }
            } else {
                lines.push("  pr: none".into());
            }
            lines.push(format!(
                "  built-in step then: {}",
                then["suggested_action"]["label"].as_str().unwrap_or("none")
            ));
            lines.push(format!(
                "  built-in step now: {}",
                v["suggested_action"]["label"].as_str().unwrap_or("none")
            ));
            lines.push(format!(
                "replay it: icarus automation replay {}",
                v["run"].as_str().unwrap_or("?")
            ));
        }
        _ => lines
            .push("decided by the built-in resolver (no script buttons for this workspace)".into()),
    }
    lines.join("\n")
}
