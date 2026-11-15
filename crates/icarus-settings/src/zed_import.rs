//! A normal Zed install's settings, keymap, snippets, themes and extensions, brought into the
//! app's own Zed config directory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::json::{self, Map, Value};
use crate::layers::{self, KeyPath};
use crate::zed::{self, ApplyError, IMPORTED_THEME_FILE, strip_jsonc};

/// Settings Icarus relies on; the Zed install's values for them are never imported.
pub const PROTECTED_KEYS: &[&str] = &[
    "agent.dock",
    "agent.enabled",
    "disable_ai",
    "agent_servers.Icarus",
];

/// The look reported back so the user sees what carried over.
pub const APPEARANCE_KEYS: &[&str] = &[
    "theme",
    "icon_theme",
    "ui_font_family",
    "ui_font_size",
    "buffer_font_family",
    "buffer_font_size",
];

const AUTO_INSTALL: &str = "auto_install_extensions";

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// The normal Zed install's config directory; `ICARUS_ZED_SOURCE_DIR` overrides it.
pub fn source_config_dir() -> Option<PathBuf> {
    if let Some(dir) = env_dir("ICARUS_ZED_SOURCE_DIR") {
        return Some(dir);
    }
    if cfg!(target_os = "windows") {
        return env_dir("APPDATA").map(|dir| dir.join("Zed"));
    }
    let home = env_dir("HOME")?;
    let base = if cfg!(target_os = "macos") {
        home.join(".config")
    } else {
        env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"))
    };
    Some(base.join("zed"))
}

/// The normal Zed install's data directory (it holds `extensions/`); `ICARUS_ZED_SOURCE_DATA_DIR`
/// overrides it.
pub fn source_data_dir() -> Option<PathBuf> {
    if let Some(dir) = env_dir("ICARUS_ZED_SOURCE_DATA_DIR") {
        return Some(dir);
    }
    if cfg!(target_os = "windows") {
        return env_dir("LOCALAPPDATA").map(|dir| dir.join("Zed"));
    }
    let home = env_dir("HOME")?;
    if cfg!(target_os = "macos") {
        return Some(home.join("Library/Application Support/Zed"));
    }
    let base = env_dir("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
    Some(base.join("zed"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Extension {
    pub id: String,
    pub name: Option<String>,
    pub version: Option<String>,
}

/// An extension Zed loads from a local checkout; it can't be downloaded, so it isn't imported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DevExtension {
    pub id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Copied,
    WouldCopy,
    AlreadyImported,
    /// The app already has its own files of this kind.
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileOutcome {
    /// `keymap`, `snippet` or `theme`.
    pub kind: &'static str,
    pub name: String,
    pub to: PathBuf,
    pub status: FileStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ZedImport {
    pub source: PathBuf,
    pub source_data: Option<PathBuf>,
    pub target: PathBuf,
    pub dry_run: bool,
    pub settings_found: bool,
    pub settings: PathBuf,
    /// Settings keys added to the app's `settings.json` (extension ids excluded).
    pub added: Vec<String>,
    /// Keys the app's `settings.json` already set differently; its values stay.
    pub kept: Vec<String>,
    /// Keys the Zed install sets that Icarus needs to own.
    pub protected: Vec<String>,
    /// The Zed install's theme and font settings.
    pub appearance: Map,
    pub extensions: Vec<Extension>,
    /// Extension ids newly added to `auto_install_extensions`; the app downloads them itself.
    pub extensions_added: Vec<String>,
    pub dev_extensions: Vec<DevExtension>,
    pub files: Vec<FileOutcome>,
    pub backup: Option<PathBuf>,
    pub notes: Vec<String>,
}

impl ZedImport {
    /// Whether the Zed install has anything this import could bring over.
    pub fn found_anything(&self) -> bool {
        self.settings_found
            || !self.extensions.is_empty()
            || !self.dev_extensions.is_empty()
            || !self.files.is_empty()
    }

    /// Whether running the import (again) changes anything.
    pub fn changes_anything(&self) -> bool {
        !self.added.is_empty()
            || !self.extensions_added.is_empty()
            || self
                .files
                .iter()
                .any(|file| matches!(file.status, FileStatus::Copied | FileStatus::WouldCopy))
    }

    /// One line for a prompt: "settings, 3 extensions, theme".
    pub fn headline(&self) -> String {
        let mut parts = Vec::new();
        if self.settings_found {
            parts.push("settings".to_owned());
        }
        match self.extensions.len() {
            0 => {}
            1 => parts.push("1 extension".to_owned()),
            count => parts.push(format!("{count} extensions")),
        }
        if self.appearance.contains_key("theme") {
            parts.push("theme".to_owned());
        }
        if self.files.iter().any(|file| file.kind == "keymap") {
            parts.push("keymap".to_owned());
        }
        if self.files.iter().any(|file| file.kind == "snippet") {
            parts.push("snippets".to_owned());
        }
        parts.join(", ")
    }

    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let would = if self.dry_run { "would be " } else { "" };
        if !self.found_anything() {
            lines.push(format!("nothing to import from {}", self.source.display()));
            return lines;
        }
        if self.settings_found {
            lines.push(format!(
                "settings {}: {} keys {would}added, {} already set and kept",
                self.settings.display(),
                self.added.len(),
                self.kept.len()
            ));
            for key in &self.added {
                lines.push(format!("  add: {key}"));
            }
            for key in &self.kept {
                lines.push(format!("  kept yours: {key}"));
            }
            for key in &self.protected {
                lines.push(format!("  not imported (Icarus needs it): {key}"));
            }
        }
        for (key, value) in &self.appearance {
            lines.push(format!("appearance: {key} = {value}"));
        }
        if !self.extensions.is_empty() {
            lines.push(format!(
                "extensions: {} installed in Zed, {} {would}added to auto_install_extensions (the app downloads them on its next start)",
                self.extensions.len(),
                self.extensions_added.len()
            ));
            let ids: Vec<&str> = self.extensions.iter().map(|ext| ext.id.as_str()).collect();
            lines.push(format!("  {}", ids.join(", ")));
        }
        for dev in &self.dev_extensions {
            lines.push(format!(
                "dev extension not imported: {} ({}); install it with `zed: install dev extension`",
                dev.id,
                dev.path.display()
            ));
        }
        for file in &self.files {
            let status = match file.status {
                FileStatus::Copied => "copied",
                FileStatus::WouldCopy => "would copy",
                FileStatus::AlreadyImported => "already imported",
                FileStatus::Skipped => "skipped, Icarus has its own",
            };
            lines.push(format!("{} {}: {status}", file.kind, file.name));
        }
        if let Some(backup) = &self.backup {
            lines.push(format!(
                "your commented settings.json is saved in {}",
                backup.display()
            ));
        }
        for note in &self.notes {
            lines.push(format!("note: {note}"));
        }
        if self.dry_run {
            lines.push("dry run: nothing written".to_owned());
        } else if !self.changes_anything() {
            lines.push("already up to date".to_owned());
        }
        lines
    }
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> ApplyError {
    let path = path.to_owned();
    move |source| ApplyError::Io { path, source }
}

fn read_optional(path: &Path) -> Result<Option<String>, ApplyError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(err) => Err(io_error(path)(err)),
    }
}

fn parse_jsonc(text: &str) -> Option<Value> {
    json::parse(&strip_jsonc(text)).ok()
}

fn read_settings(path: &Path) -> Result<Option<Value>, ApplyError> {
    match read_optional(path)? {
        None => Ok(None),
        Some(text) if text.trim().is_empty() => Ok(Some(Value::object())),
        Some(text) => parse_jsonc(&text)
            .filter(Value::is_object)
            .map(Some)
            .ok_or_else(|| ApplyError::Parse {
                path: path.to_owned(),
            }),
    }
}

// Dropping a protected key can leave `"agent": {}` behind, which would only add noise.
fn unset_and_prune(doc: &mut Value, segments: &[&str]) -> bool {
    let path = KeyPath::from_segments(segments.iter().copied());
    if layers::unset(doc, &path).is_none() {
        return false;
    }
    for depth in (1..segments.len()).rev() {
        let parent = KeyPath::from_segments(segments[..depth].iter().copied());
        if layers::get(doc, &parent)
            .and_then(Value::as_object)
            .is_some_and(Map::is_empty)
        {
            layers::unset(doc, &parent);
        }
    }
    true
}

// Top-level `key = "value"` pairs of an `extension.toml`, without pulling in a TOML parser.
fn toml_strings(text: &str) -> Map {
    let mut out = Map::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            break;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if let Some(inner) = value
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
        {
            out.insert(key.trim().to_owned(), Value::from(inner));
        }
    }
    out
}

fn read_manifest(dir: &Path) -> Map {
    if let Ok(Some(text)) = read_optional(&dir.join("extension.toml")) {
        return toml_strings(&text);
    }
    match read_optional(&dir.join("extension.json")) {
        Ok(Some(text)) => json::parse(&text)
            .ok()
            .and_then(|doc| doc.as_object().cloned())
            .unwrap_or_default(),
        _ => Map::new(),
    }
}

fn manifest_extension(fallback_id: &str, manifest: Option<&Map>) -> Extension {
    let text = |key: &str| {
        manifest
            .and_then(|m| m.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    Extension {
        id: text("id").unwrap_or_else(|| fallback_id.to_owned()),
        name: text("name"),
        version: text("version"),
    }
}

fn scan_extensions(
    data: &Path,
    notes: &mut Vec<String>,
) -> Result<(Vec<Extension>, Vec<DevExtension>), ApplyError> {
    let root = data.join("extensions");
    let index = match read_optional(&root.join("index.json"))? {
        Some(text) => {
            let parsed = json::parse(&text).ok();
            if parsed.is_none() {
                notes.push(format!(
                    "{} is not valid JSON; only the installed folders were read",
                    root.join("index.json").display()
                ));
            }
            parsed
        }
        None => None,
    };
    let indexed = index
        .as_ref()
        .and_then(|doc| doc.get("extensions"))
        .and_then(Value::as_object);
    let index_entry = |id: &str| indexed.and_then(|map| map.get(id));
    let is_dev = |id: &str| {
        index_entry(id)
            .and_then(|entry| entry.get("dev"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };

    let mut extensions = Vec::new();
    let mut dev = Vec::new();
    let installed = root.join("installed");
    match fs::read_dir(&installed) {
        Ok(entries) => {
            let mut entries: Vec<_> = entries
                .collect::<Result<_, _>>()
                .map_err(io_error(&installed))?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let dir_name = entry.file_name().to_string_lossy().into_owned();
                if dir_name.starts_with('.') {
                    continue;
                }
                let path = entry.path();
                let file_type = entry.file_type().map_err(io_error(&path))?;
                if file_type.is_symlink() {
                    let manifest = read_manifest(&path);
                    let id = manifest_extension(&dir_name, Some(&manifest)).id;
                    let target = fs::read_link(&path).map_err(io_error(&path))?;
                    dev.push(DevExtension { id, path: target });
                } else if file_type.is_dir() {
                    let manifest = read_manifest(&path);
                    let extension = manifest_extension(&dir_name, Some(&manifest));
                    if is_dev(&extension.id) {
                        dev.push(DevExtension {
                            id: extension.id,
                            path,
                        });
                    } else {
                        extensions.push(extension);
                    }
                }
            }
        }
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            for (id, entry) in indexed.into_iter().flatten() {
                if is_dev(id) {
                    dev.push(DevExtension {
                        id: id.clone(),
                        path: installed.join(id),
                    });
                } else {
                    let manifest = entry.get("manifest").and_then(Value::as_object);
                    extensions.push(manifest_extension(id, manifest));
                }
            }
        }
        Err(err) => return Err(io_error(&installed)(err)),
    }
    extensions.sort_by(|a, b| a.id.cmp(&b.id));
    extensions.dedup_by(|a, b| a.id == b.id);
    Ok((extensions, dev))
}

fn list_files(dir: &Path) -> Result<Vec<(String, PathBuf)>, ApplyError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(Vec::new());
        }
        Err(err) => return Err(io_error(dir)(err)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_error(dir))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if name.starts_with('.') || !path.is_file() {
            continue;
        }
        files.push((name, path));
    }
    files.sort();
    Ok(files)
}

// Zed writes a keymap template whose bindings are all commented out; that isn't a keymap of one's own.
fn keymap_is_empty(text: &str) -> bool {
    match parse_jsonc(text) {
        Some(Value::Array(sections)) => sections.iter().all(|section| {
            section
                .get("bindings")
                .and_then(Value::as_object)
                .is_none_or(Map::is_empty)
        }),
        _ => text.trim().is_empty(),
    }
}

fn same_content(a: &Path, b: &Path) -> bool {
    matches!((fs::read(a), fs::read(b)), (Ok(a), Ok(b)) if a == b)
}

/// Copies `sources` into `target_dir` unless the app already has files of its own there; files a
/// previous import copied don't count as its own.
fn copy_files(
    kind: &'static str,
    sources: Vec<(String, PathBuf)>,
    target_dir: &Path,
    existing: Vec<(String, PathBuf)>,
    not_own: impl Fn(&str, &Path) -> bool,
    dry_run: bool,
) -> Result<Vec<FileOutcome>, ApplyError> {
    let has_own = existing.iter().any(|(name, path)| {
        let imported = sources
            .iter()
            .any(|(source_name, source)| source_name == name && same_content(source, path));
        !imported && !not_own(name, path)
    });
    let mut outcomes = Vec::new();
    for (name, source) in sources {
        let to = target_dir.join(&name);
        let status = if same_content(&source, &to) {
            FileStatus::AlreadyImported
        } else if has_own {
            FileStatus::Skipped
        } else if dry_run {
            FileStatus::WouldCopy
        } else {
            fs::create_dir_all(target_dir).map_err(io_error(target_dir))?;
            fs::copy(&source, &to).map_err(io_error(&to))?;
            FileStatus::Copied
        };
        outcomes.push(FileOutcome {
            kind,
            name,
            to,
            status,
        });
    }
    Ok(outcomes)
}

fn keymap_files(dir: &Path) -> Result<Vec<(String, PathBuf)>, ApplyError> {
    let path = dir.join("keymap.json");
    Ok(match read_optional(&path)? {
        Some(text) if !keymap_is_empty(&text) => vec![("keymap.json".to_owned(), path)],
        _ => Vec::new(),
    })
}

/// Imports the Zed install at `source` (config) and `source_data` (data, for extensions) into the
/// app's config directory `target`; with `dry_run` nothing is written. Running it again changes
/// nothing.
pub fn import(
    source: &Path,
    source_data: Option<&Path>,
    target: &Path,
    dry_run: bool,
) -> Result<ZedImport, ApplyError> {
    let mut notes = Vec::new();
    let zed_settings = read_settings(&source.join("settings.json"))?;
    let settings_found = zed_settings.is_some();
    let mut fragment = zed_settings.unwrap_or_else(Value::object);

    let mut protected = Vec::new();
    for key in PROTECTED_KEYS {
        let segments: Vec<&str> = key.split('.').collect();
        if unset_and_prune(&mut fragment, &segments) {
            protected.push((*key).to_owned());
        }
    }

    let mut appearance = Map::new();
    for key in APPEARANCE_KEYS {
        if let Some(value) = fragment.get(key) {
            appearance.insert((*key).to_owned(), value.clone());
        }
    }

    let (extensions, dev_extensions) = match source_data {
        Some(data) => scan_extensions(data, &mut notes)?,
        None => (Vec::new(), Vec::new()),
    };
    if !extensions.is_empty() {
        let map = fragment.as_object_mut().map(|map| {
            map.entry(AUTO_INSTALL.to_owned())
                .or_insert_with(Value::object)
        });
        match map.and_then(Value::as_object_mut) {
            Some(auto_install) => {
                for extension in &extensions {
                    auto_install
                        .entry(extension.id.clone())
                        .or_insert(Value::Bool(true));
                }
            }
            None => notes.push(format!(
                "the Zed settings' {AUTO_INSTALL} is not an object; extensions were not added"
            )),
        }
    }

    let applied = zed::apply(target, &fragment, None, dry_run)?;
    let mut extensions_added = Vec::new();
    let mut added = Vec::new();
    for key in applied.added {
        if key == AUTO_INSTALL {
            let ids = fragment
                .get(AUTO_INSTALL)
                .and_then(Value::as_object)
                .into_iter()
                .flatten();
            extensions_added.extend(ids.map(|(id, _)| id.clone()));
        } else if let Some(id) = key.strip_prefix(&format!("{AUTO_INSTALL}.")) {
            extensions_added.push(id.to_owned());
        } else {
            added.push(key);
        }
    }

    let mut files = copy_files(
        "keymap",
        keymap_files(source)?,
        target,
        keymap_files(target)?,
        |_, _| false,
        dry_run,
    )?;
    files.extend(copy_files(
        "snippet",
        list_files(&source.join("snippets"))?,
        &target.join("snippets"),
        list_files(&target.join("snippets"))?,
        |_, _| false,
        dry_run,
    )?);
    // The Orca import's generated theme is Icarus's, not a theme the user made.
    files.extend(copy_files(
        "theme",
        list_files(&source.join("themes"))?,
        &target.join("themes"),
        list_files(&target.join("themes"))?,
        |name, _| name == IMPORTED_THEME_FILE,
        dry_run,
    )?);

    Ok(ZedImport {
        source: source.to_owned(),
        source_data: source_data.map(Path::to_owned),
        target: target.to_owned(),
        dry_run,
        settings_found,
        settings: applied.settings,
        added,
        kept: applied.kept,
        protected,
        appearance,
        extensions,
        extensions_added,
        dev_extensions,
        files,
        backup: applied.backup,
        notes,
    })
}
