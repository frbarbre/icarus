use crate::json::{Map, Value};

const PRIVATE_SETTING_KEYS: [&str; 9] = [
    "agentCmdOverrides",
    "agentDefaultEnv",
    "githubProjects",
    "defaultRepoSelection",
    "defaultLinearTeamSelection",
    "openInApplications",
    "devPluginPaths",
    "disabledPlugins",
    "minimaxUsageModels",
];

const PRIVATE_SETTING_SUBSTRINGS: [&str; 36] = [
    "account",
    "token",
    "cookie",
    "apikey",
    "secret",
    "password",
    "path",
    "cwd",
    "machine",
    "proxy",
    "runtime",
    "wsl",
    "udid",
    "telemetry",
    "pairing",
    "consent",
    "endpoint",
    "groupid",
    "workspaceid",
    "environment",
    "shell",
    "migrated",
    "migration",
    "defaulted",
    "seed",
    "prompted",
    "dismissed",
    "dirhistory",
    "device",
    "repoid",
    "hostid",
    "authority",
    "deliverygate",
    "parking",
    "retentionbudget",
    "http1",
];

pub const PORTABLE_UI_KEYS: [&str; 42] = [
    "groupBy",
    "sortBy",
    "projectOrderBy",
    "stackOrder",
    "showActiveOnly",
    "hideSleepingWorkspaces",
    "showSleepingWorkspaces",
    "hideDefaultBranchWorkspace",
    "hideAutomationGeneratedWorkspaces",
    "hideCliCreatedWorkspaces",
    "hideDetachedHeadWorkspaces",
    "hideTemporaryCheckoutWorkspaces",
    "hideWorkspacesFromOtherDevices",
    "alwaysShowDefaultBranchWorkspace",
    "agentsHideWorkspacesFromOtherDevices",
    "agentsHideAutomationGeneratedWorkspaces",
    "agentsHideCliCreatedWorkspaces",
    "agentsShowChildAgents",
    "agentsCompactMode",
    "agentsShowSearch",
    "agentsReadFilter",
    "agentsGroupBy",
    "uiZoomLevel",
    "editorFontZoomLevel",
    "worktreeCardProperties",
    "agentActivityDisplayMode",
    "workspaceStatuses",
    "workspaceStatusRules",
    "workspaceBoardOpacity",
    "workspaceBoardColumnWidth",
    "syncTaskStatusFromWorkspaceBoard",
    "statusBarItems",
    "statusBarVisible",
    "usagePercentageDisplay",
    "statusBarUsageMode",
    "browserDefaultUrl",
    "browserDefaultSearchEngine",
    "browserDefaultZoomLevel",
    "browserDevToolsDock",
    "petVisible",
    "petId",
    "petSize",
];

pub const LOCAL_STATUS_RULE_KEYS: [&str; 4] = [
    "repoIds",
    "handledPullRequests",
    "ledgerVersion",
    "newWorkspaceStatusSince",
];

pub const PORTABLE_REPO_FIELDS: [&str; 13] = [
    "worktreeBaseRef",
    "hookSettings",
    "issueSourcePreference",
    "defaultJiraProject",
    "forkSyncMode",
    "externalWorktreeVisibility",
    "agentWorktreeVisibility",
    "worktreeVisibilitySourcePreferences",
    "symlinkPaths",
    "pythonVenvPath",
    "portLabelsCommand",
    "ciTriage",
    "sourceControlAi",
];

pub const EDITOR_THEME_FILE_NAMES: [&str; 2] = ["editor-dark.json", "editor-light.json"];

pub const DEFAULT_CI_TRIAGE_INSTRUCTIONS: &str = include_str!("default-ci-triage.txt");

pub fn is_portable_setting_key(key: &str) -> bool {
    if PRIVATE_SETTING_KEYS.contains(&key) {
        return false;
    }
    let lower = key.to_lowercase();
    !(lower.ends_with("dir") || PRIVATE_SETTING_SUBSTRINGS.iter().any(|p| lower.contains(p)))
}

pub fn scrub_private_fields(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(scrub_private_fields).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| is_portable_setting_key(key))
                .map(|(key, entry)| (key.clone(), scrub_private_fields(entry)))
                .collect(),
        ),
        other => other.clone(),
    }
}

// With no local repos to resolve against, any command still scoped to a repo id is dropped.
fn portable_quick_commands(commands: &[Value]) -> Value {
    Value::Array(
        commands
            .iter()
            .filter(|command| {
                !command
                    .get("scope")
                    .and_then(Value::as_object)
                    .is_some_and(|scope| scope.contains_key("repoId"))
            })
            .map(scrub_private_fields)
            .collect(),
    )
}

pub fn pick_portable_settings(settings: &Map) -> Map {
    settings
        .iter()
        .filter(|(key, _)| is_portable_setting_key(key))
        .map(|(key, value)| {
            let value = match (key.as_str(), value) {
                ("terminalQuickCommands", Value::Array(commands)) => {
                    portable_quick_commands(commands)
                }
                _ => scrub_private_fields(value),
            };
            (key.clone(), value)
        })
        .collect()
}

pub fn pick_portable_ui(ui: &Map) -> Map {
    let mut picked = Map::new();
    for key in PORTABLE_UI_KEYS {
        let Some(value) = ui.get(key) else { continue };
        if key == "workspaceStatusRules" {
            if let Value::Object(rules) = value {
                let rules: Map = rules
                    .iter()
                    .filter(|(k, _)| !LOCAL_STATUS_RULE_KEYS.contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                picked.insert(key.to_owned(), Value::Object(rules));
            }
            continue;
        }
        picked.insert(key.to_owned(), value.clone());
    }
    picked
}

fn non_blank_string(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

fn one_of(value: Option<&Value>, allowed: &[&str]) -> Option<Value> {
    value
        .and_then(Value::as_str)
        .filter(|text| allowed.contains(text))
        .map(Value::from)
}

fn is_portable_relative_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    let rooted = matches!(bytes.first(), Some(b'/' | b'\\' | b'~'));
    !(drive || rooted || path.split(['/', '\\']).any(|segment| segment == ".."))
}

fn relative_path(value: Option<&Value>) -> Option<&str> {
    non_blank_string(value).filter(|path| is_portable_relative_path(path))
}

fn hook_settings(value: Option<&Value>) -> Option<Value> {
    let map = value?.as_object()?;
    let scripts = map.get("scripts")?.as_object()?;
    let mut out = Map::new();
    let mode = if map.get("mode").and_then(Value::as_str) == Some("override") {
        "override"
    } else {
        "auto"
    };
    out.insert("mode".into(), mode.into());
    let policies: [(&str, &[&str]); 3] = [
        (
            "setupRunPolicy",
            &["ask", "run-by-default", "skip-by-default"],
        ),
        (
            "setupAgentStartupPolicy",
            &["start-immediately", "wait-for-setup"],
        ),
        (
            "commandSourcePolicy",
            &["shared-only", "local-only", "run-both"],
        ),
    ];
    for (key, allowed) in policies {
        if let Some(policy) = one_of(map.get(key), allowed) {
            out.insert(key.into(), policy);
        }
    }
    let script = |name: &str| scripts.get(name).and_then(Value::as_str).unwrap_or("");
    let mut scripts_out = Map::new();
    scripts_out.insert("setup".into(), script("setup").into());
    scripts_out.insert("archive".into(), script("archive").into());
    out.insert("scripts".into(), Value::Object(scripts_out));
    Some(Value::Object(out))
}

fn jira_project(value: Option<&Value>) -> Option<Value> {
    let map = value?.as_object()?;
    let mut out = Map::new();
    for key in ["id", "key", "name", "siteId"] {
        let text = non_blank_string(map.get(key))?;
        out.insert(key.into(), text.trim().into());
    }
    Some(Value::Object(out))
}

fn visibility_source_preferences(value: Option<&Value>) -> Option<Value> {
    let built_in = value?
        .as_object()?
        .get("builtIn")
        .and_then(Value::as_object);
    let mut out = Map::new();
    for id in ["claude", "gsd"] {
        if let Some(visibility) = one_of(built_in.and_then(|b| b.get(id)), &["hide", "show"]) {
            out.insert(id.into(), visibility);
        }
    }
    if out.is_empty() {
        return None;
    }
    let mut wrapper = Map::new();
    wrapper.insert("builtIn".into(), Value::Object(out));
    Some(Value::Object(wrapper))
}

pub fn ci_triage_instructions(prompt: Option<&str>, hints: Option<&str>) -> String {
    if let Some(prompt) = prompt.filter(|p| !p.is_empty()) {
        return prompt.to_owned();
    }
    match hints.map(str::trim).filter(|h| !h.is_empty()) {
        Some(hints) => {
            format!("{DEFAULT_CI_TRIAGE_INSTRUCTIONS}\n\nAbout this repository:\n{hints}")
        }
        None => DEFAULT_CI_TRIAGE_INSTRUCTIONS.to_owned(),
    }
}

fn ci_triage(value: Option<&Value>) -> Option<Value> {
    let map = value?.as_object()?;
    let prompt = map.get("prompt").and_then(Value::as_str).map(str::trim);
    let hints = map.get("hints").and_then(Value::as_str);
    let instructions = ci_triage_instructions(prompt, hints);
    let mut out = Map::new();
    if instructions != DEFAULT_CI_TRIAGE_INSTRUCTIONS {
        out.insert("prompt".into(), instructions.into());
    }
    Some(Value::Object(out))
}

fn parse_repo_field(field: &str, value: Option<&Value>) -> Option<Value> {
    match field {
        "worktreeBaseRef" | "portLabelsCommand" => non_blank_string(value).map(Value::from),
        "hookSettings" => hook_settings(value),
        "issueSourcePreference" => one_of(value, &["upstream", "origin", "auto"]),
        "defaultJiraProject" => jira_project(value),
        "forkSyncMode" => one_of(value, &["ask", "safe-auto", "off"]),
        "externalWorktreeVisibility" | "agentWorktreeVisibility" => {
            one_of(value, &["hide", "show"])
        }
        "worktreeVisibilitySourcePreferences" => visibility_source_preferences(value),
        "symlinkPaths" => value.and_then(Value::as_array).map(|items| {
            Value::Array(
                items
                    .iter()
                    .filter_map(|item| relative_path(Some(item)).map(Value::from))
                    .collect(),
            )
        }),
        "pythonVenvPath" => relative_path(value).map(Value::from),
        "ciTriage" => ci_triage(value),
        // Approximation: Orca normalizes these per known action id; an object passes as is.
        "sourceControlAi" => value.filter(|v| v.is_object()).cloned(),
        _ => None,
    }
}

pub fn pick_portable_repo_settings(settings: &Map) -> Map {
    PORTABLE_REPO_FIELDS
        .into_iter()
        .filter_map(|field| {
            parse_repo_field(field, settings.get(field)).map(|value| (field.to_owned(), value))
        })
        .collect()
}

pub fn parse_portable_projects(value: Option<&Value>) -> Vec<Value> {
    let Some(entries) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let map = entry.as_object()?;
            let remote = map.get("remote")?.as_str()?;
            let settings = pick_portable_repo_settings(map.get("settings")?.as_object()?);
            if settings.is_empty() {
                return None;
            }
            let name = map.get("name").and_then(Value::as_str).unwrap_or("");
            let mut out = Map::new();
            out.insert("remote".into(), remote.into());
            out.insert("name".into(), name.into());
            out.insert("settings".into(), Value::Object(settings));
            Some(Value::Object(out))
        })
        .collect()
}

pub fn parse_editor_themes(value: Option<&Value>) -> Map {
    let mut out = Map::new();
    if let Some(map) = value.and_then(Value::as_object) {
        for name in EDITOR_THEME_FILE_NAMES {
            if let Some(Value::String(text)) = map.get(name) {
                out.insert(name.to_owned(), Value::String(text.clone()));
            }
        }
    }
    out
}
