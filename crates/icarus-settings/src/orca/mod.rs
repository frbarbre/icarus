mod portable;
mod types;

use indexmap::IndexMap;
use serde::de::DeserializeOwned;

use crate::json::{self, Map, ParseError, Value};

pub use portable::{
    DEFAULT_CI_TRIAGE_INSTRUCTIONS, EDITOR_THEME_FILE_NAMES, LOCAL_STATUS_RULE_KEYS,
    PORTABLE_REPO_FIELDS, PORTABLE_UI_KEYS, ci_triage_instructions, is_portable_setting_key,
    pick_portable_repo_settings, pick_portable_settings, pick_portable_ui, scrub_private_fields,
};
pub use types::{
    CiTriageConfig, EditorThemes, HookScripts, KeybindingsFile, LanguageServerSettings,
    PortableRepoSettings, Presence, ProjectSettingsTransfer, QuickCommandScope, RepoHookSettings,
    ReviewInbox, TerminalQuickCommand, WorkspaceActionPrompts, WorkspaceStatusDefinition,
    WorkspaceStatusRuleCondition, WorkspaceStatusRuleConfig,
};

pub const FORMAT: &str = "orca-settings";
pub const VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OrcaParseError {
    #[error("The file is not valid JSON.")]
    InvalidJson(ParseError),
    #[error("The file is not an Orca settings export.")]
    NotOrcaExport,
    #[error("The file comes from a newer Orca; update Orca and try again.")]
    UnsupportedVersion,
}

#[derive(Debug, thiserror::Error)]
#[error("`{key}` does not have the expected shape: {source}")]
pub struct ViewError {
    pub key: String,
    #[source]
    pub source: serde_json::Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrcaExport {
    root: Map,
}

impl OrcaExport {
    pub fn parse(text: &str) -> Result<Self, OrcaParseError> {
        match json::parse(text).map_err(OrcaParseError::InvalidJson)? {
            Value::Object(root) => Self::from_root(root),
            _ => Err(OrcaParseError::NotOrcaExport),
        }
    }

    pub fn from_root(root: Map) -> Result<Self, OrcaParseError> {
        if root.get("format").and_then(Value::as_str) != Some(FORMAT) {
            return Err(OrcaParseError::NotOrcaExport);
        }
        // JavaScript compares numbers by value, so `1.0` and `1e0` pass while `"1"` does not.
        let version_ok = root
            .get("version")
            .and_then(Value::as_number)
            .is_some_and(|n| n.as_f64() == 1.0);
        if !version_ok {
            return Err(OrcaParseError::UnsupportedVersion);
        }
        Ok(Self { root })
    }

    pub fn to_text(&self) -> String {
        let mut text = self.to_gui_text();
        text.push('\n');
        text
    }

    pub fn to_gui_text(&self) -> String {
        Value::Object(self.root.clone()).to_pretty_string()
    }

    pub fn root(&self) -> &Map {
        &self.root
    }

    pub fn root_mut(&mut self) -> &mut Map {
        &mut self.root
    }

    pub fn into_root(self) -> Map {
        self.root
    }

    pub fn exported_at(&self) -> &str {
        self.root
            .get("exportedAt")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    pub fn settings(&self) -> Option<&Map> {
        self.root.get("settings").and_then(Value::as_object)
    }

    pub fn ui(&self) -> Option<&Map> {
        self.root.get("ui").and_then(Value::as_object)
    }

    pub fn setting(&self, key: &str) -> Option<&Value> {
        self.settings().and_then(|map| map.get(key))
    }

    pub fn ui_value(&self, key: &str) -> Option<&Value> {
        self.ui().and_then(|map| map.get(key))
    }

    fn view<T: DeserializeOwned>(value: Option<&Value>, key: &str) -> Result<Option<T>, ViewError> {
        value
            .map(|value| {
                json::from_value(value).map_err(|source| ViewError {
                    key: key.to_owned(),
                    source,
                })
            })
            .transpose()
    }

    pub fn workspace_statuses(&self) -> Result<Option<Vec<WorkspaceStatusDefinition>>, ViewError> {
        Self::view(self.ui_value("workspaceStatuses"), "ui.workspaceStatuses")
    }

    pub fn status_rules(&self) -> Result<Option<WorkspaceStatusRuleConfig>, ViewError> {
        Self::view(
            self.ui_value("workspaceStatusRules"),
            "ui.workspaceStatusRules",
        )
    }

    pub fn worktree_card_properties(&self) -> Result<Option<Vec<String>>, ViewError> {
        Self::view(
            self.ui_value("worktreeCardProperties"),
            "ui.worktreeCardProperties",
        )
    }

    pub fn quick_commands(&self) -> Result<Option<Vec<TerminalQuickCommand>>, ViewError> {
        Self::view(
            self.setting("terminalQuickCommands"),
            "settings.terminalQuickCommands",
        )
    }

    pub fn language_servers(
        &self,
    ) -> Result<Option<IndexMap<String, LanguageServerSettings>>, ViewError> {
        Self::view(self.setting("languageServers"), "settings.languageServers")
    }

    pub fn formatters(&self) -> Option<&Value> {
        self.setting("formatters")
    }

    pub fn linters(&self) -> Option<&Value> {
        self.setting("linters")
    }

    // Entries Orca would skip (no string remote or no settings object) are skipped here too.
    pub fn projects(&self) -> Result<Vec<ProjectSettingsTransfer>, ViewError> {
        let Some(entries) = self.root.get("projects").and_then(Value::as_array) else {
            return Ok(Vec::new());
        };
        let mut projects = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            let usable = entry.get("remote").and_then(Value::as_str).is_some()
                && entry.get("settings").is_some_and(Value::is_object);
            if usable {
                let key = format!("projects.{index}");
                projects.extend(Self::view(Some(entry), &key)?);
            }
        }
        Ok(projects)
    }

    pub fn keybindings_text(&self) -> Option<&str> {
        self.root.get("keybindings").and_then(Value::as_str)
    }

    pub fn keybindings(&self) -> Result<Option<KeybindingsFile>, ViewError> {
        let Some(text) = self.keybindings_text() else {
            return Ok(None);
        };
        let parsed = serde_json::from_str::<Value>(text).map_err(|source| ViewError {
            key: "keybindings".to_owned(),
            source,
        })?;
        Self::view(Some(&parsed), "keybindings")
    }

    pub fn editor_themes(&self) -> EditorThemes<'_> {
        let themes = self.root.get("editorThemes");
        let theme = |name: &str| themes.and_then(|t| t.get(name)).and_then(Value::as_str);
        EditorThemes {
            dark: theme("editor-dark.json"),
            light: theme("editor-light.json"),
        }
    }

    pub fn editor_theme_name(text: &str) -> Option<String> {
        json::parse(text)
            .ok()?
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    // What Orca's parseSettingsTransfer keeps: an untrusted file is filtered again on the way in.
    pub fn portable(&self) -> OrcaExport {
        let mut root = Map::new();
        root.insert("format".into(), FORMAT.into());
        root.insert("version".into(), Value::Number(VERSION.into()));
        root.insert("exportedAt".into(), self.exported_at().into());
        let settings = self
            .settings()
            .map(pick_portable_settings)
            .unwrap_or_default();
        root.insert("settings".into(), Value::Object(settings));
        let ui = self.ui().map(pick_portable_ui).unwrap_or_default();
        root.insert("ui".into(), Value::Object(ui));
        let projects = portable::parse_portable_projects(self.root.get("projects"));
        root.insert("projects".into(), Value::Array(projects));
        let keybindings = self.keybindings_text().map_or(Value::Null, Value::from);
        root.insert("keybindings".into(), keybindings);
        let themes = portable::parse_editor_themes(self.root.get("editorThemes"));
        root.insert("editorThemes".into(), Value::Object(themes));
        OrcaExport { root }
    }

    pub fn summary(&self) -> String {
        let count = |value: Option<&Value>| value.and_then(Value::as_object).map_or(0, Map::len);
        let mut parts = vec![
            format!("{} settings", count(self.root.get("settings"))),
            format!("{} layout preferences", count(self.root.get("ui"))),
        ];
        let rules = self.ui_value("workspaceStatusRules");
        if rules.is_some_and(|r| !matches!(r, Value::Null | Value::Bool(false))) {
            parts.push("automations".to_owned());
        }
        let projects = self
            .root
            .get("projects")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if projects > 0 {
            parts.push(format!("settings of {projects} projects"));
        }
        if self.keybindings_text().is_some() {
            parts.push("keybindings".to_owned());
        }
        let themes = count(self.root.get("editorThemes"));
        if themes > 0 {
            parts.push(format!("{themes} editor themes"));
        }
        parts.join(", ")
    }
}
