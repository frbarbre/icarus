//! Orca appearance as Zed settings and a Zed theme, merged into the app's own Zed config directory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::json::{self, Map, Number, Value};

pub const IMPORTED_THEME_FILE: &str = "icarus-imported.json";
pub const ZED_THEME_SCHEMA: &str = "https://zed.dev/schema/themes/v0.2.0.json";
pub const BACKUP_SUFFIX: &str = ".before-icarus-import";
const DEFAULT_DARK: &str = "One Dark";
const DEFAULT_LIGHT: &str = "One Light";

pub fn imported_theme_name(slot: &str) -> String {
    match slot {
        "light" => "Icarus Imported Light".into(),
        _ => "Icarus Imported Dark".into(),
    }
}

/// The app's Zed config directory (`paths::config_dir()` in the fork, `APP_NAME` "Icarus");
/// `ICARUS_ZED_CONFIG_DIR` overrides it.
pub fn config_dir() -> Option<PathBuf> {
    let absolute = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    if let Some(dir) = absolute("ICARUS_ZED_CONFIG_DIR") {
        return Some(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let base = if cfg!(target_os = "macos") {
        home.join(".config")
    } else {
        absolute("FLATPAK_XDG_CONFIG_HOME")
            .or_else(|| absolute("XDG_CONFIG_HOME"))
            .unwrap_or_else(|| home.join(".config"))
    };
    Some(base.join("icarus"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub settings: Value,
    pub theme_family: Option<Value>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub settings: PathBuf,
    pub theme: Option<PathBuf>,
    /// Keys the import added to the Zed settings.
    pub added: Vec<String>,
    /// Keys the Zed settings already had; their values were left alone.
    pub kept: Vec<String>,
    /// The original settings file, saved before the first write that would drop its comments.
    pub backup: Option<PathBuf>,
    pub merged: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is not valid JSON with comments; fix it and import again", path.display())]
    Parse { path: PathBuf },
}

/// Zed's settings files are JSON with comments and trailing commas; this drops both.
pub fn strip_jsonc(text: &str) -> String {
    let text = strip_comments(text);
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' {
                i += 1;
                out.extend(chars.get(i));
            } else if c == '"' {
                in_string = false;
            }
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
            if !matches!(next, Some('}' | ']')) {
                out.push(c);
            }
        } else {
            in_string = c == '"';
            out.push(c);
        }
        i += 1;
    }
    out
}

fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = ' ';
                for next in chars.by_ref() {
                    if last == '*' && next == '/' {
                        break;
                    }
                    last = next;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

fn key_name(path: &[String]) -> String {
    path.join(".")
}

/// Adds what `overlay` has and `base` lacks; any value `base` already holds stays.
pub fn merge_missing(
    base: &mut Value,
    overlay: &Value,
    path: &mut Vec<String>,
    added: &mut Vec<String>,
    kept: &mut Vec<String>,
) {
    let (Value::Object(base), Value::Object(overlay)) = (base, overlay) else {
        return;
    };
    for (key, value) in overlay {
        path.push(key.clone());
        match base.get_mut(key) {
            None => {
                base.insert(key.clone(), value.clone());
                added.push(key_name(path));
            }
            Some(existing) if existing.is_object() && value.is_object() => {
                merge_missing(existing, value, path, added, kept);
            }
            Some(existing) => {
                if existing != value {
                    kept.push(key_name(path));
                }
            }
        }
        path.pop();
    }
}

/// Merges `fragment` into `<dir>/settings.json` and writes `theme` to `<dir>/themes`; with
/// `dry_run` nothing is written.
pub fn apply(
    dir: &Path,
    fragment: &Value,
    theme: Option<&Value>,
    dry_run: bool,
) -> Result<Applied, ApplyError> {
    let settings = dir.join("settings.json");
    let io_error = |path: &Path| {
        let path = path.to_owned();
        move |source| ApplyError::Io { path, source }
    };
    let original = match fs::read_to_string(&settings) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(io_error(&settings)(err)),
    };
    let mut merged = match &original {
        Some(text) if !text.trim().is_empty() => json::parse(&strip_jsonc(text))
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| ApplyError::Parse {
                path: settings.clone(),
            })?,
        _ => Value::object(),
    };
    let (mut added, mut kept) = (Vec::new(), Vec::new());
    merge_missing(
        &mut merged,
        fragment,
        &mut Vec::new(),
        &mut added,
        &mut kept,
    );
    let theme_path = theme.map(|_| dir.join("themes").join(IMPORTED_THEME_FILE));
    let mut backup = None;
    if !dry_run {
        if !added.is_empty() {
            if let Some(text) = original.as_deref().filter(|t| json::parse(t).is_err()) {
                let path = PathBuf::from(format!("{}{BACKUP_SUFFIX}", settings.display()));
                if !path.exists() {
                    fs::write(&path, text).map_err(io_error(&path))?;
                }
                backup = Some(path);
            }
            crate::layers::write_document(&settings, &merged).map_err(io_error(&settings))?;
        }
        if let (Some(path), Some(theme)) = (&theme_path, theme) {
            crate::layers::write_document(path, theme).map_err(io_error(path))?;
        }
    }
    Ok(Applied {
        settings,
        theme: theme_path,
        added,
        kept,
        backup,
        merged,
    })
}

fn at<'a>(doc: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(doc, |value, key| value.get(key))
}

fn text<'a>(doc: &'a Value, path: &[&str]) -> Option<&'a str> {
    at(doc, path).and_then(Value::as_str)
}

fn object(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
    )
}

// Zed takes one family; CSS stacks keep their first entry and system stacks become Zed's system font.
fn font_family(css: &str) -> Option<String> {
    let first = css
        .split(',')
        .next()?
        .trim()
        .trim_matches(|c| c == '"' || c == '\'');
    match first {
        "" => None,
        "-apple-system" | "system-ui" | "BlinkMacSystemFont" => Some(".SystemUIFont".into()),
        other => Some(other.to_owned()),
    }
}

fn color(value: Option<&Value>) -> Option<Value> {
    let hex = value?.as_str()?.trim().strip_prefix('#')?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let long = match hex.len() {
        3 | 4 => hex.chars().flat_map(|c| [c, c]).collect(),
        6 | 8 => hex.to_owned(),
        _ => return None,
    };
    Some(Value::String(format!("#{}", long.to_ascii_lowercase())))
}

// First VS Code key that is set wins for each Zed key.
const VSCODE_COLORS: &[(&str, &[&str])] = &[
    (
        "editor.background",
        &["editor.background", "editor.gutter.background"],
    ),
    ("editor.foreground", &["editor.foreground", "text"]),
    ("editorLineNumber.foreground", &["editor.line_number"]),
    (
        "editorLineNumber.activeForeground",
        &["editor.active_line_number"],
    ),
    (
        "editor.lineHighlightBackground",
        &["editor.active_line.background"],
    ),
    ("editorIndentGuide.background1", &["editor.indent_guide"]),
    ("editorIndentGuide.background", &["editor.indent_guide"]),
    (
        "editorIndentGuide.activeBackground1",
        &["editor.indent_guide_active"],
    ),
    (
        "editorIndentGuide.activeBackground",
        &["editor.indent_guide_active"],
    ),
    ("editorWhitespace.foreground", &["editor.invisible"]),
    (
        "editorBracketMatch.background",
        &["editor.document_highlight.bracket_background"],
    ),
    ("sideBar.background", &["panel.background", "background"]),
    ("panel.background", &["panel.background"]),
    ("editorGroupHeader.tabsBackground", &["tab_bar.background"]),
    ("tab.activeBackground", &["tab.active_background"]),
    ("tab.inactiveBackground", &["tab.inactive_background"]),
    ("titleBar.activeBackground", &["title_bar.background"]),
    ("statusBar.background", &["status_bar.background"]),
    ("editorWidget.background", &["elevated_surface.background"]),
    ("focusBorder", &["border.focused"]),
    ("editorGroup.border", &["border"]),
    ("panel.border", &["border"]),
    (
        "list.hoverBackground",
        &["element.hover", "ghost_element.hover"],
    ),
    (
        "list.activeSelectionBackground",
        &["element.selected", "ghost_element.selected"],
    ),
    (
        "scrollbarSlider.background",
        &["scrollbar.thumb.background"],
    ),
    (
        "scrollbarSlider.hoverBackground",
        &["scrollbar.thumb.hover_background"],
    ),
    ("descriptionForeground", &["text.muted"]),
    ("disabledForeground", &["text.disabled"]),
    ("textLink.foreground", &["text.accent"]),
    ("editorError.foreground", &["error"]),
    ("errorForeground", &["error"]),
    ("editorWarning.foreground", &["warning"]),
    ("editorInfo.foreground", &["info"]),
    ("editorHint.foreground", &["hint"]),
    ("gitDecoration.addedResourceForeground", &["created"]),
    ("gitDecoration.modifiedResourceForeground", &["modified"]),
    ("gitDecoration.deletedResourceForeground", &["deleted"]),
    ("gitDecoration.ignoredResourceForeground", &["ignored"]),
    ("gitDecoration.conflictingResourceForeground", &["conflict"]),
    ("editorGutter.addedBackground", &["version_control.added"]),
    (
        "editorGutter.modifiedBackground",
        &["version_control.modified"],
    ),
    (
        "editorGutter.deletedBackground",
        &["version_control.deleted"],
    ),
    ("terminal.background", &["terminal.background"]),
    ("terminal.foreground", &["terminal.foreground"]),
];

const ANSI: [(&str, &str); 16] = [
    ("Black", "black"),
    ("Red", "red"),
    ("Green", "green"),
    ("Yellow", "yellow"),
    ("Blue", "blue"),
    ("Magenta", "magenta"),
    ("Cyan", "cyan"),
    ("White", "white"),
    ("BrightBlack", "bright_black"),
    ("BrightRed", "bright_red"),
    ("BrightGreen", "bright_green"),
    ("BrightYellow", "bright_yellow"),
    ("BrightBlue", "bright_blue"),
    ("BrightMagenta", "bright_magenta"),
    ("BrightCyan", "bright_cyan"),
    ("BrightWhite", "bright_white"),
];

// TextMate scopes per Zed syntax key, most specific first.
const SYNTAX: &[(&str, &[&str])] = &[
    ("comment", &["comment", "punctuation.definition.comment"]),
    ("comment.doc", &["comment.block.documentation"]),
    ("string", &["string", "string.quoted"]),
    ("string.escape", &["constant.character.escape"]),
    ("string.regex", &["string.regexp"]),
    ("number", &["constant.numeric"]),
    (
        "boolean",
        &["constant.language.boolean", "constant.language"],
    ),
    (
        "constant",
        &["constant.language", "constant", "variable.other.constant"],
    ),
    (
        "keyword",
        &[
            "keyword",
            "keyword.control",
            "storage.type",
            "storage.modifier",
        ],
    ),
    ("operator", &["keyword.operator"]),
    (
        "function",
        &[
            "entity.name.function",
            "support.function",
            "meta.function-call",
        ],
    ),
    (
        "type",
        &["entity.name.type", "support.type", "entity.name.class"],
    ),
    ("constructor", &["entity.name.class", "entity.name.type"]),
    ("variable", &["variable", "variable.other"]),
    ("variable.special", &["variable.language"]),
    (
        "property",
        &[
            "variable.other.property",
            "support.type.property-name",
            "meta.object-literal.key",
        ],
    ),
    ("attribute", &["entity.other.attribute-name"]),
    ("tag", &["entity.name.tag"]),
    ("punctuation", &["punctuation"]),
    (
        "punctuation.delimiter",
        &["punctuation.separator", "punctuation.terminator"],
    ),
    ("label", &["entity.name.label"]),
    ("title", &["markup.heading", "entity.name.section"]),
    ("emphasis", &["markup.italic"]),
    ("emphasis.strong", &["markup.bold"]),
    ("link_uri", &["markup.underline.link"]),
    ("link_text", &["string.other.link"]),
    ("embedded", &["meta.embedded"]),
    ("preproc", &["meta.preprocessor"]),
];

fn rule_scopes(rule: &Value) -> Vec<String> {
    match rule.get("scope") {
        Some(Value::String(scopes)) => scopes
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_owned())
            .collect(),
        _ => Vec::new(),
    }
}

fn highlight(settings: &Value) -> Option<Value> {
    let mut style = Map::new();
    if let Some(color) = color(settings.get("foreground")) {
        style.insert("color".into(), color);
    }
    let font = settings
        .get("fontStyle")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if font.contains("italic") {
        style.insert("font_style".into(), "italic".into());
    }
    if font.contains("bold") {
        style.insert("font_weight".into(), Value::Number(Number::from(700_i64)));
    }
    (!style.is_empty()).then_some(Value::Object(style))
}

fn syntax(theme: &Value, style: &mut Map) {
    // Later rules win in VS Code, so the last rule naming a scope is the one that applies.
    let mut by_scope: Vec<(String, Value)> = Vec::new();
    for rule in theme
        .get("tokenColors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(settings) = rule.get("settings") else {
            continue;
        };
        let scopes = rule_scopes(rule);
        if scopes.is_empty() {
            if let Some(fg) = color(settings.get("foreground")) {
                style.entry("editor.foreground".to_owned()).or_insert(fg);
            }
            continue;
        }
        let Some(value) = highlight(settings) else {
            continue;
        };
        for scope in scopes {
            by_scope.retain(|(s, _)| *s != scope);
            by_scope.push((scope, value.clone()));
        }
    }
    let exact = |candidate: &str| {
        by_scope
            .iter()
            .find(|(s, _)| s == candidate)
            .map(|(_, v)| v.clone())
    };
    let nested = |candidate: &str| {
        let prefix = format!("{candidate}.");
        by_scope
            .iter()
            .filter(|(s, _)| s.starts_with(&prefix))
            .min_by_key(|(s, _)| s.len())
            .map(|(_, v)| v.clone())
    };
    let mut out = Map::new();
    for (key, candidates) in SYNTAX {
        let found = candidates
            .iter()
            .find_map(|c| exact(c))
            .or_else(|| candidates.iter().find_map(|c| nested(c)));
        if let Some(found) = found {
            out.insert((*key).to_owned(), found);
        }
    }
    if !out.is_empty() {
        style.insert("syntax".into(), Value::Object(out));
    }
}

/// A VS Code colour theme as Zed theme style; Zed fills anything missing from its base theme.
pub fn vscode_style(theme: &Value) -> Map {
    let mut style = Map::new();
    let colors = theme.get("colors");
    for (from, targets) in VSCODE_COLORS {
        if let Some(color) = color(colors.and_then(|c| c.get(from))) {
            for target in *targets {
                style
                    .entry((*target).to_owned())
                    .or_insert_with(|| color.clone());
            }
        }
    }
    for (vscode, zed) in ANSI {
        if let Some(color) = color(colors.and_then(|c| c.get(&format!("terminal.ansi{vscode}")))) {
            style.entry(format!("terminal.ansi.{zed}")).or_insert(color);
        }
    }
    let cursor = color(colors.and_then(|c| c.get("editorCursor.foreground")));
    let selection = color(colors.and_then(|c| c.get("editor.selectionBackground")));
    if cursor.is_some() || selection.is_some() {
        let mut player = Map::new();
        if let Some(cursor) = cursor {
            player.insert("cursor".into(), cursor.clone());
            player.insert("background".into(), cursor);
        }
        if let Some(selection) = selection {
            player.insert("selection".into(), selection);
        }
        style.insert("players".into(), Value::Array(vec![Value::Object(player)]));
    }
    syntax(theme, &mut style);
    style
}

/// Orca's terminal palette (`foreground`, `background`, `black`…`brightWhite`) as Zed terminal colours.
pub fn terminal_style(palette: &Value, style: &mut Map) {
    for (from, to) in [
        ("background", "terminal.background"),
        ("foreground", "terminal.foreground"),
    ] {
        if let Some(color) = color(palette.get(from)) {
            style.insert(to.into(), color);
        }
    }
    for (orca, zed) in ANSI {
        let key = format!("{}{}", orca[..1].to_ascii_lowercase(), &orca[1..]);
        if let Some(color) = color(palette.get(&key)) {
            style.insert(format!("terminal.ansi.{zed}"), color);
        }
    }
}

fn editor_theme(resolved: &Value, slot: &str, notes: &mut Vec<String>) -> Option<Value> {
    let file = text(resolved, &["theme", "editor", slot, "orcaFile"])?;
    let record = at(resolved, &["orca", "editorThemes", file])?;
    let parsed = record.get("theme").cloned().or_else(|| {
        record
            .get("text")
            .and_then(Value::as_str)
            .and_then(|t| json::parse(t).ok())
    });
    if parsed.is_none() {
        notes.push(format!(
            "theme.editor.{slot}: `{file}` is not valid JSON; Zed's default theme is used"
        ));
    }
    parsed.filter(Value::is_object)
}

fn terminal_palette(resolved: &Value, slot: &str, notes: &mut Vec<String>) -> Option<Value> {
    let separate = at(resolved, &["theme", "terminal", "separateLight"]).and_then(Value::as_bool);
    let key = if slot == "light" && separate == Some(false) {
        "dark"
    } else {
        slot
    };
    let name = text(resolved, &["theme", "terminal", key])?;
    let id = name.strip_prefix("custom:").unwrap_or(name);
    let custom = at(resolved, &["theme", "terminal", "custom"])
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|t| {
            t.get("id").and_then(Value::as_str) == Some(id)
                || t.get("name").and_then(Value::as_str) == Some(name)
        });
    match custom.and_then(|t| t.get("terminal")) {
        Some(palette) => Some(palette.clone()),
        None => {
            notes.push(format!(
                "theme.terminal.{key}: Orca's built-in terminal theme has no colours in the export; the Zed theme's terminal colours are used"
            ));
            None
        }
    }
}

pub fn render(resolved: &Value) -> Rendered {
    let mut notes = Vec::new();
    let mut settings = Map::new();

    let mode = text(resolved, &["theme", "mode"])
        .filter(|mode| matches!(*mode, "system" | "dark" | "light"))
        .unwrap_or("system");
    let mut themes = Vec::new();
    let mut selection = Map::new();
    selection.insert("mode".into(), mode.into());
    for slot in ["light", "dark"] {
        let editor = editor_theme(resolved, slot, &mut notes);
        let palette = terminal_palette(resolved, slot, &mut notes);
        let name = if editor.is_none() && palette.is_none() {
            if slot == "dark" {
                DEFAULT_DARK.to_owned()
            } else {
                DEFAULT_LIGHT.to_owned()
            }
        } else {
            let mut style = editor.as_ref().map(vscode_style).unwrap_or_default();
            if let Some(palette) = &palette {
                terminal_style(palette, &mut style);
            }
            let name = imported_theme_name(slot);
            themes.push(object([
                ("name", Value::from(name.as_str())),
                ("appearance", slot.into()),
                ("style", Value::Object(style)),
            ]));
            name
        };
        selection.insert(slot.into(), name.into());
    }
    settings.insert("theme".into(), Value::Object(selection));

    if let Some(family) = text(resolved, &["theme", "fonts", "ui"]).and_then(font_family) {
        settings.insert("ui_font_family".into(), family.into());
    }
    if let Some(family) = text(resolved, &["theme", "fonts", "editor"]).and_then(font_family) {
        settings.insert("buffer_font_family".into(), family.into());
    }
    let mut terminal = Map::new();
    if let Some(family) = text(resolved, &["theme", "fonts", "terminal"]).and_then(font_family) {
        terminal.insert("font_family".into(), family.into());
    }
    if let Some(size) =
        at(resolved, &["theme", "fonts", "terminalSize"]).filter(|v| v.as_number().is_some())
    {
        terminal.insert("font_size".into(), size.clone());
    }
    if let Some(height) =
        at(resolved, &["theme", "fonts", "terminalLineHeight"]).filter(|v| v.as_number().is_some())
    {
        terminal.insert("line_height".into(), object([("custom", height.clone())]));
    }
    if !terminal.is_empty() {
        settings.insert("terminal".into(), Value::Object(terminal));
    }
    if let Some(opacity) =
        at(resolved, &["theme", "inactivePaneOpacity"]).filter(|v| v.as_number().is_some())
    {
        settings.insert(
            "active_pane_modifiers".into(),
            object([("inactive_opacity", opacity.clone())]),
        );
    }

    let theme_family = (!themes.is_empty()).then(|| {
        object([
            ("$schema", ZED_THEME_SCHEMA.into()),
            ("name", "Icarus Imported".into()),
            ("author", "Icarus settings import".into()),
            ("themes", Value::Array(themes)),
        ])
    });
    Rendered {
        settings: Value::Object(settings),
        theme_family,
        notes,
    }
}
