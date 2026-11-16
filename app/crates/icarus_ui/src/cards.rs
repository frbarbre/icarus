//! What the workspace cards show and how the list is grouped (plan §3a, features.md §2.2).
//! Both live in Icarus settings (`workspace.cardFields`, `sidebar.groupBy`, `sidebar.stackOrder`),
//! so an Orca import, `icarus config set` and the panel's View menu all change the same values.

use gpui::Action;
use schemars::JsonSchema;
use serde::Deserialize;

/// A card field the panel can draw, under Orca's `worktreeCardProperties` id.
pub struct CardField {
    pub id: &'static str,
    pub label: &'static str,
}

/// The fields the panel draws, in menu order. Other ids in the setting are kept but have no effect.
pub const CARD_FIELDS: &[CardField] = &[
    CardField {
        id: "workspace-status",
        label: "Status chip (other groupings)",
    },
    CardField {
        id: "inline-agents",
        label: "Agents",
    },
    CardField {
        id: "pr",
        label: "Pull request",
    },
    CardField {
        id: "pr-author",
        label: "PR author",
    },
    CardField {
        id: "pr-diff",
        label: "PR diff (+N −M)",
    },
    CardField {
        id: "stack",
        label: "Stack position",
    },
    CardField {
        id: "linear-title",
        label: "Linear title and labels",
    },
    CardField {
        id: "ports",
        label: "Ports",
    },
    CardField {
        id: "project",
        label: "Project",
    },
];

/// Orca's default card properties (its `DEFAULT_WORKTREE_CARD_PROPERTIES`); the rest are opt-in.
const DEFAULT_FIELDS: &[&str] = &[
    "status",
    "unread",
    "issue",
    "linear-issue",
    "jira-issue",
    "pr",
    "automation",
    "cli",
    "comment",
    "ports",
    "inline-agents",
    "host",
    "project",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GroupBy {
    #[default]
    Status,
    Stack,
    Project,
    None,
}

impl GroupBy {
    /// Orca's `groupBy` values; pr-status grouping falls back to status for now.
    pub fn parse(value: &str) -> Self {
        match value {
            "stack" => Self::Stack,
            "repo" => Self::Project,
            "none" => Self::None,
            _ => Self::Status,
        }
    }

    pub fn setting(self) -> &'static str {
        match self {
            Self::Status => "workspace-status",
            Self::Stack => "stack",
            Self::Project => "repo",
            Self::None => "none",
        }
    }
}

/// The panel's view settings as the server last reported them.
#[derive(Debug, Clone, PartialEq)]
pub struct CardView {
    /// Every id in `workspace.cardFields`, unknown ones included, so toggling never drops them.
    pub fields: Vec<String>,
    pub group_by: GroupBy,
    pub stack_ascending: bool,
    /// The status the rules give someone else's PR; stack grouping lists it first.
    pub reviewing_status: Option<String>,
}

impl Default for CardView {
    fn default() -> Self {
        Self {
            fields: DEFAULT_FIELDS.iter().map(|id| id.to_string()).collect(),
            group_by: GroupBy::Status,
            stack_ascending: false,
            reviewing_status: None,
        }
    }
}

impl CardView {
    /// Every field on, for previews that should show the whole card.
    pub fn showing_all() -> Self {
        let mut view = Self::default();
        for field in CARD_FIELDS {
            if !view.shows(field.id) {
                view.fields.push(field.id.to_string());
            }
        }
        view
    }

    /// The status chip doubles as the status menu, so grouping by status keeps it on.
    pub fn shows_status_chip(&self) -> bool {
        self.group_by == GroupBy::Status || self.shows("workspace-status")
    }

    /// Reads `workspace.cardFields`, the `sidebar` object and `rules.orca_compat`.
    pub fn from_settings(
        card_fields: &serde_json::Value,
        sidebar: &serde_json::Value,
        rules: &serde_json::Value,
    ) -> Self {
        let defaults = Self::default();
        let fields = card_fields
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or(defaults.fields);
        let reviewing_status = rules["enabled"]
            .as_bool()
            .unwrap_or(false)
            .then(|| rules["statusByCondition"]["reviewing"].as_str())
            .flatten()
            .map(str::to_string);
        Self {
            fields,
            group_by: sidebar["groupBy"]
                .as_str()
                .map(GroupBy::parse)
                .unwrap_or_default(),
            stack_ascending: sidebar["stackOrder"].as_str() == Some("asc"),
            reviewing_status,
        }
    }

    pub fn shows(&self, field: &str) -> bool {
        self.fields.iter().any(|id| id == field)
    }

    /// The setting value after flipping `field`, keeping every other id and their order.
    pub fn toggled(&self, field: &str) -> Vec<String> {
        if self.shows(field) {
            self.fields
                .iter()
                .filter(|id| id.as_str() != field)
                .cloned()
                .collect()
        } else {
            let mut fields = self.fields.clone();
            fields.push(field.to_string());
            fields
        }
    }
}

/// Shows or hides one part of the workspace cards (an id from `workspace.cardFields`).
#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Action)]
#[action(namespace = icarus)]
#[serde(deny_unknown_fields)]
pub struct ToggleCardField {
    pub field: String,
}

/// Groups the workspaces list by status, by GitHub stack, or not at all.
#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Action)]
#[action(namespace = icarus)]
#[serde(deny_unknown_fields)]
pub struct SetWorkspaceGrouping {
    /// `workspace-status`, `stack` or `none`.
    pub group_by: String,
}

gpui::actions!(
    icarus,
    [
        /// Switches the workspaces list between grouping by stack and by status.
        ToggleGroupByStack,
        /// Opens the linked Linear issue of the workspace in this window.
        OpenLinearIssue,
    ]
);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn settings_drive_the_view_and_missing_ones_use_orcas_defaults() {
        let view = CardView::from_settings(
            &json!(["pr", "made-up", "linear-title"]),
            &json!({ "groupBy": "stack", "stackOrder": "asc" }),
            &json!({ "enabled": true, "statusByCondition": { "reviewing": "in-review" } }),
        );
        assert_eq!(view.group_by, GroupBy::Stack);
        assert!(view.stack_ascending);
        assert_eq!(view.reviewing_status.as_deref(), Some("in-review"));
        assert!(view.shows("pr") && !view.shows("pr-author"));

        let defaults = CardView::from_settings(&json!(null), &json!(null), &json!(null));
        assert_eq!(defaults.group_by, GroupBy::Status);
        assert!(defaults.shows("pr") && defaults.shows("inline-agents"));
        assert!(
            !defaults.shows("pr-author") && !defaults.shows("linear-title"),
            "opt-in in Orca"
        );
        assert!(
            defaults.shows_status_chip(),
            "grouped by status, the chip is the status menu"
        );
        let all = CardView::showing_all();
        assert!(CARD_FIELDS.iter().all(|field| all.shows(field.id)));
        let disabled_rules = CardView::from_settings(
            &json!(null),
            &json!(null),
            &json!({ "enabled": false, "statusByCondition": { "reviewing": "in-review" } }),
        );
        assert_eq!(
            disabled_rules.reviewing_status, None,
            "Orca only gives reviewing its lane while the rules are on"
        );
    }

    #[test]
    fn toggling_keeps_unknown_ids_and_order() {
        let view = CardView::from_settings(
            &json!(["made-up", "pr", "ports"]),
            &json!(null),
            &json!(null),
        );
        assert_eq!(view.toggled("pr"), ["made-up", "ports"]);
        assert_eq!(view.toggled("stack"), ["made-up", "pr", "ports", "stack"]);
    }

    #[test]
    fn grouping_round_trips_orcas_values() {
        for value in ["workspace-status", "stack", "repo", "none"] {
            assert_eq!(GroupBy::parse(value).setting(), value);
        }
        assert_eq!(
            GroupBy::parse("pr-status"),
            GroupBy::Status,
            "not built yet, so status"
        );
    }
}
