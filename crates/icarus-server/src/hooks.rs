//! Per-project setup and archive scripts, Orca's `orca.yaml` hooks (features.md §2.1).
//!
//! Scripts come from a shared source (the repo's committed `orca.yaml`, else `project.hooks` in
//! `icarus.json`) and a local one (`project.hooks` in `icarus.local.json` or the user's settings),
//! combined by `project.hooks.commandSourcePolicy` exactly as Orca does. Setup runs in a visible
//! "Setup" server terminal; archive runs before a workspace is removed and can refuse the removal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use icarus_protocol::RpcError;
use icarus_protocol::layout::{LayoutNode, Tab};
use icarus_settings::layers::Layer;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::server::{Server, rpc};
use crate::state::Workspace;
use crate::terminal::{SpawnOptions, default_shell};

/// Printed by the Setup terminal after the script, so the server learns its exit code.
const SETUP_DONE: &str = "__ICARUS_SETUP_COMPLETE__:";
const ARCHIVE_TIMEOUT_MS: u64 = 120_000;
const ARCHIVE_OUTPUT_LIMIT: usize = 64 * 1024;
const SIGTERM_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourcePolicy {
    SharedOnly,
    LocalOnly,
    RunBoth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunPolicy {
    Ask,
    RunByDefault,
    SkipByDefault,
}

/// How a create call wants setup handled: Orca's `SetupDecision`, plus `Auto` for unattended
/// callers (the rules), which run setup where Orca would ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetupDecision {
    Inherit,
    Run,
    Skip,
    Auto,
}

impl SetupDecision {
    pub(crate) fn parse(value: Option<&Value>) -> Result<Self, RpcError> {
        Ok(match value {
            None | Some(Value::Null) => Self::Inherit,
            Some(Value::Bool(true)) => Self::Run,
            Some(Value::Bool(false)) => Self::Skip,
            Some(Value::String(text)) => match text.as_str() {
                "inherit" => Self::Inherit,
                "run" => Self::Run,
                "skip" => Self::Skip,
                "auto" => Self::Auto,
                other => {
                    return Err(rpc(
                        "invalid_params",
                        format!("setup must be run, skip, inherit or auto, not {other}"),
                    ));
                }
            },
            Some(other) => {
                return Err(rpc(
                    "invalid_params",
                    format!("setup must be a boolean or a string, not {other}"),
                ));
            }
        })
    }
}

/// Where the effective script came from, as Orca reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ScriptSource {
    Shared,
    Local,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Script {
    pub command: String,
    pub source: ScriptSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectHooks {
    pub setup: Option<Script>,
    pub archive: Option<Script>,
    /// The repository's own part of `setup` / `archive`, which runs only once trusted.
    pub shared_setup: Option<String>,
    pub shared_archive: Option<String>,
    pub run_policy: RunPolicy,
    pub wait_for_setup: bool,
    pub setup_timeout_ms: Option<u64>,
    pub archive_timeout_ms: u64,
    pub default_tabs: Vec<DefaultTab>,
    pub shared_directories: Vec<String>,
    /// The template run for a workspace linked to a GitHub item (`{{artifact_url}}`, `{{issue}}`).
    pub issue_command: Option<Script>,
    pub recipes: Vec<crate::recipes::Recipe>,
    pub recipe_problems: Vec<crate::recipes::RecipeProblem>,
}

/// One of `orca.yaml`'s `defaultTabs`: a terminal a new workspace opens with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DefaultTab {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

/// The parts of `orca.yaml` Icarus honours.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct OrcaYaml {
    pub setup: Option<String>,
    pub archive: Option<String>,
    pub wait_for_setup: bool,
    pub default_tabs: Vec<DefaultTab>,
    pub shared_directories: Vec<String>,
    pub issue_command: Option<String>,
    pub recipes: Vec<crate::recipes::Recipe>,
    pub recipe_problems: Vec<crate::recipes::RecipeProblem>,
}

const MAX_SHARED_DIRECTORIES: usize = 100;
const MAX_DEFAULT_TABS: usize = 256;

/// Orca's `#RGB` / `#RRGGBB` tab colours.
pub(crate) fn is_tab_color(text: &str) -> bool {
    text.strip_prefix('#').is_some_and(|hex| {
        matches!(hex.len(), 3 | 6) && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

/// Orca's rule: repo-relative, no `..`, `.` or `.git` segments, never absolute.
pub(crate) fn safe_relative_path(raw: &str) -> Option<String> {
    let normalized = raw.trim().replace('\\', "/");
    let normalized = normalized.strip_prefix("./").unwrap_or(&normalized);
    let normalized = normalized.trim_end_matches('/');
    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized.as_bytes().get(1) == Some(&b':')
    {
        return None;
    }
    normalized
        .split('/')
        .all(|segment| !matches!(segment, "" | "." | ".." | ".git"))
        .then(|| normalized.to_owned())
}

pub(crate) fn read_orca_yaml(dir: &Path) -> Option<OrcaYaml> {
    let text = std::fs::read_to_string(dir.join("orca.yaml")).ok()?;
    parse_orca_yaml(&text)
}

pub(crate) fn parse_orca_yaml(text: &str) -> Option<OrcaYaml> {
    let documents = yaml_rust2::YamlLoader::load_from_str(text).ok()?;
    let root = documents.into_iter().next()?;
    root.as_hash()?;
    let script = |name: &str| {
        root["scripts"][name]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let text_of = |value: &yaml_rust2::Yaml| {
        value
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let default_tabs = root["defaultTabs"]
        .as_vec()
        .filter(|tabs| tabs.len() <= MAX_DEFAULT_TABS)
        .into_iter()
        .flatten()
        .filter_map(|tab| {
            let tab = DefaultTab {
                title: text_of(&tab["title"]),
                command: text_of(&tab["command"]),
                color: text_of(&tab["color"]).filter(|color| is_tab_color(color)),
            };
            (tab.title.is_some() || tab.command.is_some() || tab.color.is_some()).then_some(tab)
        })
        .collect();
    let (recipes, recipe_problems) = crate::recipes::parse(&root["environmentRecipes"]);
    let mut shared_directories: Vec<String> = Vec::new();
    for entry in root["worktree"]["sharedDirectories"]
        .as_vec()
        .into_iter()
        .flatten()
        .take(MAX_SHARED_DIRECTORIES)
    {
        if let Some(path) = entry.as_str().and_then(safe_relative_path)
            && !shared_directories.contains(&path)
        {
            shared_directories.push(path);
        }
    }
    Some(OrcaYaml {
        setup: script("setup"),
        archive: script("archive"),
        wait_for_setup: root["setupAgentStartupPolicy"].as_str() == Some("wait-for-setup"),
        default_tabs,
        shared_directories,
        issue_command: text_of(&root["issueCommand"]),
        recipes,
        recipe_problems,
    })
}

/// `.worktreeinclude`: literal repo-relative paths, one per line; globs and negations aren't supported.
pub(crate) fn parse_worktree_include(text: &str) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with('!')
            || line.contains(['*', '?'])
        {
            continue;
        }
        if let Some(path) = safe_relative_path(line)
            && !entries.contains(&path)
        {
            entries.push(path);
        }
        if entries.len() >= 1000 {
            break;
        }
    }
    entries
}

fn trimmed(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn hooks_in(doc: &Value) -> &Value {
    &doc["project"]["hooks"]
}

/// Orca's `getEffectiveHookScript`: an unset policy means local-only when a local script exists.
fn effective_script(
    shared: Option<String>,
    local: Option<String>,
    policy: Option<&str>,
) -> Option<Script> {
    let policy = match policy {
        Some("local-only") => SourcePolicy::LocalOnly,
        Some("run-both") => SourcePolicy::RunBoth,
        Some("shared-only") => SourcePolicy::SharedOnly,
        _ if local.is_some() => SourcePolicy::LocalOnly,
        _ => SourcePolicy::SharedOnly,
    };
    match (policy, shared, local) {
        (SourcePolicy::LocalOnly, _, Some(local)) => Some(Script {
            command: local,
            source: ScriptSource::Local,
        }),
        (SourcePolicy::LocalOnly, _, None) => None,
        (SourcePolicy::RunBoth, Some(shared), Some(local)) => Some(Script {
            command: format!("{shared}\n{local}"),
            source: ScriptSource::Both,
        }),
        (SourcePolicy::RunBoth, None, Some(local)) => Some(Script {
            command: local,
            source: ScriptSource::Local,
        }),
        (_, Some(shared), _) => Some(Script {
            command: shared,
            source: ScriptSource::Shared,
        }),
        (_, None, _) => None,
    }
}

/// The hooks for a project, reading `orca.yaml` from `yaml_dir` (the worktree for setup, the repo
/// root for archive, as Orca does).
pub(crate) fn project_hooks(
    layers: &[(Layer, Value)],
    resolved: &Value,
    yaml_dir: Option<&Path>,
) -> ProjectHooks {
    let layer = |wanted: Layer| {
        layers
            .iter()
            .find(|(layer, _)| *layer == wanted)
            .map(|(_, doc)| doc)
    };
    let yaml = yaml_dir.and_then(read_orca_yaml).unwrap_or_default();
    let committed = layer(Layer::Project).map(hooks_in);
    let shared = |name: &str, from_yaml: Option<String>| {
        from_yaml.or_else(|| trimmed(committed.map(|h| &h["scripts"][name])))
    };
    let local = |name: &str| {
        [Layer::ProjectLocal, Layer::User]
            .into_iter()
            .find_map(|wanted| trimmed(layer(wanted).map(|doc| &hooks_in(doc)["scripts"][name])))
    };
    let hooks = hooks_in(resolved);
    let policy = hooks["commandSourcePolicy"].as_str();
    let shared_setup = shared("setup", yaml.setup);
    let shared_archive = shared("archive", yaml.archive);
    let setup = effective_script(shared_setup.clone(), local("setup"), policy);
    let archive = effective_script(shared_archive.clone(), local("archive"), policy);
    let uses_shared = |script: &Option<Script>| {
        script
            .as_ref()
            .is_some_and(|s| matches!(s.source, ScriptSource::Shared | ScriptSource::Both))
    };
    // Orca's `.orca/issue-command` is the user's own override and wins over the repository's template.
    let local_issue_command = yaml_dir
        .and_then(|dir| std::fs::read_to_string(dir.join(".orca").join("issue-command")).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .or_else(|| {
            [Layer::ProjectLocal, Layer::User]
                .into_iter()
                .find_map(|wanted| trimmed(layer(wanted).map(|doc| &hooks_in(doc)["issueCommand"])))
        });
    let issue_command = match (local_issue_command, yaml.issue_command) {
        (Some(command), _) => Some(Script {
            command,
            source: ScriptSource::Local,
        }),
        (None, Some(command)) => Some(Script {
            command,
            source: ScriptSource::Shared,
        }),
        (None, None) => None,
    };
    ProjectHooks {
        shared_setup: shared_setup.filter(|_| uses_shared(&setup)),
        shared_archive: shared_archive.filter(|_| uses_shared(&archive)),
        setup,
        archive,
        default_tabs: yaml.default_tabs,
        shared_directories: yaml.shared_directories,
        issue_command,
        recipes: yaml.recipes,
        recipe_problems: yaml.recipe_problems,
        run_policy: match hooks["setupRunPolicy"].as_str() {
            Some("ask") => RunPolicy::Ask,
            Some("skip-by-default") => RunPolicy::SkipByDefault,
            _ => RunPolicy::RunByDefault,
        },
        wait_for_setup: yaml.wait_for_setup
            || hooks["setupAgentStartupPolicy"].as_str() == Some("wait-for-setup"),
        setup_timeout_ms: hooks["setupTimeoutMs"].as_u64().filter(|ms| *ms > 0),
        archive_timeout_ms: hooks["archiveTimeoutMs"]
            .as_u64()
            .filter(|ms| *ms > 0)
            .unwrap_or(ARCHIVE_TIMEOUT_MS),
    }
}

/// Whether setup should run for a create, or why it can't be decided.
pub(crate) fn should_run_setup(
    policy: RunPolicy,
    decision: SetupDecision,
) -> Result<bool, RpcError> {
    match (decision, policy) {
        (SetupDecision::Run, _) => Ok(true),
        (SetupDecision::Skip, _) => Ok(false),
        (_, RunPolicy::RunByDefault) => Ok(true),
        (_, RunPolicy::SkipByDefault) => Ok(false),
        (SetupDecision::Auto, RunPolicy::Ask) => Ok(true),
        (SetupDecision::Inherit, RunPolicy::Ask) => Err(rpc(
            "setup_decision_required",
            "this project asks before running its setup script; create with setup run or skip (icarus workspace create --setup / --no-setup)",
        )),
    }
}

/// The variables Orca gives hook scripts (`setup-hook-env-vars.ts`), plus Icarus's own names.
pub(crate) fn hook_env(
    root: &Path,
    worktree: &Path,
    workspace_id: &str,
) -> BTreeMap<String, String> {
    let root = root.to_string_lossy().into_owned();
    let worktree_text = worktree.to_string_lossy().into_owned();
    let name = worktree
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut env = BTreeMap::new();
    for prefix in ["ORCA", "ICARUS"] {
        env.insert(format!("{prefix}_ROOT_PATH"), root.clone());
        env.insert(format!("{prefix}_WORKTREE_PATH"), worktree_text.clone());
        env.insert(format!("{prefix}_WORKSPACE_NAME"), name.clone());
    }
    // Legacy conductor.json compatibility, as Orca keeps it.
    env.insert("CONDUCTOR_ROOT_PATH".into(), root.clone());
    env.insert("GHOSTX_ROOT_PATH".into(), root);
    env.insert("ICARUS_WORKSPACE_ID".into(), workspace_id.to_owned());
    // Unattended scripts must never stop on a Git credential prompt.
    env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    env.insert("GCM_INTERACTIVE".into(), "never".into());
    env
}

/// Orca's POSIX runner: `set -e`, then the script, with a leading shebang's flags replayed.
pub(crate) fn runner_script(script: &str) -> String {
    let normalized = script.replace("\r\n", "\n");
    let (options, body) = match normalized.split_once('\n') {
        Some((first, rest)) if first.starts_with("#!") => (shebang_options(first), rest),
        None if normalized.starts_with("#!") => (shebang_options(&normalized), ""),
        _ => (String::new(), normalized.as_str()),
    };
    format!("#!/usr/bin/env bash\nset -e\n{options}{body}\n")
}

fn shebang_options(line: &str) -> String {
    let flags: Vec<&str> = line
        .split_whitespace()
        .skip(1)
        .filter(|word| word.starts_with('-') && word.len() > 1)
        .collect();
    if flags.is_empty() {
        String::new()
    } else {
        format!("set {}\n", flags.join(" "))
    }
}

fn single_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// What the Setup terminal runs: the script, the exit-code line, then the user's shell for later use.
/// It must work in any login shell; zsh, for one, reserves `$status`.
pub(crate) fn setup_command_line(runner: &Path, shell: &str) -> String {
    format!(
        "bash {runner}; icarus_setup_code=$?; printf '\\n{SETUP_DONE}%s\\n' \"$icarus_setup_code\"; exec {shell} -l",
        runner = single_quoted(&runner.to_string_lossy()),
        shell = single_quoted(shell),
    )
}

/// Finds the exit code the Setup terminal printed after the script, if it has finished.
pub(crate) fn setup_exit_code(output: &str) -> Option<i32> {
    let start = output.rfind(SETUP_DONE)? + SETUP_DONE.len();
    let digits: String = output[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SetupState {
    Running,
    Succeeded,
    Failed,
    Skipped,
}

/// A workspace's setup run, stored in the key-value store and shown on `workspace.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SetupStatus {
    pub state: SetupState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ScriptSource>,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    /// The agent waits for this run before its first prompt (`wait-for-setup`).
    #[serde(default)]
    pub agent_waits: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

pub(crate) fn setup_key(workspace_id: &str) -> String {
    format!("workspace.setup.{workspace_id}")
}

/// What a repository runs on the user's machine from its committed files, each trusted separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ScriptKind {
    Setup,
    Archive,
    DefaultTabs,
    IssueCommand,
    VmRecipe,
}

fn trust_key(project_id: &str) -> String {
    format!("project.trust.{project_id}")
}

/// Orca's "Always trust": every committed script of the project runs, whatever it says now or later.
fn trust_all_key(project_id: &str) -> String {
    format!("project.trust_all.{project_id}")
}

/// Keeps the store bounded; the newest approvals win.
const MAX_TRUSTED_DIGESTS: usize = 64;

fn digest(kind: ScriptKind, content: &str) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(format!("{kind:?}").as_bytes());
    hasher.update([0]);
    hasher.update(content.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn default_tab_commands(hooks: &ProjectHooks) -> Option<String> {
    let commands: Vec<&str> = hooks
        .default_tabs
        .iter()
        .filter_map(|tab| tab.command.as_deref())
        .collect();
    (!commands.is_empty()).then(|| commands.join("\n"))
}

fn shared_issue_command(hooks: &ProjectHooks) -> Option<String> {
    hooks
        .issue_command
        .as_ref()
        .filter(|script| script.source == ScriptSource::Shared)
        .map(|script| script.command.clone())
}

/// Orca's `renderIssueCommandTemplate`: `{{artifact_url}}` is the linked item's URL, `{{issue}}` its number.
pub(crate) fn render_issue_command(
    template: &str,
    number: Option<u64>,
    url: Option<&str>,
) -> String {
    let mut rendered = template.to_owned();
    if let Some(url) = url {
        rendered = rendered.replace("{{artifact_url}}", url);
    }
    if let Some(number) = number {
        rendered = rendered.replace("{{issue}}", &number.to_string());
    }
    rendered
}

/// The committed scripts in `hooks` that need the user's trust before they run.
pub(crate) fn committed_scripts(hooks: &ProjectHooks) -> Vec<(ScriptKind, String)> {
    [
        (ScriptKind::Setup, hooks.shared_setup.clone()),
        (ScriptKind::Archive, hooks.shared_archive.clone()),
        (ScriptKind::DefaultTabs, default_tab_commands(hooks)),
        (ScriptKind::IssueCommand, shared_issue_command(hooks)),
        (
            ScriptKind::VmRecipe,
            Some(crate::recipes::trust_content(&hooks.recipes)).filter(|text| !text.is_empty()),
        ),
    ]
    .into_iter()
    .filter_map(|(kind, content)| Some((kind, content?)))
    .collect()
}

pub(crate) fn untrusted_message(project_id: &str, what: &str) -> String {
    format!(
        "this project's {what} comes from its committed files (orca.yaml or icarus.json) and isn't trusted yet; review it with `icarus project scripts {project_id}`, then run `icarus project trust {project_id}` (or create with --trust)"
    )
}

/// Adds a terminal tab after the first pane's tabs.
pub(crate) fn add_terminal_tab(layout: &mut LayoutNode, terminal: &str) {
    let mut node = layout;
    loop {
        match node {
            LayoutNode::Split { children, .. } => match children.first_mut() {
                Some(first) => node = first,
                None => return,
            },
            LayoutNode::Pane { tabs, .. } => {
                tabs.push(Tab::Terminal {
                    command: None,
                    terminal_id: Some(terminal.to_owned()),
                });
                return;
            }
        }
    }
}

/// Shows the Setup terminal in a layout: in its first plain terminal tab, else after the first pane's tabs.
pub(crate) fn show_setup_terminal(layout: &mut LayoutNode, terminal: &str) {
    for tab in layout.tabs_mut() {
        if let Tab::Terminal {
            command: None,
            terminal_id: terminal_id @ None,
        } = tab
        {
            *terminal_id = Some(terminal.to_owned());
            return;
        }
    }
    add_terminal_tab(layout, terminal);
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ArchiveOutcome {
    pub success: bool,
    /// Why the script didn't run although the project has one (e.g. it isn't trusted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output: String,
    pub source: ScriptSource,
}

fn tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(ARCHIVE_OUTPUT_LIMIT);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Runs an archive script to completion (or the timeout), with output captured.
pub(crate) async fn run_archive_script(
    script: &Script,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> ArchiveOutcome {
    let mut command = tokio::process::Command::new("/bin/bash");
    command
        .arg("-c")
        .arg(&script.command)
        .current_dir(cwd)
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Its own process group, so a timeout reaches the script's children too.
        .process_group(0)
        .kill_on_drop(true);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return ArchiveOutcome {
                success: false,
                skipped: None,
                exit_code: None,
                timed_out: false,
                output: format!("could not start the archive script: {error}"),
                source: script.source,
            };
        }
    };
    let pid = child.id();
    let waited = tokio::time::timeout(timeout, child.wait_with_output()).await;
    match waited {
        Ok(Ok(output)) => {
            let mut text = tail(&output.stdout);
            let stderr = tail(&output.stderr);
            if !stderr.trim().is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&stderr);
            }
            let code = output.status.code();
            let mut text = text.trim_end().to_owned();
            if code != Some(0) {
                let reason = match code {
                    Some(code) => format!("Command failed with exit code {code}."),
                    None => "Command was terminated without reporting an exit code.".to_owned(),
                };
                text = format!("{text}\n{reason}").trim().to_owned();
            }
            ArchiveOutcome {
                success: code == Some(0),
                skipped: None,
                exit_code: code,
                timed_out: false,
                output: text,
                source: script.source,
            }
        }
        Ok(Err(error)) => ArchiveOutcome {
            success: false,
            skipped: None,
            exit_code: None,
            timed_out: false,
            output: format!("the archive script failed: {error}"),
            source: script.source,
        },
        Err(_) => {
            if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
                terminate_group(pid).await;
            }
            ArchiveOutcome {
                success: false,
                skipped: None,
                exit_code: None,
                timed_out: true,
                output: format!("Hook timed out after {}ms.", timeout.as_millis()),
                source: script.source,
            }
        }
    }
}

async fn terminate_group(pid: i32) {
    let Some(group) = rustix::process::Pid::from_raw(pid) else {
        return;
    };
    if rustix::process::kill_process_group(group, rustix::process::Signal::TERM).is_err() {
        return;
    }
    tokio::time::sleep(SIGTERM_GRACE).await;
    // The group may already be gone after a cooperative exit.
    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

impl Server {
    /// The project's hooks, with `orca.yaml` read from `yaml_dir`.
    pub(crate) fn hooks_for(&self, project_root: &Path, yaml_dir: &Path) -> ProjectHooks {
        let layers: Vec<(Layer, Value)> = match self.settings_stack(Some(project_root)) {
            Ok(stack) => stack
                .docs()
                .iter()
                .map(|doc| (doc.layer, crate::settings::to_json(&doc.doc)))
                .collect(),
            Err(error) => {
                tracing::warn!("settings for {}: {}", project_root.display(), error.message);
                Vec::new()
            }
        };
        let resolved = self.resolved_settings(Some(project_root));
        project_hooks(&layers, &resolved, Some(yaml_dir))
    }

    pub(crate) fn setup_status(&self, workspace_id: &str) -> Option<SetupStatus> {
        self.store
            .kv_get(&setup_key(workspace_id))
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_value(value).ok())
    }

    fn store_setup(&self, workspace_id: &str, status: &SetupStatus) {
        if let Err(error) = self.store.kv_set(&setup_key(workspace_id), &json!(status)) {
            tracing::warn!("saving setup status of {workspace_id}: {error}");
        }
        self.broadcast(
            "workspace.changed",
            json!({ "workspaces": [workspace_id], "source": "setup" }),
        );
    }

    /// Marks a setup that couldn't even start (e.g. its terminal failed to spawn) as failed.
    pub(crate) fn record_setup_failure(&self, workspace_id: &str, error: String) {
        let now = now_ms();
        self.store_setup(
            workspace_id,
            &SetupStatus {
                state: SetupState::Failed,
                terminal: None,
                exit_code: None,
                error: Some(error),
                source: None,
                started_at_ms: now,
                finished_at_ms: Some(now),
                agent_waits: false,
                timeout_ms: None,
            },
        );
    }

    /// Starts the project's setup script in a "Setup" server terminal; `None` when there is none to run.
    pub(crate) fn start_setup(
        self: &Arc<Self>,
        workspace: &Workspace,
        project_root: &Path,
        decision: SetupDecision,
    ) -> Result<Option<SetupStatus>, RpcError> {
        let hooks = self.hooks_for(project_root, &workspace.path);
        let Some(script) = hooks.setup.clone() else {
            return Ok(None);
        };
        let runs = should_run_setup(hooks.run_policy, decision)?;
        if runs
            && let Some(shared) = &hooks.shared_setup
            && !self.is_trusted(&workspace.project_id, ScriptKind::Setup, shared)
        {
            return Err(rpc(
                "untrusted_scripts",
                untrusted_message(&workspace.project_id, "setup script"),
            ));
        }
        if !runs {
            let status = SetupStatus {
                state: SetupState::Skipped,
                terminal: None,
                exit_code: None,
                error: None,
                source: Some(script.source),
                started_at_ms: now_ms(),
                finished_at_ms: Some(now_ms()),
                agent_waits: false,
                timeout_ms: None,
            };
            self.store_setup(&workspace.id, &status);
            return Ok(Some(status));
        }
        let runners = self.paths.home.join("setup-runners");
        std::fs::create_dir_all(&runners).map_err(|e| rpc("internal", e))?;
        let runner: PathBuf = runners.join(format!("{}.sh", workspace.id));
        std::fs::write(&runner, runner_script(&script.command)).map_err(|e| rpc("internal", e))?;
        let shell = default_shell();
        let line = setup_command_line(&runner, &shell);
        let session = self
            .terminals
            .create(SpawnOptions {
                cwd: workspace.path.clone(),
                command: Some(vec![shell, "-lc".into(), line]),
                env: hook_env(project_root, &workspace.path, &workspace.id),
                columns: 120,
                rows: 32,
                label: Some("Setup".into()),
                workspace: Some(workspace.id.clone()),
                color: None,
            })
            .map_err(|e| rpc("spawn_failed", format!("{e:#}")))?;
        let status = SetupStatus {
            state: SetupState::Running,
            terminal: Some(session.id().to_owned()),
            exit_code: None,
            error: None,
            source: Some(script.source),
            started_at_ms: now_ms(),
            finished_at_ms: None,
            agent_waits: hooks.wait_for_setup,
            timeout_ms: hooks.setup_timeout_ms,
        };
        self.store_setup(&workspace.id, &status);
        self.watch_setup(workspace.id.clone());
        Ok(Some(status))
    }

    /// Follows a running setup until its terminal reports an exit code, exits, or times out.
    fn watch_setup(self: &Arc<Self>, workspace_id: String) {
        let server = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let Some(mut status) = server.setup_status(&workspace_id) else {
                    return;
                };
                if status.state != SetupState::Running {
                    return;
                }
                let finished = match status
                    .terminal
                    .as_deref()
                    .and_then(|id| server.terminals.get(id))
                {
                    None => Some((None, Some("the Setup terminal was closed".to_owned()))),
                    Some(session) => {
                        let text = session.full_text();
                        let info = session.info();
                        if let Some(code) = setup_exit_code(&text) {
                            Some((Some(code), None))
                        } else if info.lost {
                            Some((
                                None,
                                Some(
                                    "the server stopped without keeping the Setup terminal"
                                        .to_owned(),
                                ),
                            ))
                        } else if !info.running {
                            Some((
                                info.exit_code.and_then(|c| i32::try_from(c).ok()),
                                Some(
                                    "the Setup terminal exited before the script finished"
                                        .to_owned(),
                                ),
                            ))
                        } else if status
                            .timeout_ms
                            .is_some_and(|ms| now_ms().saturating_sub(status.started_at_ms) > ms)
                        {
                            if let Err(error) = session.write(b"\x03") {
                                tracing::warn!("interrupting setup of {workspace_id}: {error:#}");
                            }
                            Some((
                                None,
                                Some(format!(
                                    "Setup timed out after {}ms.",
                                    status.timeout_ms.unwrap_or_default()
                                )),
                            ))
                        } else {
                            None
                        }
                    }
                };
                let Some((exit_code, error)) = finished else {
                    continue;
                };
                status.state = if exit_code == Some(0) && error.is_none() {
                    SetupState::Succeeded
                } else {
                    SetupState::Failed
                };
                status.exit_code = exit_code;
                status.error = error;
                status.finished_at_ms = Some(now_ms());
                server.store_setup(&workspace_id, &status);
                return;
            }
        });
    }

    /// Resumes following setups that were running when the previous server stopped.
    pub(crate) fn resume_setups(self: &Arc<Self>) {
        let Ok(workspaces) = self.store.workspaces(None) else {
            return;
        };
        for workspace in workspaces {
            if self
                .setup_status(&workspace.id)
                .is_some_and(|s| s.state == SetupState::Running)
            {
                self.watch_setup(workspace.id);
            }
        }
    }

    /// Waits until the workspace's setup is no longer running (or there is none).
    pub(crate) async fn wait_for_setup(&self, workspace_id: &str) {
        while self
            .setup_status(workspace_id)
            .is_some_and(|s| s.state == SetupState::Running)
        {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Whether a new agent in this workspace should hold its first prompt until setup finishes.
    pub(crate) fn agent_waits_for_setup(&self, workspace_id: &str) -> bool {
        self.setup_status(workspace_id)
            .is_some_and(|s| s.state == SetupState::Running && s.agent_waits)
    }

    /// Runs the project's archive script for a workspace about to be removed; `None` when there is none.
    pub(crate) async fn run_archive(
        &self,
        workspace: &Workspace,
        project_root: &Path,
    ) -> Option<ArchiveOutcome> {
        let hooks = self.hooks_for(project_root, project_root);
        let script = hooks.archive?;
        if let Some(shared) = &hooks.shared_archive
            && !self.is_trusted(&workspace.project_id, ScriptKind::Archive, shared)
        {
            // Declining Orca's prompt skips the script rather than keeping the workspace.
            return Some(ArchiveOutcome {
                success: true,
                skipped: Some(untrusted_message(&workspace.project_id, "archive script")),
                exit_code: None,
                timed_out: false,
                output: String::new(),
                source: script.source,
            });
        }
        let env = hook_env(project_root, &workspace.path, &workspace.id);
        Some(
            run_archive_script(
                &script,
                &workspace.path,
                &env,
                Duration::from_millis(hooks.archive_timeout_ms),
            )
            .await,
        )
    }

    fn trusted_digests(&self, project_id: &str) -> Vec<String> {
        self.store
            .kv_get(&trust_key(project_id))
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }

    pub(crate) fn is_trusted(&self, project_id: &str, kind: ScriptKind, content: &str) -> bool {
        self.always_trusted(project_id)
            || self
                .trusted_digests(project_id)
                .contains(&digest(kind, content))
    }

    pub(crate) fn always_trusted(&self, project_id: &str) -> bool {
        self.store
            .kv_get(&trust_all_key(project_id))
            .ok()
            .flatten()
            .is_some()
    }

    /// Trusts every committed script of the project from now on, including later changes.
    pub(crate) fn trust_always(&self, project_id: &str) -> Result<(), RpcError> {
        self.store
            .kv_set(
                &trust_all_key(project_id),
                &json!({ "approved_at_ms": now_ms() }),
            )
            .map_err(|e| rpc("internal", e))
    }

    /// Trusts every committed script in `hooks` as it is now; a later change asks again.
    pub(crate) fn trust_hooks(
        &self,
        project_id: &str,
        hooks: &ProjectHooks,
    ) -> Result<(), RpcError> {
        let mut digests = self.trusted_digests(project_id);
        for (kind, content) in committed_scripts(hooks) {
            let digest = digest(kind, &content);
            digests.retain(|known| *known != digest);
            digests.push(digest);
        }
        let excess = digests.len().saturating_sub(MAX_TRUSTED_DIGESTS);
        digests.drain(..excess);
        self.store
            .kv_set(&trust_key(project_id), &json!(digests))
            .map_err(|e| rpc("internal", e))
    }

    pub(crate) fn untrust_project(&self, project_id: &str) -> Result<(), RpcError> {
        for key in [trust_key(project_id), trust_all_key(project_id)] {
            self.store.kv_delete(&key).map_err(|e| rpc("internal", e))?;
        }
        Ok(())
    }

    /// What `project.scripts` reports: each committed script, whether it's trusted, and the setup policy.
    pub(crate) fn scripts_view(&self, project_id: &str, hooks: &ProjectHooks) -> Value {
        let entry = |script: &Option<Script>, shared: &Option<String>, kind: ScriptKind| {
            script.as_ref().map(|script| {
                json!({
                    "command": script.command,
                    "source": script.source,
                    "trusted": shared
                        .as_ref()
                        .is_none_or(|shared| self.is_trusted(project_id, kind, shared)),
                })
            })
        };
        let tabs_trusted = default_tab_commands(hooks)
            .is_none_or(|commands| self.is_trusted(project_id, ScriptKind::DefaultTabs, &commands));
        let untrusted: Vec<ScriptKind> = committed_scripts(hooks)
            .into_iter()
            .filter(|(kind, content)| !self.is_trusted(project_id, *kind, content))
            .map(|(kind, _)| kind)
            .collect();
        json!({
            "project": project_id,
            "setup": entry(&hooks.setup, &hooks.shared_setup, ScriptKind::Setup),
            "archive": entry(&hooks.archive, &hooks.shared_archive, ScriptKind::Archive),
            "issue_command": entry(&hooks.issue_command, &shared_issue_command(hooks), ScriptKind::IssueCommand),
            "default_tabs": hooks.default_tabs,
            "default_tabs_trusted": tabs_trusted,
            "recipes": hooks.recipes,
            "recipe_problems": hooks.recipe_problems,
            "shared_directories": hooks.shared_directories,
            "always_trusted": self.always_trusted(project_id),
            "run_policy": match hooks.run_policy {
                RunPolicy::Ask => "ask",
                RunPolicy::RunByDefault => "run-by-default",
                RunPolicy::SkipByDefault => "skip-by-default",
            },
            "trusted": untrusted.is_empty(),
            "untrusted": untrusted,
        })
    }

    /// Starts the terminals of `orca.yaml`'s `defaultTabs`; their commands run only when trusted.
    pub(crate) fn start_default_tabs(
        &self,
        workspace: &Workspace,
        project_root: &Path,
    ) -> Vec<String> {
        let hooks = self.hooks_for(project_root, &workspace.path);
        let run_commands = default_tab_commands(&hooks).is_some_and(|commands| {
            self.is_trusted(&workspace.project_id, ScriptKind::DefaultTabs, &commands)
        });
        let shell = default_shell();
        let mut terminals = Vec::new();
        for tab in &hooks.default_tabs {
            let command = tab
                .command
                .as_ref()
                .filter(|_| run_commands)
                .map(|command| {
                    vec![
                        shell.clone(),
                        "-lc".into(),
                        format!("{command}\nexec {} -l", single_quoted(&shell)),
                    ]
                });
            let label = tab
                .title
                .clone()
                .or_else(|| tab.command.clone())
                .unwrap_or_else(|| "Terminal".into());
            match self.terminals.create(SpawnOptions {
                cwd: workspace.path.clone(),
                command,
                env: hook_env(project_root, &workspace.path, &workspace.id),
                columns: 120,
                rows: 32,
                label: Some(label),
                workspace: Some(workspace.id.clone()),
                color: tab.color.clone(),
            }) {
                Ok(session) => terminals.push(session.id().to_owned()),
                Err(error) => tracing::warn!("default tab for {}: {error:#}", workspace.name),
            }
        }
        terminals
    }

    /// Starts the project's issue command for a workspace linked to a GitHub item, in its own
    /// terminal beside the agent, as Orca's issue-command split does; it doesn't wait for setup.
    /// An untrusted repository template doesn't run, and the reason comes back instead.
    pub(crate) fn start_issue_command(
        &self,
        workspace: &Workspace,
        project_root: &Path,
        number: Option<u64>,
        url: &str,
    ) -> Result<Option<String>, String> {
        let hooks = self.hooks_for(project_root, project_root);
        let Some(script) = hooks.issue_command.clone() else {
            return Ok(None);
        };
        if script.source == ScriptSource::Shared
            && !self.is_trusted(
                &workspace.project_id,
                ScriptKind::IssueCommand,
                &script.command,
            )
        {
            return Err(untrusted_message(&workspace.project_id, "issue command"));
        }
        let shell = default_shell();
        let command = render_issue_command(&script.command, number, Some(url));
        let session = self
            .terminals
            .create(SpawnOptions {
                cwd: workspace.path.clone(),
                command: Some(vec![
                    shell.clone(),
                    "-lc".into(),
                    format!("{command}\nexec {} -l", single_quoted(&shell)),
                ]),
                env: hook_env(project_root, &workspace.path, &workspace.id),
                columns: 120,
                rows: 32,
                label: Some("Issue command".into()),
                workspace: Some(workspace.id.clone()),
                color: None,
            })
            .map_err(|error| format!("{error:#}"))?;
        Ok(Some(session.id().to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layers(project: Value, local: Value, user: Value) -> Vec<(Layer, Value)> {
        vec![
            (Layer::User, user),
            (Layer::Project, project),
            (Layer::ProjectLocal, local),
        ]
    }

    fn hooks(scripts: Value) -> Value {
        json!({ "project": { "hooks": { "scripts": scripts } } })
    }

    fn merge(base: &mut Value, overlay: &Value) {
        match (base, overlay) {
            (Value::Object(base), Value::Object(overlay)) => {
                for (key, value) in overlay {
                    merge(base.entry(key.clone()).or_insert(Value::Null), value);
                }
            }
            (base, overlay) => *base = overlay.clone(),
        }
    }

    fn resolve(layers: &[(Layer, Value)]) -> Value {
        let mut out = json!({});
        for (_, doc) in layers {
            merge(&mut out, doc);
        }
        out
    }

    #[test]
    fn orca_yaml_scripts_and_wait_policy_parse() {
        let parsed = parse_orca_yaml(
            "scripts:\n  setup: |\n    echo one\n    echo two\n  archive: echo bye\nsetupAgentStartupPolicy: wait-for-setup\n",
        )
        .expect("parses");
        assert_eq!(parsed.setup.as_deref(), Some("echo one\necho two"));
        assert_eq!(parsed.archive.as_deref(), Some("echo bye"));
        assert!(parsed.wait_for_setup);
        assert_eq!(parse_orca_yaml("scripts: [").map(|_| ()), None);
    }

    #[test]
    fn a_local_issue_command_wins_and_needs_no_trust() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(
            dir.path().join("orca.yaml"),
            "issueCommand: shared {{issue}}\n",
        )
        .expect("yaml");
        let got = project_hooks(&[], &json!({}), Some(dir.path()));
        assert_eq!(
            got.issue_command,
            Some(Script {
                command: "shared {{issue}}".into(),
                source: ScriptSource::Shared,
            })
        );
        assert_eq!(
            committed_scripts(&got),
            [(ScriptKind::IssueCommand, "shared {{issue}}".to_owned())]
        );
        std::fs::create_dir(dir.path().join(".orca")).expect("dir");
        std::fs::write(dir.path().join(".orca/issue-command"), "  mine\n").expect("local");
        let got = project_hooks(&[], &json!({}), Some(dir.path()));
        assert_eq!(
            got.issue_command,
            Some(Script {
                command: "mine".into(),
                source: ScriptSource::Local,
            })
        );
        assert!(committed_scripts(&got).is_empty());
    }

    #[test]
    fn default_tabs_and_shared_paths_parse_like_orca() {
        let parsed = parse_orca_yaml(
            "defaultTabs:\n  - title: Dev\n    command: pnpm dev\n    color: '#22aa88'\n  - color: '#fff'\n  - color: red\n  - command: ' '\n  - title: Logs\n    color: '#12345'\nworktree:\n  sharedDirectories: [node_modules, ./node_modules/, /abs, ../up, a/./b, .git/x, C:/win, packages/web/.next]\nissueCommand: |\n  claude \"Fix {{artifact_url}}\"\n",
        )
        .expect("parses");
        assert_eq!(
            parsed.default_tabs,
            [
                DefaultTab {
                    title: Some("Dev".into()),
                    command: Some("pnpm dev".into()),
                    color: Some("#22aa88".into()),
                },
                DefaultTab {
                    title: None,
                    command: None,
                    color: Some("#fff".into()),
                },
                DefaultTab {
                    title: Some("Logs".into()),
                    command: None,
                    color: None,
                },
            ]
        );
        assert_eq!(
            parsed.issue_command.as_deref(),
            Some("claude \"Fix {{artifact_url}}\"")
        );
        assert_eq!(
            render_issue_command(
                "fix {{artifact_url}} (#{{issue}}) {{artifact_url}}",
                Some(7),
                Some("https://github.com/o/r/pull/7")
            ),
            "fix https://github.com/o/r/pull/7 (#7) https://github.com/o/r/pull/7"
        );
        assert_eq!(
            parsed.shared_directories,
            ["node_modules", "packages/web/.next"]
        );
        assert_eq!(
            parse_worktree_include("# c\n.env\n./.env\n*.key\n!x\n../y\nconfig/local.json/\n"),
            [".env", "config/local.json"]
        );
    }

    #[test]
    fn only_scripts_committed_to_the_repository_need_trust() {
        let stack = layers(
            hooks(json!({ "setup": "shared", "archive": "bye" })),
            hooks(json!({ "setup": "local" })),
            json!({}),
        );
        let got = project_hooks(&stack, &resolve(&stack), None);
        assert_eq!(
            got.shared_setup, None,
            "the local setup replaces the shared one"
        );
        assert_eq!(
            committed_scripts(&got),
            [(ScriptKind::Archive, "bye".to_owned())]
        );
        assert_ne!(
            digest(ScriptKind::Setup, "bye"),
            digest(ScriptKind::Archive, "bye")
        );
    }

    #[test]
    fn a_local_script_wins_unless_the_policy_says_otherwise() {
        let stack = layers(
            hooks(json!({ "setup": "shared" })),
            hooks(json!({ "setup": "local" })),
            json!({}),
        );
        let got = project_hooks(&stack, &resolve(&stack), None);
        assert_eq!(
            got.setup,
            Some(Script {
                command: "local".into(),
                source: ScriptSource::Local
            })
        );

        let mut both = stack.clone();
        both[2].1["project"]["hooks"]["commandSourcePolicy"] = json!("run-both");
        let got = project_hooks(&both, &resolve(&both), None);
        assert_eq!(
            got.setup.map(|s| s.command).as_deref(),
            Some("shared\nlocal")
        );

        let mut shared_only = stack.clone();
        shared_only[2].1["project"]["hooks"]["commandSourcePolicy"] = json!("shared-only");
        let got = project_hooks(&shared_only, &resolve(&shared_only), None);
        assert_eq!(got.setup.map(|s| s.source), Some(ScriptSource::Shared));

        let mut local_only = layers(hooks(json!({ "setup": "shared" })), json!({}), json!({}));
        local_only[2].1 =
            json!({ "project": { "hooks": { "commandSourcePolicy": "local-only" } } });
        assert_eq!(
            project_hooks(&local_only, &resolve(&local_only), None).setup,
            None
        );
    }

    #[test]
    fn orca_yaml_is_the_shared_source_ahead_of_icarus_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("orca.yaml"),
            "scripts:\n  setup: from-yaml\nsetupAgentStartupPolicy: wait-for-setup\n",
        )
        .expect("write");
        let stack = layers(hooks(json!({ "setup": "from-json" })), json!({}), json!({}));
        let got = project_hooks(&stack, &resolve(&stack), Some(dir.path()));
        assert_eq!(got.setup.map(|s| s.command).as_deref(), Some("from-yaml"));
        assert!(got.wait_for_setup);
        assert_eq!(got.run_policy, RunPolicy::RunByDefault);
        assert_eq!(got.archive_timeout_ms, ARCHIVE_TIMEOUT_MS);
    }

    #[test]
    fn run_policy_and_decision_combine_like_orca() {
        assert!(should_run_setup(RunPolicy::RunByDefault, SetupDecision::Inherit).expect("ok"));
        assert!(!should_run_setup(RunPolicy::SkipByDefault, SetupDecision::Inherit).expect("ok"));
        assert!(should_run_setup(RunPolicy::SkipByDefault, SetupDecision::Run).expect("ok"));
        assert!(!should_run_setup(RunPolicy::RunByDefault, SetupDecision::Skip).expect("ok"));
        assert!(should_run_setup(RunPolicy::Ask, SetupDecision::Auto).expect("ok"));
        assert!(should_run_setup(RunPolicy::Ask, SetupDecision::Inherit).is_err());
    }

    #[test]
    fn the_runner_sets_e_and_replays_shebang_flags() {
        assert_eq!(
            runner_script("echo hi\r\necho bye"),
            "#!/usr/bin/env bash\nset -e\necho hi\necho bye\n"
        );
        assert_eq!(
            runner_script("#!/bin/bash -x\necho hi"),
            "#!/usr/bin/env bash\nset -e\nset -x\necho hi\n"
        );
    }

    #[test]
    fn the_setup_line_reports_the_exit_code_in_every_installed_shell() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runner = dir.path().join("runner.sh");
        std::fs::write(&runner, runner_script("echo hi\nexit 2")).expect("runner");
        let mut tried = 0;
        for shell in ["/bin/sh", "/bin/bash", "/bin/zsh"] {
            if !Path::new(shell).exists() {
                continue;
            }
            tried += 1;
            // `true` stands in for the interactive shell the terminal would continue with.
            let line = setup_command_line(&runner, "/usr/bin/true");
            let output = std::process::Command::new(shell)
                .args(["-c", &line])
                .output()
                .expect("run");
            let text = String::from_utf8_lossy(&output.stdout);
            assert_eq!(setup_exit_code(&text), Some(2), "{shell}: {text}");
        }
        assert!(tried > 0);
    }

    #[test]
    fn the_exit_code_marker_is_read_from_terminal_output() {
        assert_eq!(
            setup_exit_code("x\n__ICARUS_SETUP_COMPLETE__:3\n$ "),
            Some(3)
        );
        assert_eq!(setup_exit_code("still running"), None);
    }

    #[test]
    fn the_setup_terminal_takes_a_plain_terminal_tab_or_joins_the_first_pane() {
        let mut with_terminal: LayoutNode = serde_json::from_value(json!({
            "split": "row",
            "children": [
                { "tabs": [{ "type": "agent" }] },
                { "tabs": [{ "type": "terminal", "command": "pnpm dev" }, { "type": "terminal" }] }
            ]
        }))
        .expect("layout");
        show_setup_terminal(&mut with_terminal, "term-1");
        assert_eq!(
            serde_json::to_value(&with_terminal).expect("json")["children"][1]["tabs"][1]["terminal_id"],
            "term-1"
        );

        let mut agent_only: LayoutNode =
            serde_json::from_value(json!({ "tabs": [{ "type": "agent" }] })).expect("layout");
        show_setup_terminal(&mut agent_only, "term-2");
        assert_eq!(
            serde_json::to_value(&agent_only).expect("json"),
            json!({ "tabs": [{ "type": "agent" }, { "type": "terminal", "terminal_id": "term-2" }] })
        );
    }

    #[tokio::test]
    async fn an_archive_script_reports_success_failure_and_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = hook_env(dir.path(), dir.path(), "ws-1");
        let script = |command: &str| Script {
            command: command.into(),
            source: ScriptSource::Shared,
        };
        let ok = run_archive_script(
            &script("echo \"$ORCA_WORKSPACE_NAME\"; pwd"),
            dir.path(),
            &env,
            Duration::from_secs(10),
        )
        .await;
        assert!(ok.success, "{ok:?}");
        assert!(
            ok.output.contains(
                &dir.path()
                    .file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned()
            )
        );

        let failed = run_archive_script(
            &script("echo nope >&2; exit 4"),
            dir.path(),
            &env,
            Duration::from_secs(10),
        )
        .await;
        assert!(!failed.success);
        assert_eq!(failed.exit_code, Some(4));
        assert!(
            failed.output.contains("nope") && failed.output.contains("exit code 4"),
            "{}",
            failed.output
        );

        let slow = run_archive_script(
            &script("sleep 30"),
            dir.path(),
            &env,
            Duration::from_millis(300),
        )
        .await;
        assert!(slow.timed_out && !slow.success);
    }
}
