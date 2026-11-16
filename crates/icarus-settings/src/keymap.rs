//! Orca keybindings (`~/.orca/keybindings.json`) as a Zed keymap for the app.
//!
//! Like Orca, a remap replaces the action's default: the default keys are unbound for that action
//! and the new ones bound; an empty list leaves the action without a key; actions the file doesn't
//! mention keep the app's defaults, which follow Orca's.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

use crate::zed::{ApplyError, BACKUP_SUFFIX, strip_jsonc};

/// Remembers which keymap sections the import wrote, so importing again replaces them.
pub const IMPORT_RECORD_FILE: &str = ".icarus-orca-keymap.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Mac,
    Linux,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else {
            Self::Linux
        }
    }

    fn orca_name(self) -> &'static str {
        match self {
            Self::Mac => "darwin",
            Self::Linux => "linux",
        }
    }
}

/// Where an Orca action's keys apply in the app.
#[derive(Clone, Copy)]
enum Scope {
    /// The diff tab while it shows the diff; keys with a modifier also work from outside any text
    /// field, as Orca's outside-editor shortcuts do.
    Diff,
    /// The diff tab in either mode.
    DiffTab,
    /// Any full editor (a file or a diff tab).
    Editor,
    /// Everywhere: the window, editors and terminals, as Orca's app-wide shortcuts take precedence.
    Global,
    /// Terminals only.
    Terminal,
    /// `tab.selectByIndex`: one chord stands for 1–9.
    TabIndex,
}

impl Scope {
    fn contexts(self, key_has_modifier: bool) -> Vec<&'static str> {
        match self {
            Self::Diff if key_has_modifier => vec![
                "IcarusDiffTab && diff_mode > Editor",
                "Workspace && !Editor && !Terminal",
            ],
            Self::Diff => vec!["IcarusDiffTab && diff_mode > Editor"],
            Self::DiffTab => vec!["IcarusDiffTab > Editor"],
            Self::Editor => vec!["Editor && mode == full"],
            Self::Global | Self::TabIndex => vec!["Workspace", "Editor", "Terminal"],
            Self::Terminal => vec!["Terminal"],
        }
    }
}

struct Mapping {
    orca: &'static str,
    action: &'static str,
    /// The action's input, if it takes one.
    input: Option<fn() -> Value>,
    scope: Scope,
    /// Orca's default keys on macOS and Linux, which the app uses too.
    mac: &'static [&'static str],
    linux: &'static [&'static str],
}

fn close_unpinned() -> Value {
    json!({ "close_pinned": false })
}

fn keep_font_size() -> Value {
    json!({ "persist": false })
}

const fn map(
    orca: &'static str,
    action: &'static str,
    scope: Scope,
    mac: &'static [&'static str],
    linux: &'static [&'static str],
) -> Mapping {
    Mapping {
        orca,
        action,
        input: None,
        scope,
        mac,
        linux,
    }
}

const fn map_with(
    orca: &'static str,
    action: &'static str,
    input: fn() -> Value,
    scope: Scope,
    mac: &'static [&'static str],
    linux: &'static [&'static str],
) -> Mapping {
    Mapping {
        orca,
        action,
        input: Some(input),
        scope,
        mac,
        linux,
    }
}

const MAPPINGS: &[Mapping] = &[
    map(
        "editor.nextChange",
        "icarus::NextChange",
        Scope::Diff,
        &["F7"],
        &["F7"],
    ),
    map(
        "editor.previousChange",
        "icarus::PreviousChange",
        Scope::Diff,
        &["Shift+F7"],
        &["Shift+F7"],
    ),
    map(
        "editor.nextFile",
        "icarus::NextChangedFile",
        Scope::Diff,
        &["Alt+F7"],
        &["Alt+F7"],
    ),
    map(
        "editor.previousFile",
        "icarus::PreviousChangedFile",
        Scope::Diff,
        &["Alt+Shift+F7"],
        &["Alt+Shift+F7"],
    ),
    map(
        "editor.showEdit",
        "icarus::ShowEdit",
        Scope::Diff,
        &["Mod+Alt+E"],
        &[],
    ),
    map(
        "editor.showDiff",
        "icarus::ShowDiff",
        Scope::Editor,
        &["Mod+Alt+D"],
        &[],
    ),
    map(
        "editor.addReviewNote",
        "icarus::AddReviewComment",
        Scope::DiffTab,
        &["Mod+Shift+A"],
        &["Mod+Shift+A"],
    ),
    map(
        "editor.save",
        "workspace::Save",
        Scope::Global,
        &["Mod+S"],
        &["Mod+S"],
    ),
    map(
        "tab.nextSameType",
        "icarus::ActivateNextItemOfSameType",
        Scope::Global,
        &["Mod+Alt+BracketRight"],
        &["Mod+Alt+BracketRight"],
    ),
    map(
        "tab.previousSameType",
        "icarus::ActivatePreviousItemOfSameType",
        Scope::Global,
        &["Mod+Alt+BracketLeft"],
        &["Mod+Alt+BracketLeft"],
    ),
    map(
        "tab.nextAllTypes",
        "pane::ActivateNextItem",
        Scope::Global,
        &["Mod+Shift+BracketRight"],
        &["Mod+Shift+BracketRight"],
    ),
    map(
        "tab.previousAllTypes",
        "pane::ActivatePreviousItem",
        Scope::Global,
        &["Mod+Shift+BracketLeft"],
        &["Mod+Shift+BracketLeft"],
    ),
    map(
        "tab.selectByIndex",
        "pane::ActivateItem",
        Scope::TabIndex,
        &["Ctrl+1"],
        &["Alt+1"],
    ),
    map_with(
        "tab.close",
        "pane::CloseActiveItem",
        close_unpinned,
        Scope::Global,
        &["Mod+W"],
        &["Mod+W"],
    ),
    map_with(
        "tab.closeOthers",
        "pane::CloseOtherItems",
        close_unpinned,
        Scope::Global,
        &["Mod+Alt+Shift+W"],
        &["Mod+Alt+Shift+W"],
    ),
    map(
        "tab.reopenClosed",
        "pane::ReopenClosedItem",
        Scope::Global,
        &["Mod+Shift+T"],
        &["Mod+Shift+T"],
    ),
    map(
        "tab.newTerminal",
        "workspace::NewCenterTerminal",
        Scope::Global,
        &["Mod+T"],
        &["Mod+T"],
    ),
    map(
        "tab.newAgent",
        "icarus::NewAgent",
        Scope::Global,
        &["Mod+Alt+T"],
        &[],
    ),
    map(
        "worktree.quickOpen",
        "file_finder::Toggle",
        Scope::Global,
        &["Mod+P"],
        &["Mod+P"],
    ),
    map(
        "sidebar.left.toggle",
        "workspace::ToggleLeftDock",
        Scope::Global,
        &["Mod+B"],
        &["Mod+B"],
    ),
    map(
        "sidebar.right.toggle",
        "workspace::ToggleRightDock",
        Scope::Global,
        &["Mod+L"],
        &["Mod+L"],
    ),
    map(
        "sidebar.explorer.toggle",
        "project_panel::ToggleFocus",
        Scope::Global,
        &["Mod+Shift+E"],
        &["Mod+Shift+E"],
    ),
    map(
        "sidebar.sourceControl.toggle",
        "icarus::ToggleSourceControlPanel",
        Scope::Global,
        &["Mod+Shift+G"],
        &["Mod+Shift+G"],
    ),
    map(
        "sidebar.checks.toggle",
        "icarus::ToggleGitHubPanel",
        Scope::Global,
        &["Mod+Shift+K"],
        &["Mod+Shift+K"],
    ),
    map_with(
        "zoom.in",
        "zed::IncreaseBufferFontSize",
        keep_font_size,
        Scope::Global,
        &["Mod+Equal"],
        &["Mod+Equal"],
    ),
    map_with(
        "zoom.out",
        "zed::DecreaseBufferFontSize",
        keep_font_size,
        Scope::Global,
        &["Mod+Minus"],
        &["Mod+Minus"],
    ),
    map_with(
        "zoom.reset",
        "zed::ResetBufferFontSize",
        keep_font_size,
        Scope::Global,
        &["Mod+0"],
        &["Mod+0"],
    ),
    map(
        "terminal.clear",
        "terminal::Clear",
        Scope::Terminal,
        &["Mod+K"],
        &["Mod+K"],
    ),
    map(
        "sourceControl.toggleUncommittedOnly",
        "icarus::ToggleUncommittedChanges",
        Scope::Global,
        &["Mod+Alt+U"],
        &[],
    ),
    map(
        "sourceControl.openLatestAgentTurn",
        "icarus::OpenLatestAgentTurn",
        Scope::Global,
        &["Mod+Alt+Shift+T"],
        &[],
    ),
    map(
        "sourceControl.focusReviewSummary",
        "icarus::FocusReviewSummary",
        Scope::Global,
        &["Mod+Alt+Shift+R"],
        &[],
    ),
    map(
        "sourceControl.openPullRequest",
        "icarus::OpenPullRequest",
        Scope::Global,
        &["Mod+Alt+G"],
        &[],
    ),
    map(
        "sourceControl.copyPullRequestUrl",
        "icarus::CopyPullRequestUrl",
        Scope::Global,
        &["Mod+Alt+Shift+G"],
        &[],
    ),
    map(
        "sourceControl.openPullRequestPreview",
        "icarus::OpenPreview",
        Scope::Global,
        &["Mod+Alt+P"],
        &["Mod+Alt+P"],
    ),
    map(
        "workspace.openFrontend",
        "icarus::OpenFrontend",
        Scope::Global,
        &["Mod+Shift+B"],
        &["Mod+Shift+B"],
    ),
    map(
        "workspace.runNextAction",
        "icarus::RunNextAction",
        Scope::Global,
        &["Mod+Alt+Enter"],
        &["Mod+Alt+Enter"],
    ),
    // Orca leaves these unbound by default; its ⌥⌘P menu key is Icarus's Open preview.
    map(
        "tab.runQuickCommand",
        "icarus::RunQuickCommand",
        Scope::Global,
        &[],
        &[],
    ),
    map(
        "tab.openQuickCommandsMenu",
        "icarus::OpenQuickCommandsMenu",
        Scope::Global,
        &[],
        &[],
    ),
    // On Linux Ctrl+Alt+L is Zed's language-server menu, so Icarus binds it on macOS only.
    map(
        "workspace.openLinearIssue",
        "icarus::OpenLinearIssue",
        Scope::Global,
        &["Mod+Alt+L"],
        &[],
    ),
    map(
        "worktree.palette",
        "icarus::OpenWorkspaceSwitcher",
        Scope::Global,
        &["Mod+J"],
        &["Mod+J"],
    ),
];

/// Orca actions the app has no equivalent for yet, with why.
const UNSUPPORTED: &[(&str, &str)] = &[
    ("tab.rename", "the app has no tab rename yet"),
    (
        "sourceControl.sendQueuedComments",
        "the composer replaces the comment queue (attachments)",
    ),
    (
        "sourceControl.sendReviewNotes",
        "the composer replaces the note queue (attachments)",
    ),
];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mapped {
    pub orca: String,
    pub action: String,
    /// The Orca keys as written, and as the app's keystrokes.
    pub keys: Vec<String>,
    pub keystrokes: Vec<String>,
    /// The default keystrokes this remap unbinds.
    pub unbound: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Skipped {
    pub orca: String,
    pub keys: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Translation {
    pub platform: Platform,
    /// Zed keymap sections, in the order to append them.
    pub sections: Vec<Value>,
    pub mapped: Vec<Mapped>,
    pub skipped: Vec<Skipped>,
}

/// The overrides that apply on `platform`: the cross-platform map, then the platform's own.
fn overrides(orca: &Value, platform: Platform) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    let maps = [
        orca.get("keybindings"),
        orca.get("platforms")
            .and_then(|platforms| platforms.get(platform.orca_name())),
    ];
    for map in maps.into_iter().flatten().filter_map(Value::as_object) {
        for (action, keys) in map {
            let keys: Vec<String> = keys
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|key| key.as_str().map(str::to_owned))
                .collect();
            match out.iter_mut().find(|(existing, _)| existing == action) {
                Some(entry) => entry.1 = keys,
                None => out.push((action.clone(), keys)),
            }
        }
    }
    out
}

/// An Orca chord (`Mod+Shift+BracketRight`, `ArrowDown`, `Ctrl+E`) as a Zed keystroke (`cmd-}`).
pub fn keystroke(chord: &str, platform: Platform) -> Option<String> {
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut cmd = false;
    let parts: Vec<&str> = chord.split('+').collect();
    let (key, modifiers) = parts.split_last()?;
    for modifier in modifiers {
        match *modifier {
            "Mod" => match platform {
                Platform::Mac => cmd = true,
                Platform::Linux => ctrl = true,
            },
            "Cmd" | "Meta" => cmd = true,
            "Ctrl" => ctrl = true,
            "Alt" | "Option" => alt = true,
            "Shift" => shift = true,
            _ => return None,
        }
    }
    let mut key = key_name(key)?;
    // Zed spells shifted brackets as the symbol they type.
    if shift && (key == "[" || key == "]") {
        key = if key == "[" { "{" } else { "}" }.into();
        shift = false;
    }
    let mut out = String::new();
    for (on, name) in [
        (ctrl, "ctrl-"),
        (alt, "alt-"),
        (shift, "shift-"),
        (cmd, "cmd-"),
    ] {
        if on {
            out.push_str(name);
        }
    }
    out.push_str(&key);
    Some(out)
}

fn key_name(key: &str) -> Option<String> {
    let named = match key {
        "ArrowUp" => "up",
        "ArrowDown" => "down",
        "ArrowLeft" => "left",
        "ArrowRight" => "right",
        "BracketLeft" => "[",
        "BracketRight" => "]",
        "Equal" => "=",
        "Minus" => "-",
        "Plus" => "+",
        "Comma" => ",",
        "Period" => ".",
        "Slash" => "/",
        "Backslash" => "\\",
        "Semicolon" => ";",
        "Quote" => "'",
        "Backquote" => "`",
        "Enter" | "Return" => "enter",
        "Escape" | "Esc" => "escape",
        "Backspace" => "backspace",
        "Delete" => "delete",
        "Tab" => "tab",
        "Space" => "space",
        "Home" => "home",
        "End" => "end",
        "PageUp" => "pageup",
        "PageDown" => "pagedown",
        _ => "",
    };
    if !named.is_empty() {
        return Some(named.into());
    }
    if let Some(digit) = key.strip_prefix("Digit").filter(|d| d.len() == 1) {
        return Some(digit.into());
    }
    if let Some(number) = key.strip_prefix('F').filter(|n| n.parse::<u8>().is_ok()) {
        return Some(format!("f{number}"));
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphanumeric() => Some(c.to_ascii_lowercase().to_string()),
        _ => None,
    }
}

fn has_modifier(keystroke: &str) -> bool {
    ["ctrl-", "alt-", "cmd-"]
        .iter()
        .any(|modifier| keystroke.contains(modifier))
}

fn action_value(mapping: &Mapping, input: Option<Value>) -> Value {
    match input.or_else(|| mapping.input.map(|input| input())) {
        Some(input) => json!([mapping.action, input]),
        None => json!(mapping.action),
    }
}

/// One chord expanded to the bindings it stands for; `tab.selectByIndex`'s chord covers 1–9.
fn bindings_for(mapping: &Mapping, keystroke: &str) -> Vec<(String, Value)> {
    if !matches!(mapping.scope, Scope::TabIndex) {
        return vec![(keystroke.to_owned(), action_value(mapping, None))];
    }
    let Some(prefix) = keystroke.strip_suffix(|c: char| c.is_ascii_digit()) else {
        return Vec::new();
    };
    (1..=9)
        .map(|n| {
            (
                format!("{prefix}{n}"),
                action_value(mapping, Some(json!(n - 1))),
            )
        })
        .collect()
}

pub fn translate(orca: &Value, platform: Platform) -> Translation {
    let mut unbind = serde_json::Map::new();
    let mut bound: Vec<(&'static str, serde_json::Map<String, Value>)> = Vec::new();
    let mut mapped = Vec::new();
    let mut skipped = Vec::new();
    for (action, keys) in overrides(orca, platform) {
        let Some(mapping) = MAPPINGS.iter().find(|m| m.orca == action) else {
            let reason = UNSUPPORTED
                .iter()
                .find(|(orca, _)| *orca == action)
                .map_or("not an Orca action this importer knows", |(_, why)| why);
            skipped.push(Skipped {
                orca: action,
                keys,
                reason: reason.into(),
            });
            continue;
        };
        let defaults = match platform {
            Platform::Mac => mapping.mac,
            Platform::Linux => mapping.linux,
        };
        let mut unbound = Vec::new();
        for default in defaults {
            for (keystroke, value) in keystroke(default, platform)
                .map(|k| bindings_for(mapping, &k))
                .unwrap_or_default()
            {
                unbound.push(keystroke.clone());
                unbind.insert(keystroke, value);
            }
        }
        let mut keystrokes = Vec::new();
        for key in &keys {
            let Some(stroke) = keystroke(key, platform) else {
                skipped.push(Skipped {
                    orca: action.clone(),
                    keys: vec![key.clone()],
                    reason: format!("can't read the key {key:?}"),
                });
                continue;
            };
            for context in mapping.scope.contexts(has_modifier(&stroke)) {
                let section = match bound.iter_mut().find(|(c, _)| *c == context) {
                    Some((_, section)) => section,
                    None => {
                        bound.push((context, serde_json::Map::new()));
                        match bound.last_mut() {
                            Some((_, section)) => section,
                            None => continue,
                        }
                    }
                };
                for (keystroke, value) in bindings_for(mapping, &stroke) {
                    section.insert(keystroke, value);
                }
            }
            keystrokes.push(stroke);
        }
        mapped.push(Mapped {
            orca: action,
            action: mapping.action.into(),
            keys,
            keystrokes,
            unbound,
        });
    }
    let mut sections = Vec::new();
    if !unbind.is_empty() {
        // No context: an unbind drops that key for that action wherever the defaults bound it.
        sections.push(json!({ "unbind": unbind }));
    }
    for (context, bindings) in bound {
        sections.push(json!({ "context": context, "bindings": bindings }));
    }
    Translation {
        platform,
        sections,
        mapped,
        skipped,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeymapApplied {
    pub keymap: PathBuf,
    /// Sections a previous import wrote that this one replaced.
    pub replaced: usize,
    pub added: usize,
    /// The original keymap, saved before the first write that would drop its comments.
    pub backup: Option<PathBuf>,
}

/// Appends the translation to `<dir>/keymap.json`, replacing what an earlier import wrote and
/// leaving every other section alone; with `dry_run` nothing is written.
pub fn apply(
    dir: &Path,
    translation: &Translation,
    dry_run: bool,
) -> Result<KeymapApplied, ApplyError> {
    let keymap = dir.join("keymap.json");
    let record = dir.join(IMPORT_RECORD_FILE);
    let io_error = |path: &Path| {
        let path = path.to_owned();
        move |source| ApplyError::Io { path, source }
    };
    let read = |path: &Path| match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io_error(path)(err)),
    };
    let original = read(&keymap)?;
    let mut sections: Vec<Value> = match original.as_deref() {
        Some(text) if !text.trim().is_empty() => serde_json::from_str::<Value>(&strip_jsonc(text))
            .ok()
            .and_then(|value| value.as_array().cloned())
            .ok_or_else(|| ApplyError::Parse {
                path: keymap.clone(),
            })?,
        _ => Vec::new(),
    };
    let previous: Vec<Value> = read(&record)?
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    let before = sections.len();
    sections.retain(|section| !previous.contains(section));
    let replaced = before - sections.len();
    sections.extend(translation.sections.iter().cloned());
    let mut backup = None;
    if !dry_run {
        if let Some(text) = original
            .as_deref()
            .filter(|text| serde_json::from_str::<Value>(text).is_err())
        {
            let path = PathBuf::from(format!("{}{BACKUP_SUFFIX}", keymap.display()));
            if !path.exists() {
                fs::write(&path, text).map_err(io_error(&path))?;
            }
            backup = Some(path);
        }
        fs::create_dir_all(dir).map_err(io_error(dir))?;
        let pretty = |value: &Value| {
            serde_json::to_string_pretty(value).map(|mut text| {
                text.push('\n');
                text
            })
        };
        let keymap_text = pretty(&Value::Array(sections))
            .map_err(|err| io_error(&keymap)(io::Error::other(err)))?;
        fs::write(&keymap, keymap_text).map_err(io_error(&keymap))?;
        let record_text = pretty(&Value::Array(translation.sections.clone()))
            .map_err(|err| io_error(&record)(io::Error::other(err)))?;
        fs::write(&record, record_text).map_err(io_error(&record))?;
    }
    Ok(KeymapApplied {
        keymap,
        replaced,
        added: translation.sections.len(),
        backup,
    })
}

impl Translation {
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for mapped in &self.mapped {
            let keys = if mapped.keystrokes.is_empty() {
                "no key".to_owned()
            } else {
                mapped.keystrokes.join(", ")
            };
            lines.push(format!("{} → {}: {keys}", mapped.orca, mapped.action));
        }
        for skipped in &self.skipped {
            lines.push(format!("skipped {}: {}", skipped.orca, skipped.reason));
        }
        lines
    }
}
