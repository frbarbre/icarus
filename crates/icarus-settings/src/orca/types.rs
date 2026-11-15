use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::json::{Map, Value};

// Absent and null differ on import: an absent rule field keeps the importer's value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Presence<T> {
    #[default]
    Absent,
    Null,
    Value(T),
}

impl<T> Presence<T> {
    pub fn is_absent(&self) -> bool {
        matches!(self, Presence::Absent)
    }

    pub fn as_option(&self) -> Option<&T> {
        match self {
            Presence::Value(value) => Some(value),
            _ => None,
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Presence<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Option::<T>::deserialize(deserializer)?.map_or(Presence::Null, Presence::Value))
    }
}

impl<T: Serialize> Serialize for Presence<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Presence::Value(value) => value.serialize(serializer),
            _ => serializer.serialize_none(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceStatusDefinition {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(flatten)]
    pub extra: Map,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkspaceStatusRuleCondition {
    Merged,
    Closed,
    Reviewing,
    MergeQueue,
    Conflicts,
    Draft,
    ChangesRequested,
    Merging,
    PmApproval,
    Review,
}

impl WorkspaceStatusRuleCondition {
    pub const ORDER: [Self; 10] = [
        Self::Merged,
        Self::Closed,
        Self::Reviewing,
        Self::MergeQueue,
        Self::Conflicts,
        Self::Draft,
        Self::ChangesRequested,
        Self::Merging,
        Self::PmApproval,
        Self::Review,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::Reviewing => "reviewing",
            Self::MergeQueue => "merge-queue",
            Self::Conflicts => "conflicts",
            Self::Draft => "draft",
            Self::ChangesRequested => "changes-requested",
            Self::Merging => "merging",
            Self::PmApproval => "pm-approval",
            Self::Review => "review",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ORDER.into_iter().find(|c| c.id() == id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceActionPrompts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_and_push: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_pull_request: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_for_review: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_pull_request: Option<String>,
    #[serde(flatten)]
    pub extra: Map,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewInbox {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rereview_prompt_template: Option<String>,
    #[serde(flatten)]
    pub extra: Map,
}

impl ReviewInbox {
    pub fn is_enabled(&self) -> bool {
        self.enabled == Some(true)
    }

    pub fn agent_or_default(&self) -> &str {
        self.agent.as_deref().unwrap_or("claude")
    }

    pub fn view_or_default(&self) -> &str {
        match self.view.as_deref() {
            Some(view @ ("chat" | "terminal" | "claude-web" | "default")) => view,
            _ => "chat",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStatusRuleConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_by_condition: Option<IndexMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merging_check_name: Option<String>,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub new_workspace_status: Presence<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_workspace_status_since: Option<Value>,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub hold_status: Presence<String>,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub pm_approval_team: Presence<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_resolved: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_reviewed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolve_conflicts_with_agent: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_prompt_template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_entered_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_prompts: Option<WorkspaceActionPrompts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_inbox: Option<ReviewInbox>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handled_pull_requests: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_version: Option<Value>,
    #[serde(flatten)]
    pub extra: Map,
}

fn trimmed(value: Option<&String>) -> Option<&str> {
    value.map(|s| s.trim()).filter(|s| !s.is_empty())
}

impl WorkspaceStatusRuleConfig {
    pub fn from_value(value: &Value) -> Result<Self, serde_json::Error> {
        crate::json::from_value(value)
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled == Some(true)
    }

    pub fn status_for(&self, condition: WorkspaceStatusRuleCondition) -> Option<&str> {
        self.status_by_condition
            .as_ref()
            .and_then(|map| trimmed(map.get(condition.id())))
    }

    pub fn hold_status(&self) -> Option<&str> {
        trimmed(self.hold_status.as_option())
    }

    pub fn new_workspace_status(&self) -> Option<&str> {
        trimmed(self.new_workspace_status.as_option())
    }

    pub fn pm_approval_team(&self) -> Option<&str> {
        trimmed(self.pm_approval_team.as_option())
    }

    pub fn merging_check_name(&self) -> &str {
        trimmed(self.merging_check_name.as_ref()).unwrap_or("Reviews satisfied")
    }

    pub fn deletes_on_resolved(&self) -> bool {
        self.on_resolved.as_deref() == Some("delete")
    }

    pub fn deletes_on_reviewed(&self) -> bool {
        self.on_reviewed.as_deref() == Some("delete")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HookScripts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<String>,
    #[serde(flatten)]
    pub extra: Map,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoHookSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_run_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_agent_startup_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_source_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scripts: Option<HookScripts>,
    #[serde(flatten)]
    pub extra: Map,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CiTriageConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hints: Option<String>,
    #[serde(flatten)]
    pub extra: Map,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableRepoSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_base_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_settings: Option<RepoHookSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_source_preference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_jira_project: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_sync_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_worktree_visibility: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_worktree_visibility: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_visibility_source_preferences: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python_venv_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_labels_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_triage: Option<CiTriageConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_control_ai: Option<Value>,
    #[serde(flatten)]
    pub extra: Map,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSettingsTransfer {
    pub remote: String,
    #[serde(default)]
    pub name: String,
    pub settings: PortableRepoSettings,
    #[serde(flatten)]
    pub extra: Map,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickCommandScope {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(flatten)]
    pub extra: Map,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalQuickCommand {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<QuickCommandScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_enter: Option<bool>,
    #[serde(flatten)]
    pub extra: Map,
}

impl TerminalQuickCommand {
    pub fn project_remote(&self) -> Option<&str> {
        self.scope
            .as_ref()
            .filter(|scope| scope.kind == "project")
            .and_then(|scope| scope.remote.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LanguageServerSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(flatten)]
    pub extra: Map,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KeybindingsFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<Value>,
    #[serde(default)]
    pub keybindings: IndexMap<String, Value>,
    #[serde(default)]
    pub platforms: IndexMap<String, IndexMap<String, Value>>,
    #[serde(flatten)]
    pub extra: Map,
}

impl KeybindingsFile {
    pub fn command_ids(&self) -> impl Iterator<Item = &str> {
        self.keybindings
            .keys()
            .chain(self.platforms.values().flat_map(|p| p.keys()))
            .map(String::as_str)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EditorThemes<'a> {
    pub dark: Option<&'a str>,
    pub light: Option<&'a str>,
}
