//! VS Code and Warp colour themes as Zed themes in the app's themes directory (plan D10).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::json::{self, Map, Value};
use crate::zed::{self, ANSI, BACKUP_SUFFIX, ZED_THEME_SCHEMA, color, strip_jsonc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Vscode,
    Warp,
}

impl Source {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "vscode" | "vs-code" | "code" => Some(Self::Vscode),
            "warp" => Some(Self::Warp),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    #[error("not a valid VS Code theme (JSON with comments): {0}")]
    Vscode(String),
    #[error("not a valid Warp theme (YAML), line {line}: {message}")]
    Warp { line: usize, message: String },
    #[error("the theme has no colours Icarus can use")]
    Empty,
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is not valid JSON with comments; fix it and import again", path.display())]
    Settings { path: PathBuf },
}

#[derive(Debug, Clone)]
pub struct Converted {
    pub name: String,
    pub appearance: &'static str,
    /// A Zed theme family file holding the one converted theme.
    pub family: Value,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Installed {
    pub theme: PathBuf,
    /// The settings file, when the theme was also selected.
    pub settings: Option<PathBuf>,
    pub backup: Option<PathBuf>,
}

/// Converts `text`; `name` overrides the theme's own, `fallback_name` is used when it has none.
pub fn convert(
    source: Source,
    text: &str,
    name: Option<&str>,
    fallback_name: &str,
) -> Result<Converted, ThemeError> {
    let (own_name, appearance, style, notes) = match source {
        Source::Vscode => vscode(text)?,
        Source::Warp => warp(text)?,
    };
    if style.is_empty() {
        return Err(ThemeError::Empty);
    }
    let name = name
        .map(str::to_owned)
        .or(own_name)
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| fallback_name.to_owned());
    let mut theme = Map::new();
    theme.insert("name".into(), name.as_str().into());
    theme.insert("appearance".into(), appearance.into());
    theme.insert("style".into(), Value::Object(style));
    let mut family = Map::new();
    family.insert("$schema".into(), ZED_THEME_SCHEMA.into());
    family.insert("name".into(), name.as_str().into());
    family.insert("author".into(), "Icarus theme import".into());
    family.insert("themes".into(), Value::Array(vec![Value::Object(theme)]));
    Ok(Converted {
        name,
        appearance,
        family: Value::Object(family),
        notes,
    })
}

/// The file a converted theme is written to, so re-importing the same theme replaces it.
pub fn file_name(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    format!(
        "imported-{}.json",
        if slug.is_empty() { "theme" } else { slug }
    )
}

/// Writes the theme to `<dir>/themes` and, with `select`, makes it the theme for its appearance.
pub fn install(
    dir: &Path,
    converted: &Converted,
    select: bool,
    dry_run: bool,
) -> Result<Installed, ThemeError> {
    let theme = dir.join("themes").join(file_name(&converted.name));
    let io_error = |path: &Path| {
        let path = path.to_owned();
        move |source| ThemeError::Io { path, source }
    };
    if !dry_run {
        crate::layers::write_document(&theme, &converted.family).map_err(io_error(&theme))?;
    }
    if !select {
        return Ok(Installed {
            theme,
            settings: None,
            backup: None,
        });
    }
    let settings = dir.join("settings.json");
    let original = match fs::read_to_string(&settings) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(io_error(&settings)(err)),
    };
    let mut doc = match &original {
        Some(text) if !text.trim().is_empty() => json::parse(&strip_jsonc(text))
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| ThemeError::Settings {
                path: settings.clone(),
            })?,
        _ => Value::object(),
    };
    select_theme(&mut doc, &converted.name, converted.appearance);
    let mut backup = None;
    if !dry_run {
        if let Some(text) = original.as_deref().filter(|t| json::parse(t).is_err()) {
            let path = PathBuf::from(format!("{}{BACKUP_SUFFIX}", settings.display()));
            if !path.exists() {
                fs::write(&path, text).map_err(io_error(&path))?;
            }
            backup = Some(path);
        }
        crate::layers::write_document(&settings, &doc).map_err(io_error(&settings))?;
    }
    Ok(Installed {
        theme,
        settings: Some(settings),
        backup,
    })
}

/// Puts `name` in the slot for `appearance`, keeping the mode and the other slot.
fn select_theme(doc: &mut Value, name: &str, appearance: &str) {
    let Some(settings) = doc.as_object_mut() else {
        return;
    };
    let current = settings.get("theme").cloned();
    let mut selection = match current {
        Some(Value::Object(map)) => map,
        Some(Value::String(previous)) => {
            let mut map = Map::new();
            map.insert("mode".into(), "system".into());
            map.insert("light".into(), previous.as_str().into());
            map.insert("dark".into(), previous.as_str().into());
            map
        }
        _ => {
            let mut map = Map::new();
            map.insert("mode".into(), "system".into());
            map.insert("light".into(), "One Light".into());
            map.insert("dark".into(), "One Dark".into());
            map
        }
    };
    selection.insert(appearance.into(), name.into());
    settings.insert("theme".into(), Value::Object(selection));
}

type Parts = (Option<String>, &'static str, Map, Vec<String>);

fn vscode(text: &str) -> Result<Parts, ThemeError> {
    let theme =
        json::parse(&strip_jsonc(text)).map_err(|error| ThemeError::Vscode(error.to_string()))?;
    if !theme.is_object() {
        return Err(ThemeError::Vscode("expected an object".into()));
    }
    let mut notes = Vec::new();
    if theme.get("include").is_some() {
        notes.push(
            "the theme includes another file; only its own colours were converted".to_owned(),
        );
    }
    let appearance = match theme.get("type").and_then(Value::as_str) {
        Some("light" | "hcLight") => "light",
        _ => "dark",
    };
    let name = theme.get("name").and_then(Value::as_str).map(str::to_owned);
    Ok((name, appearance, zed::vscode_style(&theme), notes))
}

// Zed keys that take each Warp colour.
const WARP_BACKGROUND: &[&str] = &[
    "background",
    "editor.background",
    "editor.gutter.background",
    "panel.background",
    "tab_bar.background",
    "tab.active_background",
    "title_bar.background",
    "status_bar.background",
    "terminal.background",
];
const WARP_FOREGROUND: &[&str] = &["text", "editor.foreground", "terminal.foreground"];
const WARP_ACCENT: &[&str] = &["border.focused", "text.accent", "icon.accent"];

fn warp(text: &str) -> Result<Parts, ThemeError> {
    let doc = parse_yaml(text)?;
    let mut style = Map::new();
    let mut notes = Vec::new();
    let mut put = |keys: &[&str], value: Option<Value>| {
        if let Some(value) = value {
            for key in keys {
                style.insert((*key).to_owned(), value.clone());
            }
        }
    };
    put(
        WARP_BACKGROUND,
        warp_color(doc.get("background"), &mut notes),
    );
    put(
        WARP_FOREGROUND,
        warp_color(doc.get("foreground"), &mut notes),
    );
    let accent = warp_color(doc.get("accent"), &mut notes);
    put(WARP_ACCENT, accent.clone());
    if let Some(cursor) = accent {
        let mut player = Map::new();
        player.insert("cursor".into(), cursor.clone());
        player.insert("background".into(), cursor);
        style.insert("players".into(), Value::Array(vec![Value::Object(player)]));
    }
    let colors = doc.get("terminal_colors");
    for (group, prefix) in [("normal", ""), ("bright", "bright_")] {
        let Some(palette) = colors.and_then(|c| c.get(group)) else {
            continue;
        };
        for (_, zed_name) in ANSI.iter().take(8) {
            if let Some(color) = color(palette.get(zed_name)) {
                style.insert(format!("terminal.ansi.{prefix}{zed_name}"), color);
            }
        }
    }
    let appearance = match doc.get("details").and_then(Value::as_str) {
        Some("lighter") => "light",
        _ => "dark",
    };
    let name = doc.get("name").and_then(Value::as_str).map(str::to_owned);
    Ok((name, appearance, style, notes))
}

/// A colour, or the first stop of a Warp gradient (`{left, right}` / `{top, bottom}`).
fn warp_color(value: Option<&Value>, notes: &mut Vec<String>) -> Option<Value> {
    match value? {
        Value::Object(stops) => {
            let first = ["left", "top"].iter().find_map(|k| stops.get(*k));
            if first.is_some() {
                notes.push("a gradient was imported as its first colour".to_owned());
            }
            color(first)
        }
        other => color(Some(other)),
    }
}

/// The block-mapping subset of YAML that Warp themes use: nested `key: value` pairs and comments.
pub(crate) fn parse_yaml(text: &str) -> Result<Value, ThemeError> {
    let mut root = Map::new();
    // Open mappings as (indent of their key, key path); the root sits below every indent.
    let mut stack: Vec<(Option<usize>, Vec<String>)> = vec![(None, Vec::new())];
    for (index, raw) in text.lines().enumerate() {
        let error = |message: &str| ThemeError::Warp {
            line: index + 1,
            message: message.to_owned(),
        };
        let line = strip_yaml_comment(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "---" {
            continue;
        }
        if trimmed.starts_with("- ") || trimmed == "-" {
            return Err(error("lists are not supported"));
        }
        let indent = line.len() - line.trim_start().len();
        while stack
            .last()
            .is_some_and(|(at, _)| at.is_some_and(|at| at >= indent))
        {
            stack.pop();
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(error("expected `key: value`"));
        };
        let key = unquote(key).to_owned();
        let value = value.trim();
        let path = stack.last().map(|(_, p)| p.clone()).unwrap_or_default();
        let parent = descend(&mut root, &path).ok_or_else(|| error("bad nesting"))?;
        if value.is_empty() {
            parent.insert(key.clone(), Value::object());
            let mut child = path;
            child.push(key);
            stack.push((Some(indent), child));
        } else {
            parent.insert(key, unquote(value).into());
        }
    }
    Ok(Value::Object(root))
}

fn descend<'a>(root: &'a mut Map, path: &[String]) -> Option<&'a mut Map> {
    let mut current = root;
    for key in path {
        current = current.get_mut(key)?.as_object_mut()?;
    }
    Some(current)
}

fn strip_yaml_comment(line: &str) -> &str {
    let mut quote = None;
    for (index, c) in line.char_indices() {
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, '#') if index == 0 || line[..index].ends_with(char::is_whitespace) => {
                return &line[..index];
            }
            _ => {}
        }
    }
    line
}

fn unquote(text: &str) -> &str {
    let text = text.trim();
    for q in ['"', '\''] {
        if let Some(inner) = text.strip_prefix(q).and_then(|t| t.strip_suffix(q)) {
            return inner;
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_nesting_and_comments() {
        let doc = parse_yaml(
            "# Warp theme\nname: 'Sea'\naccent: \"#00aaff\" # blue\nterminal_colors:\n  normal:\n    black: '#000000'\n  bright:\n    black: '#555555'\ndetails: darker\n",
        )
        .expect("parse");
        assert_eq!(doc.get("name").and_then(Value::as_str), Some("Sea"));
        assert_eq!(doc.get("accent").and_then(Value::as_str), Some("#00aaff"));
        assert_eq!(
            doc.get("terminal_colors")
                .and_then(|c| c.get("bright"))
                .and_then(|c| c.get("black"))
                .and_then(Value::as_str),
            Some("#555555")
        );
        assert_eq!(doc.get("details").and_then(Value::as_str), Some("darker"));
    }

    #[test]
    fn yaml_lists_are_refused_with_a_line() {
        let error = parse_yaml("name: x\ncolors:\n  - '#fff'\n").expect_err("list");
        assert!(matches!(error, ThemeError::Warp { line: 3, .. }), "{error}");
    }

    #[test]
    fn file_names_are_stable_slugs() {
        assert_eq!(file_name("Solarized Dark+"), "imported-solarized-dark.json");
        assert_eq!(file_name("!!!"), "imported-theme.json");
    }
}
