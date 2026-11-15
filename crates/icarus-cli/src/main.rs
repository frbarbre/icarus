mod acp;
mod attach;
mod ports;
mod rules;
mod service;

use std::path::PathBuf;
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use icarus_client::Client;
use serde_json::{Value, json};

#[derive(Parser)]
#[command(
    name = "icarus",
    version,
    about = "Control an Icarus server from the shell"
)]
struct Cli {
    /// Server state directory (defaults to $ICARUS_HOME or ~/.icarus).
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the server process.
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    /// Manage terminal sessions owned by the server.
    Terminal {
        #[command(subcommand)]
        command: TerminalCommand,
    },
    /// Manage agent sessions (Claude Code, Codex) owned by the server.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Speak the Agent Client Protocol on stdio, backed by server-owned agent sessions (for editors such as Zed).
    Acp {
        /// claude or codex; defaults to the `agents.default` setting.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Open files in the Icarus window that owns their worktree.
    File {
        #[command(subcommand)]
        command: FileCommand,
    },
    /// Read and change settings in any layer; `explain` shows where a value comes from.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Import or export settings files (Orca's `orca-settings` v1 format).
    Settings {
        #[command(subcommand)]
        command: SettingsCommand,
    },
    /// Manage projects (git repositories).
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Manage workspaces (a worktree with its agents and terminals).
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// The Changes list: scopes, file type/area filters and why a file is classified as it is.
    Changes {
        #[command(subcommand)]
        command: ChangesCommand,
    },
    /// Named tab layouts (built-in and from settings `layouts`).
    Layout {
        #[command(subcommand)]
        command: LayoutCommand,
    },
    /// The connected Icarus app windows.
    App {
        #[command(subcommand)]
        command: AppCommand,
    },
    /// Pull requests of workspaces, through your `gh` login.
    Pr {
        #[command(subcommand)]
        command: PrCommand,
    },
    /// The automation rules: status, and a tick on demand.
    Rules {
        #[command(subcommand)]
        command: RulesCommand,
    },
    /// Attach context (PR comments, checks, files, text, agent turns) to a workspace's composer.
    Attach {
        #[command(subcommand)]
        command: attach::AttachCommand,
    },
    /// The attachments waiting in a workspace's composer.
    Composer {
        #[command(subcommand)]
        command: attach::ComposerCommand,
    },
    /// Services a workspace is running (dev servers, APIs, databases) and their URLs.
    Ports {
        #[command(subcommand)]
        command: ports::PortsCommand,
    },
}

#[derive(Subcommand)]
enum ChangesCommand {
    /// List changed files with their facet values (type, area, …) and the header totals.
    List {
        workspace: String,
        /// base (default), uncommitted, turn:<ref>|turn:latest or commit:<sha>.
        #[arg(long)]
        scope: Option<String>,
        /// Hide a facet value for this listing only, e.g. `type=generated,test`; repeatable.
        /// Without it the workspace's saved hidden values apply.
        #[arg(long = "hide", value_name = "FACET=VALUES")]
        hide: Vec<String>,
        /// Case-insensitive path substring.
        #[arg(long)]
        text: Option<String>,
    },
    /// Show each facet's value for a path and the rule that decided it.
    Explain { workspace: String, path: String },
    /// Change the workspace's saved hidden values (shared with every client).
    Filter {
        workspace: String,
        #[arg(long = "hide", value_name = "FACET=VALUES")]
        hide: Vec<String>,
        #[arg(long = "show", value_name = "FACET=VALUES")]
        show: Vec<String>,
        #[arg(long)]
        reset: bool,
    },
}

/// `type=generated,test` pairs into `{ "type": ["generated", "test"] }`.
fn facet_selection(pairs: &[String]) -> Result<Value> {
    let mut selection = serde_json::Map::new();
    for pair in pairs {
        let (facet, values) = pair
            .split_once('=')
            .with_context(|| format!("expected FACET=VALUES, got `{pair}`"))?;
        let entry = selection
            .entry(facet.trim().to_string())
            .or_insert_with(|| json!([]));
        if let Some(list) = entry.as_array_mut() {
            list.extend(
                values
                    .split(',')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(|v| json!(v)),
            );
        }
    }
    Ok(Value::Object(selection))
}

fn change_letter(status: &str) -> &'static str {
    match status {
        "added" => "A",
        "deleted" => "D",
        "renamed" => "R",
        "copied" => "C",
        _ => "M",
    }
}

#[derive(Subcommand)]
enum PrCommand {
    /// Show the pull request of a workspace's branch.
    Status {
        workspace: String,
        /// Skip the short-lived cache and ask GitHub now.
        #[arg(long)]
        refresh: bool,
    },
    /// List the preview deployments of a workspace's pull request (`*` marks the primary one).
    Previews {
        workspace: String,
        /// Skip the short-lived cache and ask GitHub now.
        #[arg(long)]
        refresh: bool,
    },
    /// Print the URL of a workspace's preview deployment.
    Preview {
        workspace: String,
        /// The app to show; defaults to the primary preview (`previews.primary_app`).
        #[arg(long)]
        app: Option<String>,
        /// Open the preview in the default browser.
        #[arg(long)]
        browser: bool,
        /// Skip the short-lived cache and ask GitHub now.
        #[arg(long)]
        refresh: bool,
    },
}

#[derive(Subcommand)]
enum RulesCommand {
    /// Show whether the rules are on, and what the last tick planned and did.
    Status,
    /// Evaluate the rules now. Ticks are dry runs until `rules.armed` is true.
    Tick {
        /// Only print the planned actions.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum FileCommand {
    /// Open a file, optionally at a line and column. Accepts `path:line:column` too, as `launch-editor` passes it.
    Open {
        /// The file, or `path:line[:column]`.
        target: String,
        /// Line, also accepted positionally (e.g. from a `LAUNCH_EDITOR` script).
        line_arg: Option<u32>,
        column_arg: Option<u32>,
        #[arg(long)]
        line: Option<u32>,
        #[arg(long)]
        column: Option<u32>,
        /// Bring the window to the front.
        #[arg(long, default_value_t = true)]
        focus: bool,
    },
}

#[derive(Subcommand)]
enum LayoutCommand {
    /// Every layout available to a project (or the user, without --project).
    List {
        #[arg(long)]
        project: Option<String>,
    },
    /// One layout's tab tree as JSON.
    Show {
        name: String,
        #[arg(long)]
        project: Option<String>,
    },
}

/// One line per pane: `row > column > [agent, browser https://…]`.
fn layout_summary(node: &Value) -> String {
    fn walk(node: &Value, path: &str, out: &mut Vec<String>) {
        if let Some(children) = node["children"].as_array() {
            let axis = node["split"].as_str().unwrap_or("split");
            let path = if path.is_empty() {
                axis.to_owned()
            } else {
                format!("{path} > {axis}")
            };
            for child in children {
                walk(child, &path, out);
            }
            return;
        }
        let tabs: Vec<String> = node["tabs"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|tab| {
                let kind = tab["type"].as_str().unwrap_or("?");
                let detail = tab["url"]
                    .as_str()
                    .or(tab["path"].as_str())
                    .or(tab["agent_id"].as_str())
                    .or(tab["command"].as_str());
                match detail {
                    Some(detail) if !detail.is_empty() => format!("{kind} {detail}"),
                    _ => kind.to_owned(),
                }
            })
            .collect();
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path} > ")
        };
        out.push(format!("{prefix}[{}]", tabs.join(", ")));
    }
    let mut out = Vec::new();
    walk(node, "", &mut out);
    out.join("\n")
}

#[derive(Subcommand)]
enum AppCommand {
    /// Save each app window's current frame as a PNG; needs no screen-recording permission.
    Capture {
        /// Directory for the PNGs (default: a new temporary directory).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Seconds to wait for the app to finish writing.
        #[arg(long, default_value_t = 15)]
        timeout: u64,
    },
}

/// Splits `path:line:column` the way editors' "open at position" conventions do.
fn split_position(target: &str) -> (String, Option<u32>, Option<u32>) {
    let mut parts: Vec<&str> = target.rsplitn(3, ':').collect();
    parts.reverse();
    match parts.as_slice() {
        [path, line, column] if line.parse::<u32>().is_ok() && column.parse::<u32>().is_ok() => {
            (path.to_string(), line.parse().ok(), column.parse().ok())
        }
        [head, tail @ ..] if tail.len() == 1 && tail[0].parse::<u32>().is_ok() => {
            (head.to_string(), tail[0].parse().ok(), None)
        }
        [path, middle, tail] if tail.parse::<u32>().is_ok() => {
            (format!("{path}:{middle}"), tail.parse().ok(), None)
        }
        _ => (target.to_string(), None, None),
    }
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Print the resolved value of a dotted key (or everything).
    Get {
        key: Option<String>,
        #[arg(long)]
        project: Option<String>,
    },
    /// Set a dotted key; the value is parsed as JSON when it can be.
    Set {
        key: String,
        value: String,
        /// user (default), project (committed icarus.json) or local (untracked icarus.local.json).
        #[arg(long)]
        layer: Option<String>,
        #[arg(long)]
        project: Option<String>,
    },
    Unset {
        key: String,
        #[arg(long)]
        layer: Option<String>,
        #[arg(long)]
        project: Option<String>,
    },
    /// Show which layer a value comes from and what it overrides.
    Explain {
        key: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// Print the JSON Schema of the settings files.
    Schema,
}

#[derive(Subcommand)]
enum SettingsCommand {
    /// Import an `orca settings export` file into your user settings and matching projects.
    Import {
        file: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    /// Write your settings in Orca's `orca-settings` v1 format.
    Export {
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ProjectCommand {
    /// Register a git repository.
    Add {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        base: Option<String>,
    },
    List,
    Remove {
        id: String,
    },
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    /// Create a worktree workspace; it starts with an agent, optionally with a prompt.
    Create {
        #[arg(long)]
        project: String,
        name: String,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        fetch: bool,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        /// Link a Linear issue by identifier, e.g. ABC-12.
        #[arg(long)]
        from_linear: Option<String>,
        #[arg(long)]
        no_agent: bool,
        /// Open the workspace with this tab layout (see `icarus layout list`).
        #[arg(long)]
        layout: Option<String>,
        /// The PR URL for `{pr_url}` in the layout, when the branch has no PR yet.
        #[arg(long)]
        pr_url: Option<String>,
    },
    List {
        #[arg(long)]
        project: Option<String>,
    },
    /// Arrange the workspace's tabs with a named layout; an app that opens later applies it then.
    Layout {
        id: String,
        layout: String,
        #[arg(long)]
        pr_url: Option<String>,
    },
    Show {
        id: String,
    },
    /// Change a workspace's status, title or base.
    Set {
        id: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        base: Option<String>,
    },
    /// List changed files against the base (or only uncommitted ones).
    Changes {
        id: String,
        #[arg(long)]
        uncommitted: bool,
    },
    /// List the agent turns recorded in the workspace, newest first.
    Turns {
        id: String,
    },
    Remove {
        id: String,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        keep_branch: bool,
    },
    /// Show the workspace's next step (the next-action button); --run runs it.
    NextAction {
        id: String,
        #[arg(long)]
        run: bool,
    },
}

#[derive(Subcommand)]
enum AgentCommand {
    /// Start an agent session; with --prompt it starts working right away.
    Create {
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// claude or codex; defaults to the `agents.default` setting.
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        /// Ask for permissions instead of bypassing them.
        #[arg(long)]
        ask_permissions: bool,
        /// Resume an earlier provider session.
        #[arg(long)]
        resume: Option<String>,
    },
    /// List agent sessions.
    List,
    /// Send a message to an agent.
    Send { id: String, text: String },
    /// Print an agent's transcript.
    Read { id: String },
    /// Wait until the agent finishes its current turn.
    Wait {
        id: String,
        #[arg(long, default_value_t = 600_000)]
        timeout_ms: u64,
    },
    /// Stop the agent's current turn.
    Interrupt { id: String },
    /// End an agent session.
    Kill {
        id: String,
        #[arg(long)]
        remove: bool,
    },
}

#[derive(Subcommand)]
enum ServerCommand {
    /// Start the server in the background if it isn't running.
    Start,
    /// Stop the server and every session it owns.
    Stop,
    /// Show whether the server is running.
    Status,
    /// Start the server at login (launchd on macOS, a systemd user unit on Linux).
    Install,
    /// Stop starting the server at login.
    Uninstall,
}

#[derive(Subcommand)]
enum TerminalCommand {
    /// Start a terminal session. Without a command it runs a login shell.
    Create {
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        /// Command and arguments to run, after `--`.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// List terminal sessions.
    List,
    /// Print the rendered screen, or all scrollback with --scrollback.
    Read {
        id: String,
        #[arg(long)]
        scrollback: bool,
    },
    /// Type text into a terminal; --enter appends a newline.
    Send {
        id: String,
        text: String,
        #[arg(long)]
        enter: bool,
    },
    /// Wait until the output contains text, or the process exits.
    Wait {
        id: String,
        #[arg(long)]
        contains: Option<String>,
        #[arg(long)]
        exit: bool,
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
    },
    /// Resize a terminal.
    Resize { id: String, columns: u16, rows: u16 },
    /// Kill a terminal's process.
    Kill {
        id: String,
        /// Also forget the session.
        #[arg(long)]
        remove: bool,
    },
}

fn home(cli: &Cli) -> Result<PathBuf> {
    match &cli.home {
        Some(home) => Ok(home.clone()),
        None => icarus_client::default_home(),
    }
}

async fn connect(home: &std::path::Path) -> Result<std::sync::Arc<Client>> {
    let (client, _events) = Client::connect(home, "icarus-cli")
        .await
        .context("no Icarus server is running (start one with `icarus server start`)")?;
    Ok(client)
}

fn open_in_browser(url: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let status = Process::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("could not run {opener}"))?;
    // Why not on Windows: explorer.exe exits non-zero even when it opened the URL.
    if !status.success() && !cfg!(windows) {
        bail!("{opener} failed to open {url}");
    }
    Ok(())
}

fn server_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let sibling = exe.with_file_name("icarus-server");
    if sibling.exists() {
        return Ok(sibling);
    }
    Ok(PathBuf::from("icarus-server"))
}

fn print(cli: &Cli, value: &Value, human: impl FnOnce(&Value) -> String) {
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_default()
        );
    } else {
        let text = human(value);
        if !text.is_empty() {
            println!("{text}");
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = home(&cli)?;
    match &cli.command {
        Command::Server { command } => match command {
            ServerCommand::Start => {
                if Client::connect(&home, "icarus-cli").await.is_ok() {
                    print(&cli, &json!({ "running": true, "started": false }), |_| {
                        "server already running".into()
                    });
                    return Ok(());
                }
                std::fs::create_dir_all(&home)?;
                let log = std::fs::File::create(home.join("server.log"))?;
                use std::os::unix::process::CommandExt;
                Process::new(server_binary()?)
                    .args(["--home", &home.to_string_lossy(), "serve"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(log)
                    .process_group(0)
                    .spawn()
                    .context("spawn icarus-server")?;
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if Client::connect(&home, "icarus-cli").await.is_ok() {
                        break;
                    }
                    if Instant::now() > deadline {
                        bail!(
                            "server did not come up; see {}",
                            home.join("server.log").display()
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                print(&cli, &json!({ "running": true, "started": true }), |_| {
                    "server started".into()
                });
            }
            ServerCommand::Stop => {
                let client = connect(&home).await?;
                let value = client.request("server.shutdown", json!({})).await?;
                print(&cli, &value, |_| "server stopping".into());
            }
            ServerCommand::Install => {
                let path = service::install(&server_binary()?, &home)?;
                print(&cli, &json!({ "installed": path }), |_| {
                    format!("installed {}", path.display())
                });
            }
            ServerCommand::Uninstall => {
                let removed = service::uninstall()?;
                print(&cli, &json!({ "removed": removed }), |_| {
                    if removed {
                        "removed".into()
                    } else {
                        "not installed".into()
                    }
                });
            }
            ServerCommand::Status => match Client::connect(&home, "icarus-cli").await {
                Ok((client, _)) => {
                    let value = client.request("server.info", json!({})).await?;
                    print(&cli, &value, |v| {
                        format!(
                            "running · version {} · pid {} · {} terminal(s)",
                            v["version"].as_str().unwrap_or("?"),
                            v["pid"],
                            v["terminals"]
                        )
                    });
                }
                Err(_) => {
                    print(&cli, &json!({ "running": false }), |_| "not running".into());
                    std::process::exit(3);
                }
            },
        },
        Command::Acp { provider } => {
            acp::run(home.clone(), provider.clone()).await?;
        }
        Command::File { command } => {
            let client = connect(&home).await?;
            match command {
                FileCommand::Open {
                    target,
                    line_arg,
                    column_arg,
                    line,
                    column,
                    focus,
                } => {
                    let (path, parsed_line, parsed_column) = split_position(target);
                    let path =
                        std::fs::canonicalize(&path).unwrap_or_else(|_| PathBuf::from(&path));
                    let value = client
                        .request(
                            "client.open_file",
                            json!({
                                "path": path,
                                "line": line.or(*line_arg).or(parsed_line),
                                "column": column.or(*column_arg).or(parsed_column),
                                "focus": focus,
                            }),
                        )
                        .await?;
                    print(&cli, &value, |_| String::new());
                }
            }
        }
        Command::Layout { command } => {
            let client = connect(&home).await?;
            match command {
                LayoutCommand::List { project } => {
                    let value = client
                        .request("layout.list", json!({ "project": project }))
                        .await?;
                    print(&cli, &value, |v| {
                        v.as_object()
                            .map(|layouts| {
                                layouts
                                    .iter()
                                    .map(|(name, layout)| {
                                        let tree = layout_summary(layout).replace('\n', "\n  ");
                                        format!("{name}\n  {tree}")
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default()
                    });
                }
                LayoutCommand::Show { name, project } => {
                    let value = client
                        .request("layout.get", json!({ "name": name, "project": project }))
                        .await?;
                    print(&cli, &value, |v| {
                        serde_json::to_string_pretty(v).unwrap_or_default()
                    });
                }
            }
        }
        Command::App { command } => {
            let client = connect(&home).await?;
            match command {
                AppCommand::Capture { out, timeout } => {
                    let dir = match out {
                        Some(dir) => {
                            std::fs::create_dir_all(dir)?;
                            std::fs::canonicalize(dir)?
                        }
                        None => std::env::temp_dir().join(format!(
                            "icarus-capture-{}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)?
                                .as_millis()
                        )),
                    };
                    std::fs::create_dir_all(&dir)?;
                    let manifest = dir.join("manifest.json");
                    if manifest.exists() {
                        std::fs::remove_file(&manifest)?;
                    }
                    client
                        .request("client.capture", json!({ "dir": dir }))
                        .await?;
                    let deadline = std::time::Instant::now() + Duration::from_secs(*timeout);
                    let value: Value = loop {
                        if let Ok(text) = std::fs::read_to_string(&manifest)
                            && let Ok(value) = serde_json::from_str(&text)
                        {
                            break value;
                        }
                        if std::time::Instant::now() > deadline {
                            anyhow::bail!(
                                "the app did not finish the capture in {timeout}s (is it built with frame capture?)"
                            );
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    };
                    print(&cli, &value, |v| {
                        let mut lines: Vec<String> = v["files"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|f| f.as_str().map(str::to_string))
                            .collect();
                        lines.extend(
                            v["errors"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|e| e.as_str().map(|e| format!("error: {e}"))),
                        );
                        lines.join("\n")
                    });
                }
            }
        }
        Command::Config { command } => {
            let client = connect(&home).await?;
            let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default();
            match command {
                ConfigCommand::Get { key, project } => {
                    let value = client
                        .request("config.get", json!({ "key": key, "project": project }))
                        .await?;
                    print(&cli, &value, |v| match v {
                        Value::String(s) => s.clone(),
                        other => pretty(other),
                    });
                }
                ConfigCommand::Set {
                    key,
                    value,
                    layer,
                    project,
                } => {
                    let parsed = serde_json::from_str::<Value>(value)
                        .unwrap_or_else(|_| Value::String(value.clone()));
                    let result = client
                        .request(
                            "config.set",
                            json!({ "key": key, "value": parsed, "layer": layer, "project": project }),
                        )
                        .await?;
                    print(&cli, &result, |v| {
                        format!(
                            "set in {} ({})",
                            v["layer"].as_str().unwrap_or("?"),
                            v["path"].as_str().unwrap_or("?")
                        )
                    });
                }
                ConfigCommand::Unset {
                    key,
                    layer,
                    project,
                } => {
                    let result = client
                        .request(
                            "config.unset",
                            json!({ "key": key, "layer": layer, "project": project }),
                        )
                        .await?;
                    print(&cli, &result, |_| String::new());
                }
                ConfigCommand::Explain { key, project } => {
                    let value = client
                        .request("config.explain", json!({ "key": key, "project": project }))
                        .await?;
                    print(&cli, &value, |v| {
                        let mut lines = vec![format!("{} = {}", key, v["value"])];
                        if let Some(from) = v["from"].as_object() {
                            lines.push(format!(
                                "from {} ({})",
                                from["layer"].as_str().unwrap_or("?"),
                                from["label"].as_str().unwrap_or("?")
                            ));
                        }
                        for overridden in v["overridden"].as_array().into_iter().flatten() {
                            lines.push(format!(
                                "overrides {} = {}",
                                overridden["layer"].as_str().unwrap_or("?"),
                                overridden["value"]
                            ));
                        }
                        lines.join("\n")
                    });
                }
                ConfigCommand::Schema => {
                    let value = client.request("config.schema", json!({})).await?;
                    println!("{}", pretty(&value));
                }
            }
        }
        Command::Settings { command } => {
            let client = connect(&home).await?;
            match command {
                SettingsCommand::Import { file, dry_run } => {
                    let path = std::fs::canonicalize(file).unwrap_or(file.clone());
                    let value = client
                        .request(
                            "settings.import_orca",
                            json!({ "path": path, "dry_run": dry_run }),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        let mut lines = vec![format!("{} settings mapped", v["mapped"])];
                        for written in v["written"].as_array().into_iter().flatten() {
                            lines.push(format!(
                                "wrote {} ({})",
                                written["path"].as_str().unwrap_or("?"),
                                written["layer"].as_str().unwrap_or("?")
                            ));
                        }
                        for remote in v["unmatched_projects"].as_array().into_iter().flatten() {
                            lines.push(format!("no project for {remote}; add it with `icarus project add` and import again"));
                        }
                        for key in v["unmapped"].as_array().into_iter().flatten() {
                            lines.push(format!("not mapped: {}", key.as_str().unwrap_or("?")));
                        }
                        lines.join("\n")
                    });
                }
                SettingsCommand::Export { output } => {
                    let value = client.request("settings.export_orca", json!({})).await?;
                    let text = value["text"].as_str().unwrap_or_default();
                    match output {
                        Some(path) => std::fs::write(path, text)?,
                        None => print!("{text}"),
                    }
                }
            }
        }
        Command::Project { command } => {
            let client = connect(&home).await?;
            let value = match command {
                ProjectCommand::Add { path, name, base } => {
                    let path = std::fs::canonicalize(path).unwrap_or(path.clone());
                    client
                        .request(
                            "project.add",
                            json!({ "path": path, "name": name, "base_ref": base }),
                        )
                        .await?
                }
                ProjectCommand::List => client.request("project.list", json!({})).await?,
                ProjectCommand::Remove { id } => {
                    client
                        .request("project.remove", json!({ "id": id }))
                        .await?
                }
            };
            print(&cli, &value, |v| match v.as_array() {
                Some(items) => items
                    .iter()
                    .map(|p| {
                        format!(
                            "{}  {}  {}  base {}",
                            p["id"].as_str().unwrap_or_default(),
                            p["name"].as_str().unwrap_or_default(),
                            p["root"].as_str().unwrap_or_default(),
                            p["base_ref"].as_str().unwrap_or("-")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                None => v["id"].as_str().map(str::to_string).unwrap_or_default(),
            });
        }
        Command::Workspace { command } => {
            let client = connect(&home).await?;
            let summary = |w: &Value| {
                format!(
                    "{}  {}  {}  {}",
                    w["id"].as_str().unwrap_or_default(),
                    w["name"].as_str().unwrap_or_default(),
                    w["status"].as_str().unwrap_or("-"),
                    w["path"].as_str().unwrap_or_default()
                )
            };
            match command {
                WorkspaceCommand::Create {
                    project,
                    name,
                    branch,
                    base,
                    fetch,
                    status,
                    prompt,
                    from_linear,
                    no_agent,
                    layout,
                    pr_url,
                } => {
                    let issue = from_linear
                        .as_ref()
                        .map(|id| json!({ "provider": "linear", "id": id }));
                    let value = client
                        .request_timeout(
                            "workspace.create",
                            json!({
                                "project": project, "name": name, "branch": branch, "base": base,
                                "fetch": fetch, "status": status, "prompt": prompt, "issue": issue,
                                "agent": !no_agent, "layout": layout, "pr_url": pr_url,
                            }),
                            Duration::from_secs(300),
                        )
                        .await?;
                    print(&cli, &value, summary);
                }
                WorkspaceCommand::Layout { id, layout, pr_url } => {
                    let value = client
                        .request_timeout(
                            "workspace.layout",
                            json!({ "id": id, "layout": layout, "pr_url": pr_url }),
                            Duration::from_secs(120),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        let delivered = v["delivered"].as_u64().unwrap_or(0);
                        let tabs = layout_summary(&v["layout"]);
                        if delivered == 0 {
                            format!(
                                "{tabs}\n(no app connected; it applies when the workspace opens)"
                            )
                        } else {
                            tabs
                        }
                    });
                }
                WorkspaceCommand::List { project } => {
                    let value = client
                        .request("workspace.list", json!({ "project": project }))
                        .await?;
                    print(&cli, &value, |v| {
                        v.as_array()
                            .map(|i| i.iter().map(summary).collect::<Vec<_>>().join("\n"))
                            .unwrap_or_default()
                    });
                }
                WorkspaceCommand::Show { id } => {
                    let value = client
                        .request("workspace.info", json!({ "id": id }))
                        .await?;
                    print(&cli, &value, |v| {
                        serde_json::to_string_pretty(v).unwrap_or_default()
                    });
                }
                WorkspaceCommand::Set {
                    id,
                    status,
                    title,
                    base,
                } => {
                    let value = client
                        .request(
                            "workspace.set",
                            json!({ "id": id, "status": status, "title": title, "base": base }),
                        )
                        .await?;
                    print(&cli, &value, summary);
                }
                WorkspaceCommand::Changes { id, uncommitted } => {
                    let value = client
                        .request(
                            "workspace.changes",
                            json!({ "id": id, "uncommitted_only": uncommitted }),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        let mut lines: Vec<String> = v["rows"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|r| {
                                let letter = match r["status"].as_str().unwrap_or_default() {
                                    "added" => "A",
                                    "deleted" => "D",
                                    "renamed" => "R",
                                    "copied" => "C",
                                    _ => "M",
                                };
                                let counts = match (r["added"].as_u64(), r["removed"].as_u64()) {
                                    (Some(a), Some(d)) => format!("+{a} -{d}"),
                                    _ => String::new(),
                                };
                                format!(
                                    "{letter}  {}  {counts}",
                                    r["path"].as_str().unwrap_or_default()
                                )
                            })
                            .collect();
                        lines.push(format!(
                            "{} files  +{} -{}",
                            lines.len(),
                            v["added"],
                            v["removed"]
                        ));
                        lines.join("\n")
                    });
                }
                WorkspaceCommand::Turns { id } => {
                    let value = client
                        .request_timeout(
                            "workspace.turns",
                            json!({ "id": id }),
                            Duration::from_secs(60),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        v.as_array()
                            .into_iter()
                            .flatten()
                            .map(|t| {
                                let prompt = t["meta"]["prompt"].as_str().unwrap_or("Agent turn");
                                format!(
                                    "{}  {} files +{} -{}  {}",
                                    t["name"].as_str().unwrap_or_default(),
                                    t["files"],
                                    t["added"],
                                    t["removed"],
                                    prompt.lines().next().unwrap_or_default()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    });
                }
                WorkspaceCommand::Remove {
                    id,
                    force,
                    keep_branch,
                } => {
                    let value = client
                        .request(
                            "workspace.remove",
                            json!({ "id": id, "force": force, "delete_branch": !keep_branch }),
                        )
                        .await?;
                    print(&cli, &value, |_| "removed".into());
                }
                WorkspaceCommand::NextAction { id, run } => {
                    if *run {
                        let value = client
                            .request_timeout(
                                "workspace.run_next_action",
                                json!({ "id": id }),
                                Duration::from_secs(120),
                            )
                            .await?;
                        print(&cli, &value, rules::ran_next_action);
                    } else {
                        let value = client
                            .request("workspace.next_action", json!({ "id": id }))
                            .await?;
                        print(&cli, &value, rules::next_action);
                    }
                }
            }
        }
        Command::Changes { command } => {
            let client = connect(&home).await?;
            match command {
                ChangesCommand::List {
                    workspace,
                    scope,
                    hide,
                    text,
                } => {
                    let mut request = json!({ "id": workspace, "scope": scope, "text": text });
                    if !hide.is_empty() {
                        request["hide"] = facet_selection(hide)?;
                    }
                    let value = client
                        .request_timeout("workspace.changes", request, Duration::from_secs(120))
                        .await?;
                    print(&cli, &value, |v| {
                        let mut lines: Vec<String> = v["rows"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|r| {
                                let counts = match (r["added"].as_u64(), r["removed"].as_u64()) {
                                    (Some(a), Some(d)) => format!("+{a} -{d}"),
                                    _ => "-".into(),
                                };
                                let facets: Vec<String> = r["facets"]
                                    .as_object()
                                    .into_iter()
                                    .flatten()
                                    .filter_map(|(_, value)| value.as_str().map(str::to_string))
                                    .collect();
                                format!(
                                    "{}  {:<10}  {}  [{}]",
                                    change_letter(r["status"].as_str().unwrap_or_default()),
                                    counts,
                                    r["path"].as_str().unwrap_or_default(),
                                    facets.join(", ")
                                )
                            })
                            .collect();
                        lines.push(format!(
                            "{} of {} files  +{} -{}  ({})",
                            v["totals"]["files"],
                            v["unfiltered"]["files"],
                            v["totals"]["added"],
                            v["totals"]["removed"],
                            v["scope"].as_str().unwrap_or_default()
                        ));
                        lines.join("\n")
                    });
                }
                ChangesCommand::Explain { workspace, path } => {
                    let value = client
                        .request_timeout(
                            "workspace.changes.explain",
                            json!({ "id": workspace, "path": path }),
                            Duration::from_secs(120),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        let mut lines = vec![format!(
                            "{}  ({})",
                            v["path"].as_str().unwrap_or_default(),
                            if v["row"].is_null() {
                                "unchanged"
                            } else {
                                "changed"
                            }
                        )];
                        for facet in v["facets"].as_array().into_iter().flatten() {
                            let decided = match facet["rule"].as_u64() {
                                Some(index) => format!(
                                    "rule {index}{}",
                                    facet["rule_id"]
                                        .as_str()
                                        .map(|id| format!(" `{id}`"))
                                        .unwrap_or_default()
                                ),
                                None => "default".into(),
                            };
                            lines.push(format!(
                                "{} = {}  ({decided})",
                                facet["facet"].as_str().unwrap_or_default(),
                                facet["value"].as_str().unwrap_or("none"),
                            ));
                        }
                        lines.push(format!("attributes: {}", v["attributes"]));
                        lines.push(format!(
                            "imports only: {}  generated heuristic: {}  test heuristic: {}",
                            v["imports_only"], v["generated_heuristic"], v["test_heuristic"]
                        ));
                        lines.join("\n")
                    });
                }
                ChangesCommand::Filter {
                    workspace,
                    hide,
                    show,
                    reset,
                } => {
                    let value = client
                        .request(
                            "workspace.changes.filter",
                            json!({
                                "id": workspace, "reset": reset,
                                "hide": facet_selection(hide)?, "show": facet_selection(show)?,
                            }),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        let hidden: Vec<String> = v["hidden"]
                            .as_object()
                            .into_iter()
                            .flatten()
                            .map(|(facet, values)| {
                                let values: Vec<&str> = values
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter_map(Value::as_str)
                                    .collect();
                                format!("{facet}={}", values.join(","))
                            })
                            .collect();
                        if hidden.is_empty() {
                            "nothing hidden".into()
                        } else {
                            format!("hidden: {}", hidden.join(" "))
                        }
                    });
                }
            }
        }
        Command::Pr { command } => {
            let client = connect(&home).await?;
            match command {
                PrCommand::Status { workspace, refresh } => {
                    let value = client
                        .request_timeout(
                            "pr.status",
                            json!({ "workspace": workspace, "refresh": refresh }),
                            Duration::from_secs(60),
                        )
                        .await?;
                    print(&cli, &value, rules::pr_status);
                }
                PrCommand::Previews { workspace, refresh } => {
                    let value = client
                        .request_timeout(
                            "pr.previews",
                            json!({ "workspace": workspace, "refresh": refresh }),
                            Duration::from_secs(60),
                        )
                        .await?;
                    print(&cli, &value, rules::previews);
                }
                PrCommand::Preview {
                    workspace,
                    app,
                    browser,
                    refresh,
                } => {
                    let value = client
                        .request_timeout(
                            "pr.previews",
                            json!({ "workspace": workspace, "refresh": refresh }),
                            Duration::from_secs(60),
                        )
                        .await?;
                    let preview =
                        rules::pick_preview(&value, app.as_deref()).map_err(anyhow::Error::msg)?;
                    let Some(url) = preview["url"].as_str() else {
                        bail!(
                            "the {} preview has no URL yet ({})",
                            preview["app"].as_str().unwrap_or_default(),
                            preview["state"].as_str().unwrap_or("unknown")
                        );
                    };
                    if *browser {
                        open_in_browser(url)?;
                    }
                    print(&cli, preview, |_| url.to_owned());
                }
            }
        }
        Command::Ports { command } => {
            let client = connect(&home).await?;
            ports::run(&cli, &client, command).await?;
        }
        Command::Attach { command } => {
            let (method, params) = attach::attach_request(command)?;
            let client = connect(&home).await?;
            let value = client
                .request_timeout(method, params, Duration::from_secs(180))
                .await?;
            print(&cli, &value, attach::attached);
        }
        Command::Composer { command } => {
            let client = connect(&home).await?;
            match command {
                attach::ComposerCommand::Show { workspace, all } => {
                    let value = client
                        .request(
                            "attachment.list",
                            json!({ "workspace": workspace, "include_sent": all }),
                        )
                        .await?;
                    print(&cli, &value, attach::composer);
                }
                attach::ComposerCommand::Clear { workspace } => {
                    let value = client
                        .request("attachment.clear", json!({ "workspace": workspace }))
                        .await?;
                    print(&cli, &value, |v| {
                        format!("removed {} attachment(s)", v["removed"])
                    });
                }
            }
        }
        Command::Rules { command } => {
            let client = connect(&home).await?;
            match command {
                RulesCommand::Status => {
                    let value = client.request("rules.status", json!({})).await?;
                    print(&cli, &value, rules::status);
                }
                RulesCommand::Tick { dry_run } => {
                    let params = if *dry_run {
                        json!({ "dry_run": true })
                    } else {
                        json!({})
                    };
                    let value = client
                        .request_timeout("rules.tick", params, Duration::from_secs(300))
                        .await?;
                    print(&cli, &value, rules::tick);
                }
            }
        }
        Command::Agent { command } => {
            let client = connect(&home).await?;
            match command {
                AgentCommand::Create {
                    cwd,
                    provider,
                    title,
                    workspace,
                    model,
                    prompt,
                    ask_permissions,
                    resume,
                } => {
                    let cwd = match cwd {
                        Some(cwd) => cwd.clone(),
                        None => std::env::current_dir()?,
                    };
                    let value = client
                        .request(
                            "agent.create",
                            json!({
                                "cwd": cwd, "provider": provider, "title": title, "workspace": workspace,
                                "model": model, "prompt": prompt, "bypass_permissions": !ask_permissions,
                                "resume": resume,
                            }),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        v["id"].as_str().unwrap_or_default().to_string()
                    });
                }
                AgentCommand::List => {
                    let value = client.request("agent.list", json!({})).await?;
                    print(&cli, &value, |v| {
                        v.as_array()
                            .map(|items| {
                                items
                                    .iter()
                                    .map(|a| {
                                        format!(
                                            "{}  {}  {}  {}",
                                            a["id"].as_str().unwrap_or_default(),
                                            a["state"].as_str().unwrap_or_default(),
                                            a["provider"].as_str().unwrap_or_default(),
                                            a["title"].as_str().unwrap_or("-")
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default()
                    });
                }
                AgentCommand::Send { id, text } => {
                    let value = client
                        .request("agent.send", json!({ "id": id, "text": text }))
                        .await?;
                    print(&cli, &value, |_| String::new());
                }
                AgentCommand::Read { id } => {
                    let value = client.request("agent.read", json!({ "id": id })).await?;
                    print(&cli, &value, |v| {
                        v["text"].as_str().unwrap_or_default().trim().to_string()
                    });
                }
                AgentCommand::Wait { id, timeout_ms } => {
                    let value = client
                        .request_timeout(
                            "agent.wait",
                            json!({ "id": id, "timeout_ms": timeout_ms }),
                            Duration::from_millis(timeout_ms + 5_000),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        v["state"].as_str().unwrap_or_default().to_string()
                    });
                }
                AgentCommand::Interrupt { id } => {
                    let value = client
                        .request("agent.interrupt", json!({ "id": id }))
                        .await?;
                    print(&cli, &value, |_| String::new());
                }
                AgentCommand::Kill { id, remove } => {
                    let value = client
                        .request("agent.kill", json!({ "id": id, "remove": remove }))
                        .await?;
                    print(&cli, &value, |_| String::new());
                }
            }
        }
        Command::Terminal { command } => {
            let client = connect(&home).await?;
            match command {
                TerminalCommand::Create {
                    cwd,
                    label,
                    workspace,
                    command,
                } => {
                    let cwd = match cwd {
                        Some(cwd) => cwd.clone(),
                        None => std::env::current_dir()?,
                    };
                    let value = client
                        .request(
                            "terminal.create",
                            json!({
                                "cwd": cwd,
                                "label": label,
                                "workspace": workspace,
                                "command": if command.is_empty() { Value::Null } else { json!(command) },
                            }),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        v["id"].as_str().unwrap_or_default().to_string()
                    });
                }
                TerminalCommand::List => {
                    let value = client.request("terminal.list", json!({})).await?;
                    print(&cli, &value, |v| {
                        v.as_array()
                            .map(|items| {
                                items
                                    .iter()
                                    .map(|t| {
                                        format!(
                                            "{}  {}  {}  {}",
                                            t["id"].as_str().unwrap_or_default(),
                                            if t["running"] == json!(true) {
                                                "running"
                                            } else {
                                                "exited"
                                            },
                                            t["label"]
                                                .as_str()
                                                .or(t["title"].as_str())
                                                .unwrap_or("-"),
                                            t["cwd"].as_str().unwrap_or_default()
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default()
                    });
                }
                TerminalCommand::Read { id, scrollback } => {
                    let value = client
                        .request(
                            "terminal.read",
                            json!({ "id": id, "scrollback": scrollback }),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        v["text"]
                            .as_str()
                            .unwrap_or_default()
                            .trim_end()
                            .to_string()
                    });
                }
                TerminalCommand::Send { id, text, enter } => {
                    let text = if *enter {
                        format!("{text}\r")
                    } else {
                        text.clone()
                    };
                    let value = client
                        .request("terminal.write", json!({ "id": id, "text": text }))
                        .await?;
                    print(&cli, &value, |_| String::new());
                }
                TerminalCommand::Wait {
                    id,
                    contains,
                    exit,
                    timeout_ms,
                } => {
                    let value = client
                        .request_timeout(
                            "terminal.wait",
                            json!({ "id": id, "contains": contains, "exit": exit, "timeout_ms": timeout_ms }),
                            Duration::from_millis(timeout_ms + 5_000),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        if v["running"] == json!(true) {
                            "matched".into()
                        } else {
                            format!("exited with {}", v["exit_code"])
                        }
                    });
                }
                TerminalCommand::Resize { id, columns, rows } => {
                    let value = client
                        .request(
                            "terminal.resize",
                            json!({ "id": id, "columns": columns, "rows": rows }),
                        )
                        .await?;
                    print(&cli, &value, |_| String::new());
                }
                TerminalCommand::Kill { id, remove } => {
                    let value = client
                        .request("terminal.kill", json!({ "id": id, "remove": remove }))
                        .await?;
                    print(&cli, &value, |_| String::new());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{facet_selection, split_position};
    use serde_json::json;

    #[test]
    fn hide_flags_group_values_by_facet() {
        let pairs = [
            "type=generated,test".to_string(),
            "area=backend".into(),
            "type=docs".into(),
        ];
        assert_eq!(
            facet_selection(&pairs).expect("parse"),
            json!({ "type": ["generated", "test", "docs"], "area": ["backend"] })
        );
        assert!(facet_selection(&["type".to_string()]).is_err());
    }

    #[test]
    fn positions_parse_like_launch_editor_targets() {
        assert_eq!(
            split_position("/w/a.tsx:12:5"),
            ("/w/a.tsx".into(), Some(12), Some(5))
        );
        assert_eq!(
            split_position("/w/a.tsx:12"),
            ("/w/a.tsx".into(), Some(12), None)
        );
        assert_eq!(split_position("/w/a.tsx"), ("/w/a.tsx".into(), None, None));
        assert_eq!(
            split_position("/w/odd:name.ts:3"),
            ("/w/odd:name.ts".into(), Some(3), None)
        );
    }
}
