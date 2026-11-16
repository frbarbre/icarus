//! What the workspace cards show and how the list is grouped (plan §3a, features.md §2.2).
//! Both live in Icarus settings (`workspace.cardFields`, `sidebar.groupBy`, `sidebar.sortBy`, `sidebar.stackOrder`),
//! so an Orca import, `icarus config set` and the panel's View menu all change the same values.

use editor::{Editor, EditorEvent};
use gpui::{Action, Context, Entity, Focusable as _, Window};
use schemars::JsonSchema;
use serde::Deserialize;
use ui::prelude::*;

use crate::WorkspacesPanel;
use crate::model::{WorkspaceSummary, matches_filter, sort_workspaces};

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
    PrStatus,
    Stack,
    Project,
    None,
}

impl GroupBy {
    /// Orca's `groupBy` values.
    pub fn parse(value: &str) -> Self {
        match value {
            "pr-status" => Self::PrStatus,
            "stack" => Self::Stack,
            "repo" => Self::Project,
            "none" => Self::None,
            _ => Self::Status,
        }
    }

    pub fn setting(self) -> &'static str {
        match self {
            Self::Status => "workspace-status",
            Self::PrStatus => "pr-status",
            Self::Stack => "stack",
            Self::Project => "repo",
            Self::None => "none",
        }
    }
}

/// Orca's `sortBy` values (`WORKSPACE_SORT_BY_CHOICES`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    Name,
    Smart,
    #[default]
    Recent,
    Repo,
    /// Orca's drag order; without dragging the list keeps the server's (creation) order.
    Manual,
}

impl SortBy {
    /// The choices the View menu offers, with Orca's labels; Manual needs dragging, so it isn't one.
    pub const CHOICES: [(SortBy, &'static str); 4] = [
        (SortBy::Name, "Name"),
        (SortBy::Smart, "Agent Activity"),
        (SortBy::Recent, "Recent"),
        (SortBy::Repo, "Project"),
    ];

    pub fn parse(value: &str) -> Self {
        match value {
            "name" => Self::Name,
            "smart" => Self::Smart,
            "repo" => Self::Repo,
            "manual" => Self::Manual,
            _ => Self::Recent,
        }
    }

    pub fn setting(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Smart => "smart",
            Self::Recent => "recent",
            Self::Repo => "repo",
            Self::Manual => "manual",
        }
    }
}

/// The panel's view settings as the server last reported them.
#[derive(Debug, Clone, PartialEq)]
pub struct CardView {
    /// Every id in `workspace.cardFields`, unknown ones included, so toggling never drops them.
    pub fields: Vec<String>,
    pub group_by: GroupBy,
    pub sort_by: SortBy,
    pub stack_ascending: bool,
    /// The status the rules give someone else's PR; stack grouping lists it first.
    pub reviewing_status: Option<String>,
}

impl Default for CardView {
    fn default() -> Self {
        Self {
            fields: DEFAULT_FIELDS.iter().map(|id| id.to_string()).collect(),
            group_by: GroupBy::Status,
            sort_by: SortBy::default(),
            stack_ascending: false,
            reviewing_status: None,
        }
    }
}

impl CardView {
    /// Every field on and the cards in the order given, for previews that should show the whole card.
    pub fn showing_all() -> Self {
        let mut view = Self {
            sort_by: SortBy::Manual,
            ..Self::default()
        };
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
            sort_by: sidebar["sortBy"]
                .as_str()
                .map(SortBy::parse)
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
    /// `workspace-status`, `pr-status`, `stack`, `repo` or `none`.
    pub group_by: String,
}

/// Sorts the workspaces inside each group.
#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Action)]
#[action(namespace = icarus)]
#[serde(deny_unknown_fields)]
pub struct SetWorkspaceSort {
    /// `name`, `smart` (agent activity), `recent`, `repo` or `manual`.
    pub sort_by: String,
}

gpui::actions!(
    icarus,
    [
        /// Focuses the text filter at the top of the workspaces list.
        FocusWorkspaceFilter,
        /// Switches the workspaces list between grouping by stack and by status.
        ToggleGroupByStack,
        /// Opens the linked Linear issue of the workspace in this window.
        OpenLinearIssue,
    ]
);

impl WorkspacesPanel {
    /// The workspaces the text filter keeps, in the chosen sort order (before grouping and pins).
    pub(crate) fn arranged_workspaces(&self) -> Vec<WorkspaceSummary> {
        let mut workspaces: Vec<WorkspaceSummary> = self
            .workspaces
            .iter()
            .filter(|workspace| matches_filter(workspace, &self.filter_query))
            .cloned()
            .collect();
        sort_workspaces(&mut workspaces, self.view.sort_by, |id| {
            self.project_names.get(id).cloned().unwrap_or_default()
        });
        workspaces
    }

    pub fn set_sort_by(&mut self, sort_by: SortBy, cx: &mut Context<Self>) {
        self.view.sort_by = sort_by;
        cx.notify();
        self.save_setting("sidebar.sortBy", serde_json::json!(sort_by.setting()), cx);
    }

    pub fn filter_query(&self) -> &str {
        &self.filter_query
    }

    /// The filter field, made on first use because the panel is built without a window.
    pub(crate) fn filter_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Editor> {
        if let Some(editor) = &self.filter {
            return editor.clone();
        }
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter workspaces…", window, cx);
            editor
        });
        self._filter_subscription = Some(cx.subscribe(
            &editor,
            |panel, editor, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    panel.filter_query = editor.read(cx).text(cx).trim().to_string();
                    cx.notify();
                }
            },
        ));
        self.filter = Some(editor.clone());
        editor
    }

    pub fn focus_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.filter_editor(window, cx);
        window.focus(&editor.focus_handle(cx), cx);
    }

    fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.filter_editor(window, cx);
        editor.update(cx, |editor, cx| editor.set_text("", window, cx));
        window.focus(&self.focus_handle, cx);
    }

    pub(crate) fn render_filter(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let editor = self.filter_editor(window, cx);
        h_flex()
            .key_context("IcarusWorkspacesFilter")
            .on_action(
                cx.listener(|panel, _: &menu::Cancel, window, cx| panel.clear_filter(window, cx)),
            )
            .on_action(
                cx.listener(|panel, _: &editor::actions::Cancel, window, cx| {
                    panel.clear_filter(window, cx)
                }),
            )
            .flex_none()
            .mx_1()
            .mt_1()
            .px_1p5()
            .py_0p5()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Icon::new(IconName::MagnifyingGlass)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(div().flex_1().child(editor))
            .into_any_element()
    }
}

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

    fn sections(panel: &Entity<WorkspacesPanel>, cx: &mut gpui::VisualTestContext) -> Vec<String> {
        panel.read_with(cx, |panel, cx| {
            panel
                .sections(cx)
                .into_iter()
                .map(|(_, title, _, items)| {
                    let names: Vec<&str> = items.iter().map(|w| w.name.as_str()).collect();
                    format!("{title}: {}", names.join(","))
                })
                .collect()
        })
    }

    fn with_pr(mut workspace: WorkspaceSummary, number: u64, state: &str) -> WorkspaceSummary {
        workspace.pr = Some(crate::PrSummary {
            number,
            title: format!("PR {number}"),
            state: state.into(),
            ..crate::PrSummary::default()
        });
        workspace
    }

    fn listed() -> Vec<WorkspaceSummary> {
        use crate::tests::summary;
        let mut beta = with_pr(summary("beta", "/w/beta", Some("todo")), 7, "open");
        beta.created_at_ms = 3;
        let mut alpha = summary("alpha", "/w/alpha", Some("todo"));
        alpha.created_at_ms = 1;
        let mut gamma = with_pr(summary("gamma", "/w/gamma", Some("todo")), 9, "merged");
        gamma.created_at_ms = 2;
        let mut delta = with_pr(summary("delta", "/w/delta", Some("todo")), 4, "draft");
        delta.created_at_ms = 4;
        delta.issue = Some(json!({ "provider": "linear", "id": "ABC-12", "title": "Fix login" }));
        vec![beta, alpha, gamma, delta]
    }

    #[gpui::test]
    async fn sorting_pins_and_pr_grouping_order_the_list(cx: &mut gpui::TestAppContext) {
        let (_, panel, mut cx) = crate::tests::panel_window(listed(), cx).await;
        let cx = &mut cx;
        assert_eq!(
            sections(&panel, cx),
            ["todo: beta,alpha,gamma,delta"],
            "previews keep the order given"
        );

        cx.dispatch_action(SetWorkspaceSort {
            sort_by: "name".into(),
        });
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.view().sort_by),
            SortBy::Name
        );
        assert_eq!(
            sections(&panel, cx),
            ["todo: delta,alpha,beta,gamma"],
            "by card title, and ABC-12 titles delta"
        );
        cx.dispatch_action(SetWorkspaceSort {
            sort_by: "recent".into(),
        });
        assert_eq!(sections(&panel, cx), ["todo: delta,beta,gamma,alpha"]);

        panel.update(cx, |panel, cx| {
            let mut workspaces = listed();
            workspaces[2].pinned = true;
            panel.set_workspaces(workspaces, cx);
        });
        assert_eq!(
            sections(&panel, cx),
            ["todo: gamma,delta,beta,alpha"],
            "a pinned card leads its group"
        );

        cx.dispatch_action(SetWorkspaceGrouping {
            group_by: "pr-status".into(),
        });
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.view().group_by),
            GroupBy::PrStatus
        );
        assert_eq!(
            sections(&panel, cx),
            ["Done: gamma", "In review: beta", "In progress: delta,alpha"]
        );
    }

    #[gpui::test]
    async fn the_filter_field_narrows_the_list_and_escape_clears_it(cx: &mut gpui::TestAppContext) {
        let (_, panel, mut cx) = crate::tests::panel_window(listed(), cx).await;
        let cx = &mut cx;
        cx.dispatch_action(FocusWorkspaceFilter);
        cx.run_until_parked();
        cx.simulate_input("login");
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.filter_query().to_string()),
            "login"
        );
        assert_eq!(
            sections(&panel, cx),
            ["todo: delta"],
            "the Linear issue title"
        );

        panel.update_in(cx, |panel, window, cx| {
            let editor = panel.filter_editor(window, cx);
            editor.update(cx, |editor, cx| editor.set_text("#7", window, cx));
        });
        cx.run_until_parked();
        assert_eq!(sections(&panel, cx), ["todo: beta"], "the PR number");

        panel.update_in(cx, |panel, window, cx| panel.focus_filter(window, cx));
        cx.dispatch_action(editor::actions::Cancel);
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.filter_query().to_string()),
            ""
        );
        assert_eq!(sections(&panel, cx), ["todo: beta,alpha,gamma,delta"]);
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
        for value in ["workspace-status", "pr-status", "stack", "repo", "none"] {
            assert_eq!(GroupBy::parse(value).setting(), value);
        }
        assert_eq!(GroupBy::parse("made-up"), GroupBy::Status);
        for value in ["name", "smart", "recent", "repo", "manual"] {
            assert_eq!(SortBy::parse(value).setting(), value);
        }
        assert_eq!(
            SortBy::parse("made-up"),
            SortBy::Recent,
            "the settings default"
        );
        let view =
            CardView::from_settings(&json!(null), &json!({ "sortBy": "name" }), &json!(null));
        assert_eq!(view.sort_by, SortBy::Name);
    }
}
