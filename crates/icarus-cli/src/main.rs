mod acp;
mod attach;
mod extras;
mod git_ops;
mod issue;
mod linear;
mod notes;
mod orca_state;
mod ports;
mod recipe;
mod review;
mod rules;
mod service;
mod term_attach;
mod theme;
mod update;

use std::path::PathBuf;
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use icarus_client::Client;
use icarus_settings::zed_import;
use serde_json::{Value, json};

/// The release workflow stamps `ICARUS_VERSION` from the tag; local builds report the crate version.
const VERSION: &str = match option_env!("ICARUS_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Parser)]
#[command(
    name = "icarus",
    version = VERSION,
    about = "Control an Icarus server from the shell"
)]
struct Cli {
    /// Server state directory (defaults to $ICARUS_HOME or ~/.icarus).
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    /// Talk to a saved server (see `icarus remote list`) instead of this machine's.
    #[arg(long, global = true, value_name = "NAME")]
    server: Option<String>,
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
    /// Servers on other machines this CLI can talk to with `--server <name>`.
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Per-workspace environments from orca.yaml's `environmentRecipes` (create one with
    /// `icarus workspace create --recipe <id>`).
    Recipe {
        #[command(subcommand)]
        command: recipe::RecipeCommand,
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
    /// Import colour themes (VS Code, Warp) for the app.
    Theme {
        #[command(subcommand)]
        command: theme::ThemeCommand,
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
    /// Review a pull request: draft comments, diff base, summary, submit, threads, replies, reactions, media.
    Review {
        #[command(flatten)]
        at: review::At,
        #[command(subcommand)]
        command: review::ReviewCommand,
    },
    /// Linear issues linked to workspaces: show, open, refresh.
    Issue {
        #[command(subcommand)]
        command: issue::IssueCommand,
    },
    /// Connect Linear: save, remove or check the API key the server uses.
    Linear {
        #[command(subcommand)]
        command: linear::LinearCommand,
    },
    /// Notes on lines of a workspace's files: how agents point the user at code in the diff, with
    /// threads answered by throwaway agent sessions.
    #[command(alias = "note")]
    Notes {
        /// Workspace id, name or path; defaults to $ICARUS_WORKSPACE_ID, then the current directory's.
        #[arg(long, global = true)]
        workspace: Option<String>,
        #[command(subcommand)]
        command: notes::NotesCommand,
    },
    /// Source Control actions in a workspace's worktree: discard, stage, commit, push, pull, sync.
    Git {
        /// Workspace id, name or path; defaults to $ICARUS_WORKSPACE_ID, then the current directory's.
        #[arg(long, global = true)]
        workspace: Option<String>,
        #[command(subcommand)]
        command: git_ops::GitCommand,
    },
    /// Notifications from the server (agents, CI, reviews, merges, rules); newest first.
    Notifications {
        /// Only unread ones.
        #[arg(long)]
        unread: bool,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[command(subcommand)]
        command: Option<extras::NotificationsCommand>,
    },
    /// Agent accounts: several logins per provider, each in its own config dir.
    Account {
        #[command(subcommand)]
        command: extras::AccountCommand,
    },
    /// Check for and install new Icarus releases.
    Update {
        #[command(subcommand)]
        command: update::UpdateCommand,
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
        /// Directory for the images (default: a new temporary directory).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Seconds to wait for the app to finish writing.
        #[arg(long, default_value_t = 15)]
        timeout: u64,
    },
    /// Run an app action (any bindable action, e.g. `icarus::NewAgent`) in the app's frontmost window.
    Dispatch {
        action: String,
        /// The action's arguments as JSON, for actions that take some.
        #[arg(long)]
        args: Option<String>,
    },
    /// The front window's visible elements: debug selectors (ids such as `card:<workspace>`,
    /// `scm-row:<path>`, `diff-tab:<path>`, `composer`, `agent-tab:<id>`), text, and bounds.
    ///
    /// Selectors for click and wait: an id (`card:ws-1`), an id prefix ending in `*`
    /// (`scm-row:*`), text (`text:"Changes"`, exact, else the first containing it), or a point
    /// (`120,48`). Input is simulated inside the app, so it needs no OS permissions and works
    /// over SSH. Example flow:
    ///   icarus app click 'scm-row:src/main.rs' --wait 'diff-tab:src/main.rs'
    ///   icarus app key alt-f7 --repeat 20 --interval 30 --wait 'diff-tab:*'
    Tree,
    /// Clicks an element (see `tree` for selectors), optionally waiting for another to show.
    Click {
        target: String,
        /// After clicking, wait for this selector and report how long it took.
        #[arg(long)]
        wait: Option<String>,
        /// With --wait: wait for it to disappear instead.
        #[arg(long)]
        gone: bool,
        /// Milliseconds to wait.
        #[arg(long, default_value_t = 10_000)]
        timeout: u64,
    },
    /// Types text into the focused element, as typing on the keyboard would.
    Type { text: String },
    /// Presses keystrokes (`cmd-k`, `alt-f7`, `escape`; several separated by spaces).
    Key {
        keys: String,
        /// Press it this many times, like a held key's repeats.
        #[arg(long, default_value_t = 1)]
        repeat: u32,
        /// Milliseconds between repeats.
        #[arg(long, default_value_t = 30)]
        interval: u64,
        /// After pressing, wait for this selector and report how long it took.
        #[arg(long)]
        wait: Option<String>,
        /// With --wait: wait for it to disappear instead.
        #[arg(long)]
        gone: bool,
        /// Milliseconds to wait.
        #[arg(long, default_value_t = 10_000)]
        timeout: u64,
    },
    /// Waits until an element shows (or with --gone, disappears) and prints how long it took.
    Wait {
        target: String,
        #[arg(long)]
        gone: bool,
        /// Milliseconds to wait before failing.
        #[arg(long, default_value_t = 10_000)]
        timeout: u64,
    },
    /// The app's own settings (its `client.json`): view state kept per app, such as card fields,
    /// folded groups or the changes view. A key the server also has is overridden for this app only.
    Config {
        #[command(subcommand)]
        command: AppConfigCommand,
    },
}

#[derive(Subcommand)]
enum AppConfigCommand {
    /// Print the app's own value of a dotted key (or all of them).
    Get { key: Option<String> },
    /// Set a dotted key for this app; the value is parsed as JSON when it can be.
    Set { key: String, value: String },
    /// Go back to the server's value.
    Unset { key: String },
}

/// Waits for the app to write its answer to `path`, then removes the file.
async fn app_answer(path: &std::path::Path) -> Result<Value> {
    app_answer_within(path, Duration::from_secs(10)).await
}

async fn app_answer_within(path: &std::path::Path, within: Duration) -> Result<Value> {
    let deadline = std::time::Instant::now() + within;
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(value) = serde_json::from_str::<Value>(&text)
        {
            std::fs::remove_file(path)?;
            if let Some(error) = value["error"].as_str() {
                anyhow::bail!("{error}");
            }
            return Ok(value);
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("the app did not answer within {}s", within.as_secs());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The `client.drive` request for an app driver command, and how long to wait for the app's answer.
fn drive_request(command: &AppCommand) -> (Value, Duration) {
    let wait = |target: &Option<String>, gone: bool, timeout: u64| {
        target
            .as_ref()
            .map(|target| json!({ "target": target, "gone": gone, "timeout_ms": timeout }))
    };
    let slack = Duration::from_secs(10);
    match command {
        AppCommand::Click {
            target,
            wait: until,
            gone,
            timeout,
        } => (
            json!({ "op": "click", "target": target, "wait": wait(until, *gone, *timeout) }),
            Duration::from_millis(*timeout) + slack,
        ),
        AppCommand::Type { text } => (json!({ "op": "type", "text": text }), slack),
        AppCommand::Key {
            keys,
            repeat,
            interval,
            wait: until,
            gone,
            timeout,
        } => (
            json!({
                "op": "key",
                "keys": keys,
                "repeat": repeat,
                "interval_ms": interval,
                "wait": wait(until, *gone, *timeout),
            }),
            Duration::from_millis(*timeout + u64::from(*repeat) * *interval) + slack,
        ),
        AppCommand::Wait {
            target,
            gone,
            timeout,
        } => (
            json!({ "op": "wait", "wait": wait(&Some(target.clone()), *gone, *timeout) }),
            Duration::from_millis(*timeout) + slack,
        ),
        _ => (json!({ "op": "tree" }), slack),
    }
}

fn drive_summary(value: &Value) -> String {
    fn walk(node: &Value, depth: usize, out: &mut Vec<String>) {
        let bounds = node["bounds"]
            .as_array()
            .map(|b| {
                let n = |i: usize| b.get(i).and_then(Value::as_f64).unwrap_or_default();
                format!("{:.0},{:.0} {:.0}x{:.0}", n(0), n(1), n(2), n(3))
            })
            .unwrap_or_default();
        let label = match (node["id"].as_str(), node["text"].as_str()) {
            (Some(id), _) => id.to_owned(),
            (None, Some(text)) => format!("{:?}", text.replace('\n', " ⏎ ")),
            _ => String::new(),
        };
        out.push(format!("{}{label}  [{bounds}]", "  ".repeat(depth)));
        for child in node["children"].as_array().into_iter().flatten() {
            walk(child, depth + 1, out);
        }
    }
    let mut lines = Vec::new();
    if let Some(nodes) = value["nodes"].as_array() {
        for node in nodes {
            walk(node, 0, &mut lines);
        }
    }
    if let Some(clicked) = value["clicked"].as_object() {
        lines.push(format!(
            "clicked {} at {:.0},{:.0}",
            clicked["target"].as_str().unwrap_or_default(),
            clicked["x"].as_f64().unwrap_or_default(),
            clicked["y"].as_f64().unwrap_or_default()
        ));
    }
    if let Some(pressed) = value["pressed"].as_str() {
        lines.push(format!("pressed {pressed}"));
    }
    if let Some(typed) = value["typed"].as_str() {
        lines.push(format!("typed {typed:?}"));
    }
    if let Some(waited) = value["waited"].as_object() {
        lines.push(format!(
            "{} {} after {} ms",
            waited["target"].as_str().unwrap_or_default(),
            if waited["gone"].as_bool() == Some(true) {
                "was gone"
            } else {
                "showed"
            },
            waited["elapsed_ms"].as_u64().unwrap_or_default()
        ));
    }
    lines.join("\n")
}

fn answer_path(kind: &str) -> Result<PathBuf> {
    Ok(std::env::temp_dir().join(format!(
        "icarus-{kind}-{}.json",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    )))
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
    /// Bring Orca's per-workspace state over: statuses, pinned names, linked Linear issues, diff
    /// notes and agent notes, and unsent PR review comments (queued as drafts on the workspace's
    /// pull request), matched to Icarus workspaces by path or by repository and name.
    /// Reads Orca's files read-only; workspace values you already set win.
    ImportOrcaState {
        /// Orca's `profile-state.db` (default: the one in Orca's profile folder on this machine).
        #[arg(long, value_name = "FILE")]
        from: Option<PathBuf>,
        /// Orca's `pending-review-drafts.json` (default: the one in Orca's data folder, if any).
        #[arg(long, value_name = "FILE")]
        drafts: Option<PathBuf>,
        /// Report what would change; write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Import an `orca settings export` file into your user settings and matching projects. Its
    /// appearance and language tooling are merged into the app's Zed settings (keys you already
    /// set win) and its colours become the Zed theme `themes/icarus-imported.json`.
    Import {
        file: PathBuf,
        /// Report what would change; write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Print the Zed settings the export translates to.
        #[arg(long)]
        print_zed: bool,
        /// Leave the Zed settings and themes alone.
        #[arg(long)]
        skip_zed: bool,
        /// Zed config directory to merge into (default: the app's, `~/.config/icarus`).
        #[arg(long)]
        zed_dir: Option<PathBuf>,
    },
    /// Translate Orca keybindings (`~/.orca/keybindings.json`, or an `orca settings export`) into
    /// the app's keymap. Like Orca, a remap replaces that action's default key; actions the file
    /// doesn't mention keep their defaults. Importing again replaces what the last import wrote.
    /// Runs on this machine, where the app lives, even with `--server`.
    ImportKeybindings {
        file: PathBuf,
        /// The platform whose bindings to use (default: this machine's).
        #[arg(long, value_enum)]
        platform: Option<KeymapPlatform>,
        /// The app's Zed config directory to write (default: `~/.config/icarus`).
        #[arg(long, value_name = "DIR")]
        zed_dir: Option<PathBuf>,
        /// Report what would change; write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Bring a normal Zed install's settings, keymap, snippets, themes and extensions into the app.
    /// Settings the app already has win; extensions are downloaded by the app on its next start.
    /// Runs on this machine, where the app and Zed live, even with `--server`.
    ImportZed {
        /// Zed config directory to read (default: `~/.config/zed`).
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// Zed data directory holding `extensions/` (default: Zed's, e.g.
        /// `~/Library/Application Support/Zed`).
        #[arg(long, value_name = "DIR")]
        from_data: Option<PathBuf>,
        /// The app's Zed config directory to write (default: `~/.config/icarus`).
        #[arg(long, value_name = "DIR")]
        zed_dir: Option<PathBuf>,
        /// Report what would change; write nothing.
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
    /// Show the scripts the repository's committed files (orca.yaml, icarus.json) run, and whether you trust them.
    Scripts {
        id: String,
        /// Read orca.yaml from this workspace's checkout instead of the project root.
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Trust the project's committed scripts as they are now; a later change asks again.
    Trust {
        id: String,
        /// Also trust the scripts in this workspace's checkout.
        #[arg(long)]
        workspace: Option<String>,
        /// Always trust this repository's scripts, including later changes (Orca's "Always trust").
        #[arg(long)]
        always: bool,
    },
    /// Forget every trusted version of the project's committed scripts, and "always trust".
    Untrust {
        id: String,
    },
}

// Parsed once per process, so the size of `Create` costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum WorkspaceCommand {
    /// Create a worktree workspace; it starts with an agent, optionally with a prompt.
    Create {
        #[arg(long)]
        project: String,
        /// Defaults to the Linear issue's identifier, lowercased (e.g. abc-12), with --from-linear,
        /// and to the pull request's head branch with --from-pr.
        #[arg(required_unless_present_any = ["from_linear", "from_pr"])]
        name: Option<String>,
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
        /// Link a Linear issue by identifier, e.g. ABC-12; it names the workspace, and Linear's branch name is the branch.
        #[arg(long)]
        from_linear: Option<String>,
        /// Start from a pull request (number or URL): its head branch, on its base.
        #[arg(long, value_name = "NUMBER|URL", conflicts_with = "base")]
        from_pr: Option<String>,
        #[arg(long)]
        no_agent: bool,
        /// Open the workspace with this tab layout (see `icarus layout list`).
        #[arg(long)]
        layout: Option<String>,
        /// The PR URL for `{pr_url}` in the layout, when the branch has no PR yet.
        #[arg(long)]
        pr_url: Option<String>,
        /// Account (id or label) for the agent; `system` for the provider's own login.
        #[arg(long)]
        account: Option<String>,
        /// Start the agent with Claude Code's Remote Control; defaults to `agents.remote_control`.
        #[arg(long, value_name = "on|off")]
        remote_control: Option<Switch>,
        /// Run the project's setup script even if its policy skips it (or asks).
        #[arg(long, conflicts_with = "no_setup")]
        setup: bool,
        /// Don't run the project's setup script.
        #[arg(long)]
        no_setup: bool,
        /// Trust the project's committed scripts (see `icarus project scripts`) and run them.
        #[arg(long)]
        trust: bool,
        /// Always trust the project's committed scripts, including later changes.
        #[arg(long)]
        always_trust: bool,
        /// Link a GitHub issue by URL; the project's `issueCommand` runs for it.
        #[arg(long, value_name = "URL")]
        from_issue: Option<String>,
        /// Don't run the project's `issueCommand` for the linked pull request or issue.
        #[arg(long)]
        no_issue_command: bool,
        /// Create it in a new environment from one of orca.yaml's `environmentRecipes`.
        #[arg(long, value_name = "ID")]
        recipe: Option<String>,
    },
    /// Register an existing linked worktree as a workspace, leaving it where it is.
    Adopt {
        path: PathBuf,
        /// Defaults to the project registered for the worktree's repository or its remote.
        #[arg(long)]
        project: Option<String>,
        /// Defaults to the worktree's folder name.
        #[arg(long)]
        name: Option<String>,
    },
    /// List workspaces, with the worktrees found by discovery; origin is shown unless native.
    List {
        #[arg(long)]
        project: Option<String>,
        /// Include hidden workspaces, marked `hidden`.
        #[arg(long)]
        all: bool,
    },
    /// Hide a workspace from the sidebar; its worktree stays where it is.
    Hide {
        id: String,
    },
    /// Show a hidden workspace again (`--follow-settings` drops the choice instead).
    Unhide {
        id: String,
        /// Forget the explicit show/hide and follow the visibility settings again.
        #[arg(long)]
        follow_settings: bool,
    },
    /// Newly found worktrees waiting for a show/hide decision.
    Inbox {
        #[arg(long)]
        project: Option<String>,
    },
    /// Show every worktree from one source in a project: external, claude, gsd or custom:<id>.
    ShowSource {
        #[arg(long)]
        project: String,
        source: String,
    },
    /// Run the project's setup script again, in a new Setup terminal.
    Setup {
        id: String,
        /// Trust the committed setup script in this workspace's checkout first.
        #[arg(long)]
        trust: bool,
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
    /// Change a workspace's status, title or base, or pin it to the top of its sidebar group.
    Set {
        id: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long, conflicts_with = "unpin")]
        pin: bool,
        #[arg(long)]
        unpin: bool,
    },
    /// Put workspaces at the top of the sidebar's Manual sort, in the order given.
    Reorder {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Clear the unread mark an agent left (focusing the workspace in the app does the same).
    MarkRead {
        id: String,
    },
    /// Mark the workspace unread, as a finished agent turn does.
    MarkUnread {
        id: String,
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
    /// Remove the workspace. A worktree Icarus created is deleted (archive script first, a failure
    /// keeps it); any other worktree is only hidden unless --delete-worktree.
    Remove {
        id: String,
        /// Remove even with local changes or a failed archive script.
        #[arg(long)]
        force: bool,
        #[arg(long, conflicts_with = "delete_branch")]
        keep_branch: bool,
        /// Delete a worktree Icarus didn't create (discovered or adopted) instead of hiding it.
        #[arg(long)]
        delete_worktree: bool,
        /// Also delete the branch of a worktree Icarus didn't create (its branch is kept by default).
        #[arg(long)]
        delete_branch: bool,
        /// Don't run the project's archive script.
        #[arg(long)]
        skip_archive: bool,
        /// Remove even if the archive script fails, but keep the dirty-checkout check.
        #[arg(long)]
        allow_failed_archive: bool,
    },
    /// Show the workspace's next step (the next-action button); --run runs it.
    NextAction {
        id: String,
        #[arg(long)]
        run: bool,
    },
    /// Park the workspace: its agents sleep and its terminals' processes stop, keeping their scrollback.
    Sleep {
        id: String,
    },
    /// Restart what `sleep` parked.
    Wake {
        id: String,
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
        /// Permission mode to start in (see `icarus agent mode`); wins over --ask-permissions.
        #[arg(long)]
        mode: Option<String>,
        /// Resume an earlier provider session.
        #[arg(long)]
        resume: Option<String>,
        /// Account (id or label); `system` for the provider's own login.
        #[arg(long)]
        account: Option<String>,
        /// Start with Claude Code's Remote Control; defaults to `agents.remote_control`.
        #[arg(long, value_name = "on|off")]
        remote_control: Option<Switch>,
    },
    /// Switch Claude Code's own Remote Control for an agent (or every agent of --workspace), so the
    /// session can be followed on claude.ai or the Claude app; prints the session's claude.ai link.
    RemoteControl {
        state: Switch,
        #[arg(required_unless_present = "workspace")]
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        workspace: Option<String>,
    },
    /// Show the agent's permission mode and the modes it offers, or switch to MODE; the agent keeps
    /// it across restarts.
    Mode { id: String, mode: Option<String> },
    /// Show the agent's model and the models it offers, or switch to MODEL for the following turns.
    Model { id: String, model: Option<String> },
    /// List the slash commands the agent offers.
    Commands { id: String },
    /// List agent sessions.
    List,
    /// Past sessions (sleeping, exited or killed), newest first; --search looks through prompts and answers.
    History {
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        search: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Only sessions last active before this time (ms since the epoch), for paging.
        #[arg(long)]
        before: Option<u64>,
    },
    /// Bring a past session back as a live agent in its workspace.
    Resume { id: String },
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
    Start {
        /// Also accept paired clients on this ip:port, e.g. your Tailscale address (repeatable).
        #[arg(long, value_name = "ADDR")]
        listen: Vec<String>,
        /// Allow --listen on addresses outside loopback, private and Tailscale ranges.
        #[arg(long)]
        allow_public: bool,
    },
    /// Stop the server and every session it owns.
    Stop,
    /// Restart the server (e.g. onto a new binary) without stopping anything: terminals keep
    /// their processes and output, and agents sleep and resume.
    Restart,
    /// Show whether the server is running.
    Status,
    /// Start the server at login (launchd on macOS, a systemd user unit on Linux).
    Install,
    /// Stop starting the server at login.
    Uninstall,
    /// Print a one-time code another machine uses to pair (`icarus remote add … --code`).
    Pair {
        /// Seconds the code stays valid (at most an hour).
        #[arg(long, default_value_t = 600)]
        ttl: u64,
    },
    /// List the clients paired over the network.
    Clients,
    /// Revoke a paired client's token and close its connections.
    Revoke { id: String },
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// Save a server: pair with a `--listen` address using a code from `icarus server pair`,
    /// or save `ssh://host` to reach it over SSH.
    ///
    /// Examples:
    ///   icarus remote add desk 100.101.102.103:7878 --code ABCD-EFGH
    ///   icarus remote add devbox ssh://devbox
    Add {
        name: String,
        address: String,
        /// The one-time code printed by `icarus server pair` on the server's machine.
        #[arg(long)]
        code: Option<String>,
        /// Require this certificate fingerprint (as printed by `icarus server pair`).
        #[arg(long)]
        fingerprint: Option<String>,
        /// How the server lists this machine; defaults to the host name.
        #[arg(long)]
        label: Option<String>,
        /// For SSH servers: the remote command instead of `icarus-server proxy`.
        #[arg(long)]
        command: Option<String>,
        /// For a paired address: an SSH destination for the same machine, so the app can open
        /// its worktrees (e.g. `--ssh devbox`).
        #[arg(long)]
        ssh: Option<String>,
    },
    /// List saved servers.
    List,
    /// Forget a saved server. The server keeps the pairing until `icarus server revoke`.
    Remove { name: String },
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
    /// Set the title its tabs and panes show (Orca's pane title); omit it to clear.
    Rename { id: String, label: Option<String> },
    /// Kill a terminal's process.
    Kill {
        id: String,
        /// Also forget the session.
        #[arg(long)]
        remove: bool,
    },
    /// Show a server terminal in this one and type into it; it keeps running when this exits.
    Attach {
        /// The terminal to show (omit with --create).
        #[arg(required_unless_present = "create")]
        id: Option<String>,
        /// Start a new terminal and attach to it.
        #[arg(long, conflicts_with = "id")]
        create: bool,
        #[arg(long, requires = "create")]
        cwd: Option<PathBuf>,
        #[arg(long, requires = "create")]
        workspace: Option<String>,
        #[arg(long, requires = "create")]
        label: Option<String>,
        /// Command and arguments to run, after `--` (with --create).
        #[arg(last = true)]
        command: Vec<String>,
    },
}

fn home(cli: &Cli) -> Result<PathBuf> {
    match &cli.home {
        Some(home) => Ok(home.clone()),
        None => icarus_client::default_home(),
    }
}

type Connection = (
    std::sync::Arc<Client>,
    tokio::sync::mpsc::UnboundedReceiver<icarus_client::Event>,
);

/// Connects to the `--server` saved server, or else the local one.
async fn open(cli: &Cli, client_name: &str) -> Result<Connection> {
    match &cli.server {
        Some(name) => open_target(Some(name), None, client_name).await,
        None => open_target(None, Some(&home(cli)?), client_name).await,
    }
}

/// Connects to a saved server by name, else to the local server in `home`.
async fn open_target(
    server: Option<&str>,
    home: Option<&std::path::Path>,
    client_name: &str,
) -> Result<Connection> {
    match (server, home) {
        (Some(name), _) => {
            let saved = icarus_client::servers::SavedServers::load()?;
            let server = saved.get(name).with_context(|| {
                format!("no saved server named {name} (see `icarus remote list`)")
            })?;
            Client::connect_saved(server, client_name).await
        }
        (None, Some(home)) => Client::connect(home, client_name)
            .await
            .context("no Icarus server is running (start one with `icarus server start`)"),
        (None, None) => anyhow::bail!("no home directory"),
    }
}

async fn connect(cli: &Cli) -> Result<std::sync::Arc<Client>> {
    Ok(open(cli, "icarus-cli").await?.0)
}

fn host_name() -> String {
    Process::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "client".into())
}

async fn remote_command(cli: &Cli, command: &RemoteCommand) -> Result<()> {
    use icarus_client::servers::{SavedServer, SavedServers, Target, parse_address};
    match command {
        RemoteCommand::Add {
            name,
            address,
            code,
            fingerprint,
            label,
            command,
            ssh,
        } => {
            let mut saved = SavedServers::load()?;
            let server = match parse_address(address)? {
                Target::Ssh(_) => SavedServer {
                    name: name.clone(),
                    address: address.clone(),
                    token: None,
                    fingerprint: None,
                    client_id: None,
                    command: command.clone(),
                    ssh: None,
                },
                Target::Tcp(addr) => {
                    let code = code.as_deref().context(
                        "pairing needs --code; run `icarus server pair` on the server's machine",
                    )?;
                    recipe::pair(
                        name,
                        &addr,
                        code,
                        fingerprint.clone(),
                        label.clone(),
                        ssh.clone(),
                    )
                    .await?
                }
            };
            saved.upsert(server.clone());
            saved.save()?;
            let value = json!({
                "name": server.name,
                "address": server.address,
                "client_id": server.client_id,
                "fingerprint": server.fingerprint,
            });
            print(cli, &value, |_| match &server.fingerprint {
                Some(fp) => format!(
                    "paired with {} as client {}; certificate sha256 {fp}",
                    server.address,
                    server.client_id.as_deref().unwrap_or("?")
                ),
                None => format!("saved {} ({})", server.name, server.address),
            });
        }
        RemoteCommand::List => {
            let saved = SavedServers::load()?;
            let value = json!(
                saved
                    .servers
                    .iter()
                    .map(|s| json!({
                        "name": s.name,
                        "address": s.address,
                        "paired": s.token.is_some(),
                        "client_id": s.client_id,
                        "fingerprint": s.fingerprint,
                        "ssh": s.ssh_destination(),
                    }))
                    .collect::<Vec<_>>()
            );
            print(cli, &value, |_| {
                saved
                    .servers
                    .iter()
                    .map(|s| match &s.ssh {
                        Some(ssh) => format!("{}\t{}\tssh {ssh}", s.name, s.address),
                        None => format!("{}\t{}", s.name, s.address),
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        RemoteCommand::Remove { name } => {
            let mut saved = SavedServers::load()?;
            if saved.remove(name).is_none() {
                bail!("no saved server named {name}");
            }
            saved.save()?;
            print(cli, &json!({ "removed": name }), |_| {
                format!("removed {name}")
            });
        }
    }
    Ok(())
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

#[derive(Clone, Copy, clap::ValueEnum)]
enum Switch {
    On,
    Off,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum KeymapPlatform {
    Mac,
    Linux,
}

impl Switch {
    fn enabled(self) -> bool {
        matches!(self, Switch::On)
    }
}

/// `agent.modes` / `agent.models`: one choice per line, the current one marked.
fn choices_summary(v: &Value, key: &str) -> String {
    let current = v["current"].as_str().unwrap_or_default();
    v[key]
        .as_array()
        .into_iter()
        .flatten()
        .map(|choice| {
            let id = choice["id"].as_str().unwrap_or_default();
            let mark = if id == current { "*" } else { " " };
            format!(
                "{mark} {id}  {}",
                choice["name"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn remote_control_summary(v: &Value) -> String {
    let agents = v["agents"].as_array().cloned().unwrap_or_default();
    if agents.is_empty() {
        return "no Claude Code agents".into();
    }
    agents
        .iter()
        .map(|a| {
            let state = if a["remote_control"] == json!(true) {
                "on"
            } else {
                "off"
            };
            let when = if a["restarted"] == json!(true) {
                "restarted on its session"
            } else if a["state"] == "working" {
                "applies when its process next starts"
            } else {
                "applies on its next start"
            };
            let mut line = format!(
                "{}  remote control {state} ({when})",
                a["id"].as_str().unwrap_or_default()
            );
            if let Some(url) = a["remote_control_url"].as_str() {
                line.push_str(&format!("\n    {url}"));
            } else if a["remote_control"] == json!(true) {
                line.push_str("\n    `icarus agent list` shows the claude.ai link once Claude Code registers the session");
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn scripts_summary(value: &Value) -> String {
    let mut lines = Vec::new();
    let mut script = |label: &str, entry: &Value| {
        if let Some(command) = entry["command"].as_str() {
            let trust = if entry["trusted"] == true {
                "trusted"
            } else {
                "NOT TRUSTED"
            };
            lines.push(format!(
                "{label} ({}, {trust}):",
                entry["source"].as_str().unwrap_or("?")
            ));
            lines.extend(command.lines().map(|line| format!("  {line}")));
        }
    };
    script("setup", &value["setup"]);
    script("archive", &value["archive"]);
    script("issue command", &value["issue_command"]);
    if let Some(tabs) = value["default_tabs"].as_array().filter(|t| !t.is_empty()) {
        let trust = if value["default_tabs_trusted"] == true {
            "trusted"
        } else {
            "NOT TRUSTED"
        };
        lines.push(format!("default tabs ({trust}):"));
        for tab in tabs {
            let color = tab["color"]
                .as_str()
                .map(|color| format!(" [{color}]"))
                .unwrap_or_default();
            lines.push(format!(
                "  {}{color}: {}",
                tab["title"].as_str().unwrap_or("Terminal"),
                tab["command"].as_str().unwrap_or("(shell)")
            ));
        }
    }
    if let Some(recipes) = value["recipes"].as_array().filter(|r| !r.is_empty()) {
        lines.push(format!(
            "environment recipes ({}):",
            if value["untrusted"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "vm_recipe"))
            {
                "NOT TRUSTED"
            } else {
                "trusted"
            }
        ));
        for recipe in recipes {
            lines.push(format!(
                "  {}: {} ({})",
                recipe["id"].as_str().unwrap_or("?"),
                recipe["name"].as_str().unwrap_or(""),
                recipe["create"].as_str().unwrap_or("")
            ));
        }
    }
    for problem in value["recipe_problems"].as_array().into_iter().flatten() {
        lines.push(format!(
            "  recipe {}: {}",
            problem["index"],
            problem["message"].as_str().unwrap_or("")
        ));
    }
    if lines.is_empty() {
        lines.push("no scripts".into());
    }
    lines.push(format!(
        "setup policy: {}",
        value["run_policy"].as_str().unwrap_or("?")
    ));
    if value["always_trusted"] == true {
        lines.push(format!(
            "always trusted (undo with `icarus project untrust {}`)",
            value["project"].as_str().unwrap_or("<id>")
        ));
    } else if value["trusted"] != true {
        lines.push(format!(
            "trust them with `icarus project trust {}`",
            value["project"].as_str().unwrap_or("<id>")
        ));
    }
    lines.join("\n")
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
        Command::Terminal {
            command:
                TerminalCommand::Attach {
                    id,
                    create,
                    cwd,
                    workspace,
                    label,
                    command,
                },
        } => {
            let spec = create
                .then(|| -> Result<term_attach::CreateSpec> {
                    Ok(term_attach::CreateSpec {
                        cwd: match cwd {
                            Some(cwd) => cwd.clone(),
                            None => std::env::current_dir()?,
                        },
                        workspace: workspace.clone(),
                        label: label.clone(),
                        command: command.clone(),
                    })
                })
                .transpose()?;
            let code = term_attach::run(&cli, id.clone(), spec).await?;
            std::process::exit(code);
        }
        Command::Server {
            command:
                ServerCommand::Start { .. }
                | ServerCommand::Restart
                | ServerCommand::Install
                | ServerCommand::Uninstall,
        } if cli.server.is_some() => {
            bail!(
                "--server can't start, restart, install or uninstall a server; do that on its machine"
            );
        }
        Command::Server { command } => match command {
            ServerCommand::Start {
                listen,
                allow_public,
            } => {
                if Client::connect(&home, "icarus-cli").await.is_ok() {
                    print(&cli, &json!({ "running": true, "started": false }), |_| {
                        "server already running".into()
                    });
                    return Ok(());
                }
                std::fs::create_dir_all(&home)?;
                let log = std::fs::File::create(home.join("server.log"))?;
                let mut args = vec![
                    "--home".to_string(),
                    home.to_string_lossy().into_owned(),
                    "serve".to_string(),
                ];
                for addr in listen {
                    args.extend(["--listen".to_string(), addr.clone()]);
                }
                if *allow_public {
                    args.push("--allow-public".into());
                }
                use std::os::unix::process::CommandExt;
                let mut child = Process::new(server_binary()?)
                    .args(&args)
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
                    if child.try_wait()?.is_some() || Instant::now() > deadline {
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
                let client = connect(&cli).await?;
                let value = client.request("server.shutdown", json!({})).await?;
                print(&cli, &value, |_| "server stopping".into());
            }
            ServerCommand::Restart => {
                let client = connect(&cli).await?;
                let before = client.request("server.info", json!({})).await?;
                let exe = std::env::current_exe()?;
                let how = update::restart_server(&cli, &client, &exe, &server_binary()?).await?;
                let after = connect(&cli)
                    .await?
                    .request("server.info", json!({}))
                    .await?;
                let value = json!({
                    "restarted": true,
                    "how": how,
                    "old_pid": before["pid"],
                    "pid": after["pid"],
                    "terminals": after["terminals"],
                });
                print(&cli, &value, |v| {
                    format!(
                        "{how} · pid {} · {} terminal(s) kept",
                        v["pid"], v["terminals"]
                    )
                });
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
            ServerCommand::Pair { ttl } => {
                let client = connect(&cli).await?;
                let value = client
                    .request("server.pair", json!({ "ttl_secs": ttl }))
                    .await?;
                print(&cli, &value, |v| {
                    let code = v["code"].as_str().unwrap_or("?");
                    let minutes = v["expires_in_secs"].as_u64().unwrap_or(0).div_ceil(60);
                    let listen: Vec<&str> = v["listen"]
                        .as_array()
                        .map(|a| a.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    let mut text =
                        format!("Pairing code: {code} (single use, valid {minutes} minute(s))\n");
                    match listen.first() {
                        Some(addr) => {
                            text += &format!(
                                "On the other machine: icarus remote add <name> {addr} --code {code}\n"
                            );
                            if let Some(fp) = v["tls_fingerprint"].as_str() {
                                text += &format!("Certificate sha256: {fp}");
                            }
                        }
                        None => {
                            text += "The server isn't listening on the network yet: restart it with \
                             `icarus server start --listen <tailscale-ip>:<port>` or set `server.listen`."
                        }
                    }
                    text.trim_end().to_string()
                });
            }
            ServerCommand::Clients => {
                let client = connect(&cli).await?;
                let value = client.request("server.clients", json!({})).await?;
                print(&cli, &value, |v| {
                    v.as_array()
                        .map(|clients| {
                            clients
                                .iter()
                                .map(|c| {
                                    format!(
                                        "{}\t{}\tlast seen {}\t{} connection(s)",
                                        c["id"].as_str().unwrap_or("?"),
                                        c["label"].as_str().unwrap_or("?"),
                                        c["last_seen_ms"],
                                        c["connections"]
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default()
                });
            }
            ServerCommand::Revoke { id } => {
                let client = connect(&cli).await?;
                let value = client.request("server.revoke", json!({ "id": id })).await?;
                print(&cli, &value, |v| {
                    format!(
                        "revoked {id}; closed {} connection(s)",
                        v["closed_connections"]
                    )
                });
            }
            ServerCommand::Status => match open(&cli, "icarus-cli").await {
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
            let server = cli.server.clone();
            let home = Some(home.clone());
            let reconnect = move || {
                let server = server.clone();
                let home = home.clone();
                async move { open_target(server.as_deref(), home.as_deref(), "icarus-acp").await }
            };
            // A tab opened while the server restarts waits for it instead of failing to launch.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            let connection = loop {
                match reconnect().await {
                    Ok(connection) => break connection,
                    Err(_) if std::time::Instant::now() < deadline => {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Err(error) => return Err(error),
                }
            };
            acp::run(connection, provider.clone(), reconnect).await?;
        }
        Command::Remote { command } => remote_command(&cli, command).await?,
        Command::Recipe { command } => recipe::run(&cli, command).await?,
        Command::File { command } => {
            let client = connect(&cli).await?;
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
            let client = connect(&cli).await?;
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
            let client = connect(&cli).await?;
            match command {
                AppCommand::Dispatch { action, args } => {
                    let args: Option<Value> = args
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()
                        .map_err(|e| anyhow::anyhow!("--args is not JSON: {e}"))?;
                    let result = answer_path("dispatch")?;
                    client
                        .request(
                            "client.dispatch",
                            json!({ "action": action, "args": args, "result": result }),
                        )
                        .await?;
                    let value = app_answer(&result).await?;
                    print(&cli, &value, |v| {
                        format!("dispatched {}", v["action"].as_str().unwrap_or_default())
                    });
                }
                AppCommand::Tree
                | AppCommand::Click { .. }
                | AppCommand::Type { .. }
                | AppCommand::Key { .. }
                | AppCommand::Wait { .. } => {
                    let (request, within) = drive_request(command);
                    let result = answer_path("drive")?;
                    let mut request = request;
                    request["result"] = json!(result);
                    client.request("client.drive", request).await?;
                    let value = app_answer_within(&result, within).await?;
                    print(&cli, &value, drive_summary);
                }
                AppCommand::Config { command } => {
                    let (op, key, value) = match command {
                        AppConfigCommand::Get { key } => ("get", key.clone(), None),
                        AppConfigCommand::Set { key, value } => (
                            "set",
                            Some(key.clone()),
                            Some(
                                serde_json::from_str::<Value>(value)
                                    .unwrap_or_else(|_| Value::String(value.clone())),
                            ),
                        ),
                        AppConfigCommand::Unset { key } => ("unset", Some(key.clone()), None),
                    };
                    let result = answer_path("app-config")?;
                    client
                        .request(
                            "client.config",
                            json!({ "op": op, "key": key, "value": value, "result": result }),
                        )
                        .await?;
                    let answer = app_answer(&result).await?;
                    print(&cli, &answer["value"], |v| match v {
                        Value::Null => "(not set; the server's value applies)".to_owned(),
                        Value::String(text) => text.clone(),
                        other => serde_json::to_string_pretty(other).unwrap_or_default(),
                    });
                }
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
            let client = connect(&cli).await?;
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
        Command::Theme { command } => {
            let value = theme::run(command)?;
            print(&cli, &value, theme::summary);
        }
        Command::Settings {
            command:
                SettingsCommand::ImportZed {
                    from,
                    from_data,
                    zed_dir,
                    dry_run,
                },
        } => {
            let source = from
                .clone()
                .or_else(zed_import::source_config_dir)
                .context("no Zed config directory; pass --from")?;
            let source_data = from_data.clone().or_else(zed_import::source_data_dir);
            let target = zed_dir
                .clone()
                .or_else(icarus_settings::zed::config_dir)
                .context("no app config directory; pass --zed-dir")?;
            let result = zed_import::import(&source, source_data.as_deref(), &target, *dry_run)?;
            print(&cli, &serde_json::to_value(&result)?, |_| {
                result.summary_lines().join("\n")
            });
        }
        Command::Settings {
            command:
                SettingsCommand::ImportKeybindings {
                    file,
                    platform,
                    zed_dir,
                    dry_run,
                },
        } => {
            use icarus_settings::keymap;
            let text = std::fs::read_to_string(file)
                .with_context(|| format!("reading {}", file.display()))?;
            let mut orca: Value = serde_json::from_str(&text)
                .with_context(|| format!("{} is not JSON", file.display()))?;
            // An `orca settings export` carries the keybindings file as text.
            if let Some(embedded) = orca.get("keybindings").and_then(Value::as_str) {
                orca = serde_json::from_str(embedded)
                    .context("the export's keybindings aren't JSON")?;
            }
            let platform = match platform {
                Some(KeymapPlatform::Mac) => keymap::Platform::Mac,
                Some(KeymapPlatform::Linux) => keymap::Platform::Linux,
                None => keymap::Platform::current(),
            };
            let target = zed_dir
                .clone()
                .or_else(icarus_settings::zed::config_dir)
                .context("no app config directory; pass --zed-dir")?;
            let translation = keymap::translate(&orca, platform);
            let applied = keymap::apply(&target, &translation, *dry_run)?;
            let value =
                json!({ "translation": translation, "applied": applied, "dry_run": dry_run });
            print(&cli, &value, |_| {
                let mut lines = translation.summary_lines();
                let verb = if *dry_run { "would write" } else { "wrote" };
                lines.push(format!(
                    "{verb} {} section(s) to {} (replacing {} from the last import)",
                    applied.added,
                    applied.keymap.display(),
                    applied.replaced
                ));
                lines.join("\n")
            });
        }
        Command::Settings { command } => {
            let client = connect(&cli).await?;
            match command {
                SettingsCommand::Import {
                    file,
                    dry_run,
                    print_zed,
                    skip_zed,
                    zed_dir,
                } => {
                    let path = std::fs::canonicalize(file).unwrap_or(file.clone());
                    let zed_dir = zed_dir
                        .as_ref()
                        .map(|dir| std::fs::canonicalize(dir).unwrap_or(dir.clone()));
                    let value = client
                        .request(
                            "settings.import_orca",
                            json!({ "path": path, "dry_run": dry_run, "skip_zed": skip_zed, "zed_dir": zed_dir }),
                        )
                        .await?;
                    if *print_zed && !cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&value["zed"]["fragment"])
                                .unwrap_or_default()
                        );
                    }
                    print(&cli, &value, |v| {
                        let mut lines = vec![format!("{} settings mapped", v["mapped"])];
                        let zed = &v["zed"];
                        if let Some(settings) = zed["settings"].as_str() {
                            let count = |key: &str| zed[key].as_array().map_or(0, Vec::len);
                            lines.push(format!(
                                "Zed settings {settings}: {} keys added, {} already set and kept",
                                count("added"),
                                count("kept")
                            ));
                            for key in zed["kept"].as_array().into_iter().flatten() {
                                lines
                                    .push(format!("  kept yours: {}", key.as_str().unwrap_or("?")));
                            }
                        }
                        if let Some(backup) = zed["backup"].as_str() {
                            lines
                                .push(format!("your commented Zed settings are saved in {backup}"));
                        }
                        for note in v["notes"].as_array().into_iter().flatten() {
                            lines.push(format!("note: {}", note.as_str().unwrap_or("?")));
                        }
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
                SettingsCommand::ImportOrcaState {
                    from,
                    drafts,
                    dry_run,
                } => {
                    let mut request = orca_state::read(from.as_deref())?;
                    request["pending_review_drafts"] = orca_state::read_drafts(drafts.as_deref())?;
                    request["dry_run"] = json!(dry_run);
                    let value = client
                        .request_timeout(
                            "settings.import_orca_state",
                            request,
                            Duration::from_secs(120),
                        )
                        .await?;
                    print(&cli, &value, orca_state::summary);
                }
                SettingsCommand::ImportZed { .. } | SettingsCommand::ImportKeybindings { .. } => {
                    unreachable!("handled without a server")
                }
            }
        }
        Command::Project { command } => {
            let client = connect(&cli).await?;
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
                ProjectCommand::Scripts { id, workspace } => {
                    let value = client
                        .request(
                            "project.scripts",
                            json!({ "id": id, "workspace": workspace }),
                        )
                        .await?;
                    print(&cli, &value, scripts_summary);
                    return Ok(());
                }
                ProjectCommand::Trust {
                    id,
                    workspace,
                    always,
                } => {
                    let value = client
                        .request(
                            "project.trust",
                            json!({ "id": id, "workspace": workspace, "always": always }),
                        )
                        .await?;
                    print(&cli, &value, scripts_summary);
                    return Ok(());
                }
                ProjectCommand::Untrust { id } => {
                    let value = client
                        .request("project.untrust", json!({ "id": id }))
                        .await?;
                    print(&cli, &value, |_| {
                        "no versions of its scripts are trusted now".into()
                    });
                    return Ok(());
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
            let client = connect(&cli).await?;
            let summary = |w: &Value| {
                let setup = match (
                    w["setup"]["state"].as_str(),
                    w["setup"]["exit_code"].as_i64(),
                ) {
                    (Some("failed"), Some(code)) => format!("  setup failed ({code})"),
                    (Some("failed"), None) => "  setup failed".to_owned(),
                    (Some("running"), _) => "  setting up".to_owned(),
                    _ => String::new(),
                };
                let pinned = if w["pinned"] == true { "  pinned" } else { "" };
                let unread = match w["unread"]["kind"].as_str() {
                    Some("needs_input") => "  needs input",
                    Some(_) => "  unread",
                    None => "",
                };
                let origin = match w["origin"].as_str() {
                    Some("native") | None => String::new(),
                    Some(origin) => format!("  [{origin}]"),
                };
                let hidden = if w["hidden"] == true { "  hidden" } else { "" };
                format!(
                    "{}  {}  {}  {}{origin}{hidden}{setup}{pinned}{unread}",
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
                    account,
                    remote_control,
                    setup,
                    no_setup,
                    from_pr,
                    trust,
                    always_trust,
                    from_issue,
                    no_issue_command,
                    recipe,
                } => {
                    let issue = match (from_linear, from_issue) {
                        (Some(id), _) => Some(json!({ "provider": "linear", "id": id })),
                        (None, Some(url)) => Some(json!({
                            "provider": "github",
                            "id": url.rsplit('/').next().map(|n| format!("#{n}")),
                            "url": url,
                        })),
                        (None, None) => None,
                    };
                    let trust = match (always_trust, trust) {
                        (true, _) => json!("always"),
                        (false, trust) => json!(trust),
                    };
                    let setup = match (setup, no_setup) {
                        (true, _) => json!("run"),
                        (_, true) => json!("skip"),
                        _ => Value::Null,
                    };
                    let create_id = format!(
                        "cli-{}-{}",
                        std::process::id(),
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos())
                            .unwrap_or_default()
                    );
                    let params = json!({
                        "project": project, "name": name, "branch": branch, "base": base,
                        "fetch": fetch, "status": status, "prompt": prompt, "issue": issue,
                        "agent": !no_agent, "layout": layout, "pr_url": pr_url,
                        "account": account, "remote_control": remote_control.map(Switch::enabled),
                        "setup": setup, "pr": from_pr, "trust": trust, "create_id": create_id,
                        "issue_command": !no_issue_command,
                    });
                    if let Some(recipe) = recipe {
                        let value = recipe::create(&client, recipe, params, trust != false).await?;
                        print(&cli, &value, |v| {
                            format!(
                                "environment {} on saved server {}\n{}",
                                v["environment"].as_str().unwrap_or("?"),
                                v["server"].as_str().unwrap_or("?"),
                                summary(&v["workspace"])
                            )
                        });
                        return Ok(());
                    }
                    let create = client.request_timeout(
                        "workspace.create",
                        params,
                        Duration::from_secs(300),
                    );
                    tokio::pin!(create);
                    let value = tokio::select! {
                        value = &mut create => value?,
                        _ = tokio::signal::ctrl_c() => {
                            eprintln!("cancelling…");
                            client
                                .request(
                                    "workspace.create_cancel",
                                    json!({ "create_id": create_id }),
                                )
                                .await?;
                            // The server answers the create once it has cleaned up.
                            create.await?
                        }
                    };
                    print(&cli, &value, summary);
                    for warning in value["warnings"].as_array().into_iter().flatten() {
                        eprintln!("warning: {}", warning.as_str().unwrap_or_default());
                    }
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
                WorkspaceCommand::Adopt {
                    path,
                    project,
                    name,
                } => {
                    let path = std::fs::canonicalize(path).unwrap_or(path.clone());
                    let value = client
                        .request(
                            "workspace.adopt",
                            json!({ "path": path, "project": project, "name": name }),
                        )
                        .await?;
                    print(&cli, &value, summary);
                }
                WorkspaceCommand::List { project, all } => {
                    let value = client
                        .request(
                            "workspace.list",
                            json!({ "project": project, "refresh": true, "include_hidden": all }),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        v.as_array()
                            .map(|i| i.iter().map(summary).collect::<Vec<_>>().join("\n"))
                            .unwrap_or_default()
                    });
                }
                WorkspaceCommand::Hide { id } | WorkspaceCommand::Unhide { id, .. } => {
                    let visibility = match command {
                        WorkspaceCommand::Hide { .. } => json!("hide"),
                        WorkspaceCommand::Unhide {
                            follow_settings: true,
                            ..
                        } => Value::Null,
                        _ => json!("show"),
                    };
                    let value = client
                        .request(
                            "workspace.visibility",
                            json!({ "id": id, "visibility": visibility }),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        if v["hidden"] == true {
                            "hidden".into()
                        } else {
                            "shown".into()
                        }
                    });
                }
                WorkspaceCommand::Inbox { project } => {
                    let value = client
                        .request("workspace.inbox", json!({ "project": project }))
                        .await?;
                    print(&cli, &value, |v| {
                        let lines: Vec<String> =
                            v.as_array()
                                .into_iter()
                                .flatten()
                                .flat_map(|entry| {
                                    let project =
                                        entry["project_name"].as_str().unwrap_or_default();
                                    entry["workspaces"].as_array().into_iter().flatten().map(
                                        move |w| {
                                            format!(
                                                "{project}  {}  {}  {}  [{}]",
                                                w["id"].as_str().unwrap_or_default(),
                                                w["name"].as_str().unwrap_or_default(),
                                                w["path"].as_str().unwrap_or_default(),
                                                w["origin"].as_str().unwrap_or_default(),
                                            )
                                        },
                                    )
                                })
                                .collect();
                        if lines.is_empty() {
                            "no new worktrees".into()
                        } else {
                            lines.join("\n")
                        }
                    });
                }
                WorkspaceCommand::ShowSource { project, source } => {
                    let value = client
                        .request(
                            "workspace.show_source",
                            json!({ "project": project, "source": source }),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        format!("set {} to show", v["key"].as_str().unwrap_or_default())
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
                    pin,
                    unpin,
                } => {
                    let pinned = (*pin || *unpin).then_some(*pin);
                    let value = client
                        .request(
                            "workspace.set",
                            json!({ "id": id, "status": status, "title": title, "base": base, "pinned": pinned }),
                        )
                        .await?;
                    print(&cli, &value, summary);
                }
                WorkspaceCommand::Reorder { ids } => {
                    let value = client
                        .request("workspace.reorder", json!({ "ids": ids }))
                        .await?;
                    print(&cli, &value, |v| {
                        format!(
                            "ordered {} workspace(s)",
                            v["ordered"].as_u64().unwrap_or(0)
                        )
                    });
                }
                WorkspaceCommand::MarkRead { id } | WorkspaceCommand::MarkUnread { id } => {
                    let unread = matches!(command, WorkspaceCommand::MarkUnread { .. });
                    let value = client
                        .request(
                            "workspace.set_unread",
                            json!({ "id": id, "unread": unread }),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        if v["unread"].is_null() {
                            "read".to_owned()
                        } else {
                            "unread".to_owned()
                        }
                    });
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
                    delete_worktree,
                    delete_branch,
                    skip_archive,
                    allow_failed_archive,
                } => {
                    let delete_branch = if *keep_branch {
                        Some(false)
                    } else {
                        delete_branch.then_some(true)
                    };
                    let value = client
                        .request_timeout(
                            "workspace.remove",
                            json!({
                                "id": id, "force": force, "delete_branch": delete_branch,
                                "delete_worktree": delete_worktree.then_some(true),
                                "skip_archive": skip_archive,
                                "allow_failed_archive": allow_failed_archive,
                            }),
                            Duration::from_secs(300),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        if v["hidden"] == true {
                            "hidden (the worktree was not created by Icarus, so it stays; pass --delete-worktree to delete it)".into()
                        } else if v["archive"]["success"] == false {
                            format!(
                                "removed (the archive script failed)\n{}",
                                v["archive"]["output"].as_str().unwrap_or_default()
                            )
                        } else {
                            "removed".into()
                        }
                    });
                }
                WorkspaceCommand::Setup { id, trust } => {
                    let value = client
                        .request("workspace.setup", json!({ "id": id, "trust": trust }))
                        .await?;
                    print(&cli, &value, |v| {
                        format!(
                            "setup running in terminal {}",
                            v["terminal"].as_str().unwrap_or("?")
                        )
                    });
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
                WorkspaceCommand::Sleep { id } | WorkspaceCommand::Wake { id } => {
                    let method = if matches!(command, WorkspaceCommand::Sleep { .. }) {
                        "workspace.sleep"
                    } else {
                        "workspace.wake"
                    };
                    let value = client
                        .request_timeout(method, json!({ "id": id }), Duration::from_secs(120))
                        .await?;
                    print(&cli, &value, |v| {
                        let count = |key: &str| v[key].as_array().map_or(0, Vec::len);
                        let verb = if v["sleeping"] == json!(true) {
                            "parked"
                        } else {
                            "woke"
                        };
                        format!(
                            "{verb} {} agent(s) and {} terminal(s)",
                            count("agents"),
                            count("terminals")
                        )
                    });
                }
            }
        }
        Command::Notifications {
            unread,
            limit,
            command,
        } => extras::notifications(&cli, *unread, *limit, command.as_ref()).await?,
        Command::Account { command } => extras::account(&cli, command).await?,
        Command::Update { command } => update::run(&cli, command).await?,
        Command::Changes { command } => {
            let client = connect(&cli).await?;
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
            let client = connect(&cli).await?;
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
            let client = connect(&cli).await?;
            ports::run(&cli, &client, command).await?;
        }
        Command::Notes { workspace, command } => {
            let client = connect(&cli).await?;
            notes::run(&cli, &client, workspace.as_deref(), command).await?;
        }
        Command::Git { workspace, command } => {
            let client = connect(&cli).await?;
            git_ops::run(&cli, &client, workspace.as_deref(), command).await?;
        }
        Command::Issue { command } => {
            let client = connect(&cli).await?;
            issue::run(&cli, &client, command).await?;
        }
        Command::Linear { command } => {
            let client = connect(&cli).await?;
            linear::run(&cli, &client, command).await?;
        }
        Command::Review { at, command } => {
            let client = connect(&cli).await?;
            review::run(&cli, &client, at, command).await?;
        }
        Command::Attach { command } => {
            let (method, params) = attach::attach_request(command)?;
            let client = connect(&cli).await?;
            let value = client
                .request_timeout(method, params, Duration::from_secs(180))
                .await?;
            print(&cli, &value, attach::attached);
        }
        Command::Composer { command } => {
            let client = connect(&cli).await?;
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
            let client = connect(&cli).await?;
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
            let client = connect(&cli).await?;
            match command {
                AgentCommand::Create {
                    cwd,
                    provider,
                    title,
                    workspace,
                    model,
                    prompt,
                    ask_permissions,
                    mode,
                    resume,
                    account,
                    remote_control,
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
                                "mode": mode, "resume": resume, "account": account,
                                "remote_control": remote_control.map(Switch::enabled),
                            }),
                        )
                        .await?;
                    print(&cli, &value, |v| {
                        v["id"].as_str().unwrap_or_default().to_string()
                    });
                }
                AgentCommand::RemoteControl {
                    state,
                    id,
                    workspace,
                } => {
                    let value = client
                        .request_timeout(
                            "agent.remote_control",
                            json!({ "id": id, "workspace": workspace, "enabled": state.enabled() }),
                            Duration::from_secs(60),
                        )
                        .await?;
                    print(&cli, &value, remote_control_summary);
                }
                AgentCommand::Mode { id, mode } => {
                    if let Some(mode) = mode {
                        client
                            .request_timeout(
                                "agent.set_mode",
                                json!({ "id": id, "mode": mode }),
                                Duration::from_secs(60),
                            )
                            .await?;
                    }
                    let value = client.request("agent.modes", json!({ "id": id })).await?;
                    print(&cli, &value, |v| choices_summary(v, "modes"));
                }
                AgentCommand::Model { id, model } => {
                    if let Some(model) = model {
                        client
                            .request_timeout(
                                "agent.set_model",
                                json!({ "id": id, "model": model }),
                                Duration::from_secs(60),
                            )
                            .await?;
                    }
                    let value = client.request("agent.models", json!({ "id": id })).await?;
                    print(&cli, &value, |v| choices_summary(v, "models"));
                }
                AgentCommand::Commands { id } => {
                    let value = client
                        .request("agent.commands", json!({ "id": id }))
                        .await?;
                    print(&cli, &value, |v| {
                        v["commands"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|c| {
                                let name = c["name"].as_str().unwrap_or_default();
                                let hint = c["hint"]
                                    .as_str()
                                    .map(|h| format!(" {h}"))
                                    .unwrap_or_default();
                                let description = c["description"].as_str().unwrap_or_default();
                                format!("/{name}{hint}  {description}")
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
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
                                        let mut line = format!(
                                            "{}  {}  {}  {}",
                                            a["id"].as_str().unwrap_or_default(),
                                            a["state"].as_str().unwrap_or_default(),
                                            a["provider"].as_str().unwrap_or_default(),
                                            a["title"].as_str().unwrap_or("-")
                                        );
                                        if let Some(url) = a["remote_control_url"].as_str() {
                                            line.push_str(&format!("  {url}"));
                                        } else if a["remote_control"] == json!(true) {
                                            line.push_str("  (remote control)");
                                        }
                                        line
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default()
                    });
                }
                AgentCommand::History {
                    workspace,
                    search,
                    limit,
                    before,
                } => {
                    extras::history(
                        &cli,
                        workspace.as_deref(),
                        search.as_deref(),
                        *limit,
                        *before,
                    )
                    .await?
                }
                AgentCommand::Resume { id } => extras::resume(&cli, id).await?,
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
            let client = connect(&cli).await?;
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
                                        let name = t["label"]
                                            .as_str()
                                            .or(t["title"].as_str())
                                            .unwrap_or("-");
                                        format!(
                                            "{}  {}  {}{}  {}",
                                            t["id"].as_str().unwrap_or_default(),
                                            if t["running"] == json!(true) {
                                                "running"
                                            } else {
                                                "exited"
                                            },
                                            if t["task"] == json!(true) {
                                                "task: "
                                            } else {
                                                ""
                                            },
                                            name,
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
                TerminalCommand::Rename { id, label } => {
                    let value = client
                        .request("terminal.set_label", json!({ "id": id, "label": label }))
                        .await?;
                    print(&cli, &value, |v| {
                        v["label"].as_str().unwrap_or("(no label)").to_owned()
                    });
                }
                TerminalCommand::Kill { id, remove } => {
                    let value = client
                        .request("terminal.kill", json!({ "id": id, "remove": remove }))
                        .await?;
                    print(&cli, &value, |_| String::new());
                }
                TerminalCommand::Attach { .. } => {
                    anyhow::bail!("attach is handled before connecting");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AppCommand, drive_request, drive_summary, facet_selection, split_position};
    use serde_json::json;
    use std::time::Duration;

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

    #[test]
    fn app_driver_commands_ask_the_app_and_read_its_answer() {
        let (request, within) = drive_request(&AppCommand::Key {
            keys: "alt-f7".into(),
            repeat: 20,
            interval: 30,
            wait: Some("diff-tab:*".into()),
            gone: false,
            timeout: 2000,
        });
        assert_eq!(
            request,
            json!({
                "op": "key", "keys": "alt-f7", "repeat": 20, "interval_ms": 30,
                "wait": { "target": "diff-tab:*", "gone": false, "timeout_ms": 2000 },
            })
        );
        assert_eq!(within, Duration::from_millis(2000 + 20 * 30 + 10_000));

        let answer = json!({
            "nodes": [{ "id": "card:ws-1", "bounds": [0.0, 0.0, 200.0, 40.0], "children": [
                { "text": "Fix login\nmain", "bounds": [8.0, 4.0, 120.0, 16.0] },
            ]}],
            "clicked": { "target": "card:ws-1", "x": 100.0, "y": 20.0 },
            "waited": { "target": "text:Changes", "gone": false, "elapsed_ms": 42 },
        });
        assert_eq!(
            drive_summary(&answer),
            "card:ws-1  [0,0 200x40]\n  \"Fix login ⏎ main\"  [8,4 120x16]\n\
             clicked card:ws-1 at 100,20\ntext:Changes showed after 42 ms"
        );
    }
}
