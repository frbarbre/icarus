use std::collections::HashSet;

use indexmap::IndexMap;

use crate::json::{self, Map, Value};
use crate::layers::{self, KeyPath, ValidationError};
use crate::orca::{
    EDITOR_THEME_FILE_NAMES, FORMAT, OrcaExport, OrcaParseError, PORTABLE_REPO_FIELDS, VERSION,
};
use crate::tooling;

const SETTINGS_MAP: [(&str, &str); 13] = [
    ("terminalQuickCommands", "quickCommands"),
    ("openClaudeTabsInWebView", "agents.remote_control"),
    ("terminalInactivePaneOpacity", "theme.inactivePaneOpacity"),
    ("theme", "theme.mode"),
    ("terminalThemeDark", "theme.terminal.dark"),
    ("terminalThemeLight", "theme.terminal.light"),
    (
        "terminalUseSeparateLightTheme",
        "theme.terminal.separateLight",
    ),
    ("terminalCustomThemes", "theme.terminal.custom"),
    ("appFontFamily", "theme.fonts.ui"),
    ("editorFontFamily", "theme.fonts.editor"),
    ("terminalFontFamily", "theme.fonts.terminal"),
    ("terminalFontSize", "theme.fonts.terminalSize"),
    ("terminalLineHeight", "theme.fonts.terminalLineHeight"),
];

const TRANSLATED_SETTINGS: [&str; 3] = ["languageServers", "formatters", "linters"];

const UI_MAP: [(&str, &str); 13] = [
    ("workspaceStatuses", "workspace.statuses"),
    ("workspaceStatusRules", "rules.pr"),
    ("worktreeCardProperties", "workspace.cardFields"),
    ("workspaceBoardOpacity", "workspace.boardOpacity"),
    ("workspaceBoardColumnWidth", "workspace.boardColumnWidth"),
    ("groupBy", "sidebar.groupBy"),
    ("sortBy", "sidebar.sortBy"),
    ("projectOrderBy", "sidebar.projectOrderBy"),
    ("stackOrder", "sidebar.stackOrder"),
    ("statusBarItems", "statusBar.items"),
    ("statusBarVisible", "statusBar.visible"),
    ("statusBarUsageMode", "statusBar.usageMode"),
    ("usagePercentageDisplay", "statusBar.usagePercentageDisplay"),
];

const ENVELOPE_ORDER: [&str; 8] = [
    "format",
    "version",
    "exportedAt",
    "settings",
    "ui",
    "projects",
    "keybindings",
    "editorThemes",
];

const PROJECT_ENTRY_ORDER: [&str; 3] = ["remote", "name", "settings"];

const PORTS_LABELS_COMMAND: &str = "ports.labels_command";

fn project_field_path(field: &str) -> String {
    match field {
        "hookSettings" => "project.hooks".to_owned(),
        other => format!("project.{other}"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportReport {
    pub mapped: Vec<Mapping>,
    pub unmapped: Vec<String>,
    pub notes: Vec<String>,
    pub validation: Vec<ValidationError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrcaImport {
    pub user: Value,
    pub projects: IndexMap<String, Value>,
    /// Zed user settings: appearance, extensions, `languages` and `lsp`.
    pub zed: Value,
    /// A Zed theme family made from Orca's editor and terminal colours, if it had any.
    pub zed_theme: Option<Value>,
    pub report: ImportReport,
}

fn path(key: &str) -> KeyPath {
    KeyPath::from_segments(key.split('.'))
}

// Mapping targets only ever pass through objects this module creates, so `set` cannot fail.
fn put(doc: &mut Value, key: &str, value: Value) {
    let _ = layers::set(doc, &path(key), value);
}

fn strings(values: impl IntoIterator<Item = impl Into<String>>) -> Value {
    Value::Array(
        values
            .into_iter()
            .map(|v| Value::String(v.into()))
            .collect(),
    )
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

// Embedded files (keybindings, themes) are JSON text; keep the parsed value plus how it was laid out.
fn embed(text: &str) -> (Option<Value>, Value) {
    let mut record = Map::new();
    if let Ok(parsed) = json::parse(text) {
        let pretty = parsed.to_pretty_string();
        let format = if parsed.to_compact_string() == text {
            Some("compact")
        } else if pretty == text {
            Some("pretty")
        } else if format!("{pretty}\n") == text {
            Some("pretty-newline")
        } else {
            None
        };
        if let Some(format) = format {
            record.insert("format".into(), format.into());
            return (Some(parsed), Value::Object(record));
        }
    }
    record.insert("text".into(), text.into());
    (None, Value::Object(record))
}

fn unembed(value: Option<&Value>, record: Option<&Value>) -> Option<String> {
    let format = record.and_then(|r| r.get("format")).and_then(Value::as_str);
    match value {
        Some(value) => Some(match format {
            Some("compact") => value.to_compact_string(),
            Some("pretty") => value.to_pretty_string(),
            _ => format!("{}\n", value.to_pretty_string()),
        }),
        None => record
            .and_then(|r| r.get("text"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

struct Importer {
    user: Value,
    zed: Value,
    carry: Map,
    projects: IndexMap<String, Value>,
    report: ImportReport,
}

impl Importer {
    fn map_section(
        &mut self,
        section: &str,
        source: &Map,
        table: &[(&str, &str)],
    ) -> (Vec<String>, Map) {
        let mut order = Vec::new();
        let mut rest = Map::new();
        for (key, value) in source {
            order.push(key.clone());
            match table.iter().find(|(from, _)| from == key) {
                Some((_, to)) => {
                    put(&mut self.user, to, value.clone());
                    self.report.mapped.push(Mapping {
                        from: format!("{section}.{key}"),
                        to: (*to).to_owned(),
                    });
                }
                None => {
                    rest.insert(key.clone(), value.clone());
                    if !(section == "settings" && TRANSLATED_SETTINGS.contains(&key.as_str())) {
                        self.report.unmapped.push(format!("{section}.{key}"));
                    }
                }
            }
        }
        (order, rest)
    }

    // Orca's tooling choices become Zed-style settings; the originals stay carried for export.
    fn language_tooling(&mut self, settings: &Map) {
        let tooling = tooling::translate(settings);
        self.zed = Value::Object(tooling.zed);
        self.report
            .mapped
            .extend(tooling.mapped.into_iter().map(|(from, to)| Mapping {
                from,
                to: format!("zed: {to}"),
            }));
        self.report.notes.extend(tooling.notes);
        if settings
            .get("openClaudeTabsInWebView")
            .and_then(Value::as_bool)
            == Some(true)
        {
            self.report.notes.push(
                "settings.openClaudeTabsInWebView: new Claude Code sessions start with Remote Control (`agents.remote_control`), so they can be followed on claude.ai".into(),
            );
        }
    }

    fn project_entry(&mut self, index: usize, entry: &Value, seen: &mut HashSet<String>) -> Value {
        let usable = entry.as_object().and_then(|map| {
            let remote = map.get("remote")?.as_str()?;
            let settings = map.get("settings")?.as_object()?;
            Some((map, remote, settings))
        });
        let Some((map, remote, settings)) = usable.filter(|(_, r, _)| seen.insert((*r).to_owned()))
        else {
            self.report.unmapped.push(format!("projects.{index}"));
            self.report.notes.push(format!(
                "projects.{index} has no usable remote and settings object, or repeats a remote; kept verbatim"
            ));
            let mut raw = Map::new();
            raw.insert("raw".into(), entry.clone());
            return Value::Object(raw);
        };

        let mut doc = Value::object();
        let mut field_order = Vec::new();
        let mut rest = Map::new();
        for (field, value) in settings {
            field_order.push(field.clone());
            if PORTABLE_REPO_FIELDS.contains(&field.as_str()) {
                let to = project_field_path(field);
                put(&mut doc, &to, value.clone());
                self.report.mapped.push(Mapping {
                    from: format!("projects[{remote}].settings.{field}"),
                    to: format!("{remote}: {to}"),
                });
                if field == "portLabelsCommand" {
                    put(&mut doc, PORTS_LABELS_COMMAND, value.clone());
                    self.report.mapped.push(Mapping {
                        from: format!("projects[{remote}].settings.{field}"),
                        to: format!("{remote}: {PORTS_LABELS_COMMAND}"),
                    });
                }
            } else {
                rest.insert(field.clone(), value.clone());
                self.report
                    .unmapped
                    .push(format!("projects[{remote}].settings.{field}"));
            }
        }
        let mut project_carry = Map::new();
        project_carry.insert("entryOrder".into(), strings(map.keys().cloned()));
        if let Some(name) = map.get("name") {
            project_carry.insert("name".into(), name.clone());
        }
        let extra: Map = map
            .iter()
            .filter(|(k, _)| !PROJECT_ENTRY_ORDER.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !extra.is_empty() {
            for key in extra.keys() {
                self.report
                    .unmapped
                    .push(format!("projects[{remote}].{key}"));
            }
            project_carry.insert("entryExtra".into(), Value::Object(extra));
        }
        project_carry.insert("settingsOrder".into(), strings(field_order));
        project_carry.insert("settings".into(), Value::Object(rest));
        put(&mut doc, "orca.project", Value::Object(project_carry));
        self.projects.insert(remote.to_owned(), doc);
        Value::String(remote.to_owned())
    }

    fn keybindings(&mut self, value: &Value) -> Option<Value> {
        let Value::String(text) = value else {
            return None;
        };
        let (parsed, record) = embed(text);
        match parsed {
            Some(parsed) => {
                let commands = json::from_value::<crate::orca::KeybindingsFile>(&parsed)
                    .map(|file| file.command_ids().count())
                    .unwrap_or(0);
                put(&mut self.user, "keybindings", parsed);
                self.report.mapped.push(Mapping {
                    from: "keybindings".into(),
                    to: "keybindings".into(),
                });
                self.report.notes.push(format!(
                    "keybindings: {commands} bindings keep Orca's command ids; they are not translated to Icarus commands yet"
                ));
            }
            None => {
                self.report.unmapped.push("keybindings".into());
                self.report
                    .notes
                    .push("keybindings is not valid JSON; kept as text".into());
            }
        }
        Some(record)
    }

    fn editor_themes(&mut self, themes: &Map) -> Value {
        let mut files = Map::new();
        for (name, value) in themes {
            let record = match value {
                Value::String(text) if EDITOR_THEME_FILE_NAMES.contains(&name.as_str()) => {
                    let (parsed, mut record) = embed(text);
                    let theme_name = parsed
                        .as_ref()
                        .and_then(|p| p.get("name"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    let slot = if name == "editor-dark.json" {
                        "dark"
                    } else {
                        "light"
                    };
                    let mut reference = Map::new();
                    reference.insert("name".into(), theme_name);
                    reference.insert("orcaFile".into(), name.as_str().into());
                    put(
                        &mut self.user,
                        &format!("theme.editor.{slot}"),
                        Value::Object(reference),
                    );
                    if let (Some(parsed), Value::Object(r)) = (parsed, &mut record) {
                        r.insert("theme".into(), parsed);
                    }
                    self.report.mapped.push(Mapping {
                        from: format!("editorThemes.{name}"),
                        to: format!("theme.editor.{slot}"),
                    });
                    self.report.notes.push(format!(
                        "editorThemes.{name}: kept in Orca's VS Code format under `orca.editorThemes` and converted to the Zed theme `{}` in Zed's `themes/{}`",
                        crate::zed::imported_theme_name(slot),
                        crate::zed::IMPORTED_THEME_FILE
                    ));
                    record
                }
                other => {
                    self.report.unmapped.push(format!("editorThemes.{name}"));
                    let mut record = Map::new();
                    record.insert("raw".into(), other.clone());
                    Value::Object(record)
                }
            };
            files.insert(name.clone(), record);
        }
        Value::Object(files)
    }

    fn check_statuses(&mut self) {
        let Some(statuses) =
            layers::get(&self.user, &path("workspace.statuses")).and_then(Value::as_array)
        else {
            return;
        };
        let mut seen = HashSet::new();
        let mut notes = Vec::new();
        for (i, status) in statuses.iter().enumerate() {
            match status.get("id").and_then(Value::as_str) {
                Some(id) if !seen.insert(id.to_owned()) => {
                    notes.push(format!("workspace.statuses.{i}: duplicate id `{id}`"));
                }
                Some(_) => {}
                None => notes.push(format!("workspace.statuses.{i}: no string id")),
            }
        }
        self.report.notes.extend(notes);
    }

    fn check_rules(&mut self) {
        let Some(rules) = layers::get(&self.user, &path("rules.pr")) else {
            return;
        };
        let enabled = rules.get("enabled").and_then(Value::as_bool) == Some(true);
        let scoped = rules
            .get("repoIds")
            .and_then(Value::as_array)
            .is_some_and(|ids| !ids.is_empty());
        if enabled && !scoped {
            self.report.notes.push(
                "rules.pr: the export carries no project scope (repoIds); the rules do nothing until projects are chosen".into(),
            );
        }
        if rules.get("holdStatus").is_none() {
            self.report
                .notes
                .push("rules.pr: no holdStatus (older Orca build); absent means no hold".into());
        }
    }

    fn quick_command_notes(&mut self) {
        let Some(commands) =
            layers::get(&self.user, &path("quickCommands")).and_then(Value::as_array)
        else {
            return;
        };
        let scoped = commands
            .iter()
            .filter(|c| {
                c.get("scope")
                    .and_then(|s| s.get("type"))
                    .and_then(Value::as_str)
                    == Some("project")
            })
            .count();
        if scoped > 0 {
            self.report.notes.push(format!(
                "quickCommands: {scoped} commands are scoped to a project by canonical remote and stay in user settings"
            ));
        }
    }
}

pub fn import_orca_text(text: &str) -> Result<OrcaImport, OrcaParseError> {
    OrcaExport::parse(text).map(|export| import_orca(&export))
}

pub fn import_orca(export: &OrcaExport) -> OrcaImport {
    let mut importer = Importer {
        user: Value::object(),
        zed: Value::object(),
        carry: Map::new(),
        projects: IndexMap::new(),
        report: ImportReport::default(),
    };
    let root = export.root();
    importer
        .carry
        .insert("envelope".into(), strings(root.keys().cloned()));
    let mut raw = Map::new();
    for (key, value) in root {
        match (key.as_str(), value) {
            ("settings", Value::Object(settings)) => {
                let (order, rest) = importer.map_section("settings", settings, &SETTINGS_MAP);
                importer.language_tooling(settings);
                importer
                    .carry
                    .insert("settingsOrder".into(), strings(order));
                importer
                    .carry
                    .insert("settings".into(), Value::Object(rest));
            }
            ("ui", Value::Object(ui)) => {
                let (order, rest) = importer.map_section("ui", ui, &UI_MAP);
                importer.carry.insert("uiOrder".into(), strings(order));
                importer.carry.insert("ui".into(), Value::Object(rest));
            }
            ("projects", Value::Array(entries)) => {
                let mut seen = HashSet::new();
                let list: Vec<Value> = entries
                    .iter()
                    .enumerate()
                    .map(|(i, entry)| importer.project_entry(i, entry, &mut seen))
                    .collect();
                importer.carry.insert("projects".into(), Value::Array(list));
            }
            ("keybindings", Value::String(_)) => {
                if let Some(record) = importer.keybindings(value) {
                    importer.carry.insert("keybindings".into(), record);
                }
            }
            ("keybindings", Value::Null) => {}
            ("editorThemes", Value::Object(themes)) => {
                let files = importer.editor_themes(themes);
                importer.carry.insert("editorThemes".into(), files);
            }
            ("format" | "version" | "exportedAt", _) => {
                raw.insert(key.clone(), value.clone());
            }
            _ => {
                importer.report.unmapped.push(key.clone());
                raw.insert(key.clone(), value.clone());
            }
        }
    }
    importer.carry.insert("values".into(), Value::Object(raw));
    importer.check_statuses();
    importer.check_rules();
    importer.quick_command_notes();

    let Importer {
        mut user,
        zed: tooling,
        carry,
        projects,
        mut report,
    } = importer;
    put(&mut user, "orca", Value::Object(carry));
    let mut resolved = layers::defaults();
    layers::deep_merge(&mut resolved, &user);
    let appearance = crate::zed::render(&resolved);
    let mut zed = appearance.settings;
    layers::deep_merge(&mut zed, &tooling);
    report.notes.extend(appearance.notes);
    report.validation = layers::validate(&user, Some(layers::Layer::User));
    for (remote, doc) in &projects {
        report.validation.extend(
            layers::validate(doc, Some(layers::Layer::ProjectLocal))
                .into_iter()
                .map(|mut e| {
                    e.key = format!("{remote}: {}", e.key);
                    e
                }),
        );
    }
    OrcaImport {
        user,
        projects,
        zed,
        zed_theme: appearance.theme_family,
        report,
    }
}

fn assemble(order: &[String], table: &[(&str, &str)], doc: &Value, rest: Option<&Map>) -> Map {
    let mapped = |key: &str| {
        table
            .iter()
            .find(|(from, _)| *from == key)
            .and_then(|(_, to)| layers::get(doc, &path(to)))
    };
    let mut out = Map::new();
    for key in order {
        let value = if table.iter().any(|(from, _)| from == key) {
            mapped(key)
        } else {
            rest.and_then(|r| r.get(key))
        };
        if let Some(value) = value {
            out.insert(key.clone(), value.clone());
        }
    }
    for (from, _) in table {
        if !out.contains_key(*from)
            && let Some(value) = mapped(from)
        {
            out.insert((*from).to_owned(), value.clone());
        }
    }
    for (key, value) in rest.into_iter().flatten() {
        if !out.contains_key(key) {
            out.insert(key.clone(), value.clone());
        }
    }
    out
}

fn export_project(remote: &str, doc: &Value) -> Value {
    let carry = layers::get(doc, &path("orca.project"));
    let field = |key: &str| carry.and_then(|c| c.get(key));
    let entry_order = match field("entryOrder") {
        Some(order) => string_list(Some(order)),
        None => PROJECT_ENTRY_ORDER
            .iter()
            .map(|k| (*k).to_owned())
            .collect(),
    };
    let table: Vec<(String, String)> = PORTABLE_REPO_FIELDS
        .iter()
        .map(|f| ((*f).to_owned(), project_field_path(f)))
        .collect();
    let table: Vec<(&str, &str)> = table
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let mut settings = assemble(
        &string_list(field("settingsOrder")),
        &table,
        doc,
        field("settings").and_then(Value::as_object),
    );
    if !settings.contains_key("portLabelsCommand")
        && let Some(command) = layers::get(doc, &path(PORTS_LABELS_COMMAND))
    {
        settings.insert("portLabelsCommand".into(), command.clone());
    }
    let extra = field("entryExtra").and_then(Value::as_object);
    let mut entry = Map::new();
    for key in &entry_order {
        match key.as_str() {
            "remote" => {
                entry.insert(key.clone(), remote.into());
            }
            "name" => {
                let name = field("name").cloned().unwrap_or_else(|| "".into());
                entry.insert(key.clone(), name);
            }
            "settings" => {
                entry.insert(key.clone(), Value::Object(settings.clone()));
            }
            other => {
                if let Some(value) = extra.and_then(|e| e.get(other)) {
                    entry.insert(key.clone(), value.clone());
                }
            }
        }
    }
    Value::Object(entry)
}

pub fn export_orca(
    user: &Value,
    projects: &IndexMap<String, Value>,
) -> Result<OrcaExport, OrcaParseError> {
    let empty = Map::new();
    let carry = user
        .get("orca")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let field = |key: &str| carry.get(key);
    let values = field("values").and_then(Value::as_object);
    let mut order = match field("envelope") {
        Some(order) => string_list(Some(order)),
        None => ENVELOPE_ORDER.iter().map(|k| (*k).to_owned()).collect(),
    };
    for key in values.into_iter().flat_map(|v| v.keys()) {
        if !order.contains(key) {
            order.push(key.clone());
        }
    }

    let mut root = Map::new();
    for key in &order {
        let value = match key.as_str() {
            "settings" if values.is_none_or(|v| !v.contains_key(key)) => Value::Object(assemble(
                &string_list(field("settingsOrder")),
                &SETTINGS_MAP,
                user,
                field("settings").and_then(Value::as_object),
            )),
            "ui" if values.is_none_or(|v| !v.contains_key(key)) => Value::Object(assemble(
                &string_list(field("uiOrder")),
                &UI_MAP,
                user,
                field("ui").and_then(Value::as_object),
            )),
            "projects" if values.is_none_or(|v| !v.contains_key(key)) => {
                let mut written = HashSet::new();
                let mut list = Vec::new();
                for item in field("projects")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match item {
                        Value::String(remote) => {
                            if let Some(doc) = projects.get(remote) {
                                list.push(export_project(remote, doc));
                                written.insert(remote.clone());
                            }
                        }
                        other => {
                            if let Some(raw) = other.get("raw") {
                                list.push(raw.clone());
                            }
                        }
                    }
                }
                for (remote, doc) in projects {
                    if !written.contains(remote) {
                        list.push(export_project(remote, doc));
                    }
                }
                Value::Array(list)
            }
            "keybindings" if values.is_none_or(|v| !v.contains_key(key)) => {
                unembed(user.get("keybindings"), field("keybindings"))
                    .map_or(Value::Null, Value::String)
            }
            "editorThemes" if values.is_none_or(|v| !v.contains_key(key)) => {
                let mut themes = Map::new();
                for (name, record) in field("editorThemes")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                {
                    let value = match record.get("raw") {
                        Some(raw) => Some(raw.clone()),
                        None => unembed(record.get("theme"), Some(record)).map(Value::String),
                    };
                    if let Some(value) = value {
                        themes.insert(name.clone(), value);
                    }
                }
                Value::Object(themes)
            }
            "format" => values
                .and_then(|v| v.get(key))
                .cloned()
                .unwrap_or_else(|| FORMAT.into()),
            "version" => values
                .and_then(|v| v.get(key))
                .cloned()
                .unwrap_or(Value::Number(VERSION.into())),
            "exportedAt" => values
                .and_then(|v| v.get(key))
                .cloned()
                .unwrap_or_else(|| "".into()),
            _ => match values.and_then(|v| v.get(key)) {
                Some(value) => value.clone(),
                None => continue,
            },
        };
        root.insert(key.clone(), value);
    }
    OrcaExport::from_root(root)
}
