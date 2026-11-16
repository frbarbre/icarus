//! What the workspace cards show and how the list is grouped (plan §3a, features.md §2.2).
//! Both live in Icarus settings (`workspace.cardFields`, `sidebar.groupBy`, `sidebar.sortBy`, `sidebar.stackOrder`),
//! so an Orca import, `icarus config set` and the panel's View menu all change the same values.

use editor::{Editor, EditorEvent};
use gpui::{Action, Context, Entity, Focusable as _, Modifiers, Pixels, Render, Window};
use schemars::JsonSchema;
use serde::Deserialize;
use ui::prelude::*;
use util::ResultExt as _;

use crate::WorkspacesPanel;
use crate::model::{
    AgentSummary, PrSummary, Unread, UnreadKind, WorkspaceSummary, matches_filter, sort_workspaces,
};
use crate::routing::ServerRef;

/// A card field the panel can draw, under Orca's `worktreeCardProperties` id.
pub struct CardField {
    pub id: &'static str,
    pub label: &'static str,
}

/// The fields the panel draws, in menu order. Other ids in the setting are kept but have no effect.
pub const CARD_FIELDS: &[CardField] = &[
    CardField {
        id: "unread",
        label: "Unread dot",
    },
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
        label: "Linear link, labels and title",
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

pub const STATUS_FIELD: &str = "workspace-status";
/// What older defaults called the status chip; still read so existing settings keep working.
const STATUS_FIELD_ALIAS: &str = "status";

/// The field id `id` stands for.
fn canonical_field(id: &str) -> &str {
    if id == STATUS_FIELD_ALIAS {
        STATUS_FIELD
    } else {
        id
    }
}

/// `fields` without every id that stands for `field`, or `None` when none did.
pub fn without_field(fields: &[String], field: &str) -> Option<Vec<String>> {
    let field = canonical_field(field);
    let kept: Vec<String> = fields
        .iter()
        .filter(|id| canonical_field(id) != field)
        .cloned()
        .collect();
    (kept.len() != fields.len()).then_some(kept)
}

/// Orca's default card properties (its `DEFAULT_WORKTREE_CARD_PROPERTIES`); the rest are opt-in.
const DEFAULT_FIELDS: &[&str] = &[
    STATUS_FIELD,
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
    /// Orca's drag order, stored on the server; new workspaces go on top.
    Manual,
}

impl SortBy {
    /// The choices the View menu offers, with Orca's labels.
    pub const CHOICES: [(SortBy, &'static str); 5] = [
        (SortBy::Name, "Name"),
        (SortBy::Smart, "Agent Activity"),
        (SortBy::Recent, "Recent"),
        (SortBy::Repo, "Project"),
        (SortBy::Manual, "Manual"),
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
    /// This app's own fields (`client.json`), which win over `fields` when set.
    pub local_fields: Option<Vec<String>>,
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
            local_fields: None,
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

    /// Grouped by status, the section header already says it.
    pub fn shows_status_chip(&self) -> bool {
        self.group_by != GroupBy::Status && self.shows(STATUS_FIELD)
    }

    /// Reads `workspace.cardFields`, the `sidebar` object and `rules.pr`.
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
            local_fields: None,
            reviewing_status,
        }
    }

    /// The fields drawn: this app's own, else the server's.
    pub fn shown_fields(&self) -> &[String] {
        self.local_fields.as_deref().unwrap_or(&self.fields)
    }

    pub fn shows(&self, field: &str) -> bool {
        without_field(self.shown_fields(), field).is_some()
    }

    /// The fields after flipping `field`, keeping every other id and their order.
    pub fn toggled(&self, field: &str) -> Vec<String> {
        let shown = self.shown_fields();
        if let Some(hidden) = without_field(shown, field) {
            hidden
        } else {
            let mut fields = shown.to_vec();
            fields.push(field.to_string());
            fields
        }
    }
}

/// Shows or hides one part of the workspace cards in this app only (an id from `workspace.cardFields`).
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
        /// Drops this app's own card fields, so the cards follow `workspace.cardFields` again.
        UseSharedCardFields,
    ]
);

/// The card's unread dot: accent for a finished agent, warning when it needs you.
pub fn render_unread_dot(workspace_id: &str, unread: &Unread, cx: &App) -> impl IntoElement {
    let (color, tooltip) = match unread.kind {
        UnreadKind::NeedsInput => (Color::Warning, "An agent needs your input"),
        UnreadKind::Done => (Color::Accent, "An agent finished while you were away"),
    };
    div()
        .id(SharedString::from(format!("icarus-unread-{workspace_id}")))
        .tooltip(ui::Tooltip::text(tooltip))
        .child(div().size_2().rounded_full().bg(color.color(cx)))
}

pub const MOON_ICON: &str = "icons/icarus/moon.svg";
const STACK_ICON: &str = "icons/icarus/layers.svg";
const FINISHED_ICON: &str = "icons/icarus/circle_check.svg";
const CLAUDE_ORANGE: u32 = 0xd97757;

/// How far a card's rows below the title are indented, so they line up with the title text.
pub const CARD_INDENT: Pixels = px(20.);

/// The colour GitHub gives a pull request in `state`.
pub fn pr_state_color(state: &str) -> Color {
    match state {
        "merged" => Color::Accent,
        "closed" => Color::Error,
        "draft" => Color::Muted,
        _ => Color::Success,
    }
}

/// What the card's PR icon says on hover.
pub fn pr_tooltip(pr: &PrSummary) -> String {
    let state = if pr.state.is_empty() { "open" } else { &pr.state };
    let mut text = format!("#{} · {state}", pr.number);
    match pr.checks.as_deref() {
        Some("failure") => text.push_str(" · checks failing"),
        Some("pending") => text.push_str(" · checks running"),
        Some("success") => text.push_str(" · checks passed"),
        _ => {}
    }
    if pr.in_merge_queue {
        text.push_str(" · in merge queue");
    }
    text
}

/// `1599` as `1,599`, like the diff counts on Orca's cards.
pub fn with_thousands(count: u64) -> String {
    let digits = count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// `10h`, `3d`: how long ago `at_ms` was, as short as the card's activity line needs.
pub fn short_age(at_ms: u64, now_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(at_ms) / 1000;
    match seconds {
        0..60 => "now".into(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

pub fn stack_icon() -> Icon {
    Icon::from_path(STACK_ICON)
        .size(IconSize::XSmall)
        .color(Color::Muted)
}

pub fn agent_state_icon(agent: &AgentSummary) -> Icon {
    let icon = match agent.state.as_str() {
        "working" => Icon::new(IconName::LoadCircle).color(Color::Accent),
        "idle" if agent.turns > 0 => Icon::from_path(FINISHED_ICON).color(Color::Success),
        "sleeping" => Icon::from_path(MOON_ICON).color(Color::Muted),
        "exited" => Icon::new(IconName::XCircle).color(Color::Muted),
        _ => Icon::from_path(crate::status_style::icon_path("", Some("circle-dashed")))
            .color(Color::Muted),
    };
    icon.size(IconSize::XSmall)
}

pub fn agent_provider_icon(provider: &str) -> Icon {
    let icon = match provider {
        "claude" => {
            Icon::new(IconName::AiClaude).color(Color::Custom(gpui::rgb(CLAUDE_ORANGE).into()))
        }
        "codex" => Icon::new(IconName::AiOpenAi).color(Color::Default),
        _ => Icon::new(IconName::Sparkle).color(Color::Muted),
    };
    icon.size(IconSize::XSmall)
}

/// A small rounded tag on a card: status, Linear label, project or merge queue.
pub fn chip(cx: &App) -> gpui::Div {
    h_flex()
        .flex_none()
        .h(px(18.))
        .px_1()
        .gap_1()
        .rounded_sm()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().element_background)
}

/// A card in the panel: its server and workspace id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardKey {
    pub server: ServerRef,
    pub id: String,
}

/// The cards picked with ⌘-click (Ctrl elsewhere) and ⇧-click; the anchor is where a ⇧-range starts.
#[derive(Default, Debug)]
pub struct CardSelection {
    pub keys: Vec<CardKey>,
    pub anchor: Option<CardKey>,
}

/// Cards being dragged to a new place in the Manual order: every selected card when the dragged one
/// is selected, else just it.
#[derive(Clone, Debug)]
pub struct DraggedCards {
    pub server: ServerRef,
    pub ids: Vec<String>,
    pub title: String,
}

impl Render for DraggedCards {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label = if self.ids.len() > 1 {
            format!("{} workspaces", self.ids.len())
        } else {
            self.title.clone()
        };
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().elevated_surface_background)
            .child(Label::new(label).size(LabelSize::Small))
    }
}

/// The order after dropping `moving` on `target`: the moved cards keep their relative order and take the
/// target's place, after it when they came from above (so a drag can reach the end) and before it otherwise.
pub fn order_after_drop(order: &[String], moving: &[String], target: &str) -> Option<Vec<String>> {
    if moving.iter().any(|id| id == target) {
        return None;
    }
    let from = order.iter().position(|id| moving.contains(id))?;
    let to = order.iter().position(|id| id == target)?;
    let moved: Vec<String> = order
        .iter()
        .filter(|id| moving.contains(id))
        .cloned()
        .collect();
    let mut rest: Vec<String> = order
        .iter()
        .filter(|id| !moving.contains(id))
        .cloned()
        .collect();
    let at = rest.iter().position(|id| id == target)? + usize::from(from < to);
    rest.splice(at..at, moved);
    Some(rest)
}

/// `Sleep 1 Workspace`, `Sleep 3 Workspaces` (Orca's batch labels).
pub fn count_label(verb: &str, count: usize) -> String {
    if count == 1 {
        format!("{verb} 1 Workspace")
    } else {
        format!("{verb} {count} Workspaces")
    }
}

struct SortChangedToast;

impl WorkspacesPanel {
    pub fn agents_expanded(&self, workspace_id: &str) -> bool {
        self.expanded_agents.contains(workspace_id)
    }

    pub fn toggle_agents(&mut self, workspace_id: &str, cx: &mut Context<Self>) {
        if !self.expanded_agents.remove(workspace_id) {
            self.expanded_agents.insert(workspace_id.to_owned());
        }
        cx.notify();
    }

    /// The card's agent row: the latest agent's finished turn in one line when there is one, else a pill
    /// with the lead agent and how many more; either opens the full list.
    pub(crate) fn render_agents(
        &self,
        workspace: &WorkspaceSummary,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.view.shows("inline-agents") {
            return None;
        }
        let latest = workspace
            .agents
            .iter()
            .rev()
            .max_by_key(|agent| agent.last_activity_ms)?;
        let expanded = self.agents_expanded(&workspace.id);
        let toggle = {
            let workspace_id = workspace.id.clone();
            cx.listener(move |panel, _: &gpui::ClickEvent, _, cx| {
                cx.stop_propagation();
                panel.toggle_agents(&workspace_id, cx);
            })
        };
        let colors = cx.theme().colors();
        let row_id = SharedString::from(format!("icarus-agents-{}", workspace.id));
        let row = match latest.finished_summary().filter(|_| !expanded) {
            Some(summary) => {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |elapsed| elapsed.as_millis() as u64);
                h_flex()
                    .id(row_id.clone())
                    .debug_selector(|| row_id.to_string())
                    .min_w_0()
                    .gap_1()
                    .cursor_pointer()
                    .child(agent_state_icon(latest))
                    .child(agent_provider_icon(&latest.provider))
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(summary.to_owned())
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    )
                    .when(latest.last_activity_ms > 0, |row| {
                        row.child(
                            Label::new(short_age(latest.last_activity_ms, now_ms))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .on_click(toggle)
            }
            None => {
                let lead = workspace
                    .agents
                    .iter()
                    .find(|agent| agent.state == "working")
                    .unwrap_or(latest);
                let more = workspace.agents.len() - 1;
                h_flex()
                    .id(row_id.clone())
                    .debug_selector(|| row_id.to_string())
                    .h(px(22.))
                    .px_1p5()
                    .gap_1()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border_variant)
                    .bg(colors.element_background.opacity(0.5))
                    .cursor_pointer()
                    .hover(|style| style.bg(colors.element_hover))
                    .child(agent_state_icon(lead))
                    .child(agent_provider_icon(&lead.provider))
                    .when(more > 0, |pill| {
                        pill.child(
                            Label::new(format!("+{more}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .on_click(toggle)
            }
        };
        let list = expanded.then(|| {
            v_flex().pl_1p5().gap_0p5().children(workspace.agents.iter().map(|agent| {
                let title = agent
                    .title
                    .clone()
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| crate::accounts::provider_name(&agent.provider).to_owned());
                h_flex()
                    .gap_1()
                    .child(agent_state_icon(agent))
                    .child(agent_provider_icon(&agent.provider))
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(title)
                                .size(LabelSize::XSmall)
                                .truncate(),
                        ),
                    )
                    .child(
                        Label::new(agent.state.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
            }))
        });
        Some(
            v_flex()
                .gap_0p5()
                .child(row)
                .children(list)
                .into_any_element(),
        )
    }

    /// The workspaces the text filter keeps, in the chosen sort order (before grouping and pins).
    pub(crate) fn arranged_workspaces(&self) -> Vec<WorkspaceSummary> {
        self.arranged(&self.workspaces)
    }

    /// `workspaces` filtered and sorted as the panel shows them, pinned ones first; for a saved server's section.
    pub(crate) fn arranged_remote(&self, workspaces: &[WorkspaceSummary]) -> Vec<WorkspaceSummary> {
        let mut arranged = self.arranged(workspaces);
        crate::model::pinned_first(&mut arranged);
        arranged
    }

    fn arranged(&self, workspaces: &[WorkspaceSummary]) -> Vec<WorkspaceSummary> {
        let mut workspaces: Vec<WorkspaceSummary> = workspaces
            .iter()
            .filter(|workspace| matches_filter(workspace, &self.filter_query))
            .cloned()
            .collect();
        sort_workspaces(&mut workspaces, self.view.sort_by, |id| {
            self.project_names.get(id).cloned().unwrap_or_default()
        });
        workspaces
    }

    /// Every card in the order the panel draws them: this server's sections, then each saved server's.
    pub(crate) fn card_order(&self, cx: &App) -> Vec<(CardKey, WorkspaceSummary)> {
        let mut cards: Vec<(CardKey, WorkspaceSummary)> = self
            .sections(cx)
            .into_iter()
            .flat_map(|(_, _, workspaces)| workspaces)
            .map(|workspace| {
                (
                    CardKey {
                        server: ServerRef::Primary,
                        id: workspace.id.clone(),
                    },
                    workspace,
                )
            })
            .collect();
        for server in self.remote.read(cx).servers() {
            for workspace in self.arranged_remote(&server.workspaces) {
                cards.push((
                    CardKey {
                        server: ServerRef::Saved(server.name.clone()),
                        id: workspace.id.clone(),
                    },
                    workspace,
                ));
            }
        }
        cards
    }

    /// What every card does: a click opens it (or changes the selection with ⌘/⇧), a selected card is
    /// highlighted, and cards drag onto each other to reorder.
    pub(crate) fn with_card_behavior(
        &self,
        key: CardKey,
        title: String,
        card: gpui::Stateful<gpui::Div>,
        open: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let selected = self.is_selected(&key);
        let dragged = self.dragged_cards(&key, title);
        let click_key = key.clone();
        card.when(selected, |card| {
            card.bg(cx.theme().colors().element_selected)
        })
        .on_click(
            cx.listener(move |panel, event: &gpui::ClickEvent, window, cx| {
                if !panel.select_on_click(&click_key, event.modifiers(), cx) {
                    open(panel, window, cx);
                }
            }),
        )
        .on_drag(dragged, |dragged, _, _, cx| cx.new(|_| dragged.clone()))
        .drag_over::<DraggedCards>(|style, _, _, cx| {
            style.bg(cx.theme().colors().drop_target_background)
        })
        .on_drop(cx.listener(move |panel, dragged: &DraggedCards, _, cx| {
            panel.drop_cards(dragged, &key, cx)
        }))
    }

    fn move_to_status(&mut self, id: String, status: Option<String>, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspaces.iter_mut().find(|w| w.id == id) {
            workspace.status = status.clone();
        }
        if let Some(request) = crate::review::request(
            crate::routing::client_for(&ServerRef::Primary, cx),
            "workspace.set",
            serde_json::json!({ "id": id, "status": status.unwrap_or_default() }),
            cx,
        ) {
            request.detach_and_log_err(cx);
        }
    }

    pub fn selection(&self) -> &[CardKey] {
        &self.selection.keys
    }

    pub fn is_selected(&self, key: &CardKey) -> bool {
        self.selection.keys.contains(key)
    }

    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if !self.selection.keys.is_empty() {
            self.selection.keys.clear();
            cx.notify();
        }
        self.selection.anchor = None;
    }

    /// Orca's selection intent: ⌘ (Ctrl elsewhere) toggles the card, ⇧ selects from the anchor to it in
    /// drawn order, and a plain click clears the selection. True when the click only changed the selection.
    pub fn select_on_click(
        &mut self,
        key: &CardKey,
        modifiers: Modifiers,
        cx: &mut Context<Self>,
    ) -> bool {
        if modifiers.shift {
            let order: Vec<CardKey> = self
                .card_order(cx)
                .into_iter()
                .map(|(key, _)| key)
                .collect();
            let anchor = self
                .selection
                .anchor
                .clone()
                .filter(|anchor| order.contains(anchor))
                .unwrap_or_else(|| key.clone());
            let (Some(from), Some(to)) = (
                order.iter().position(|item| *item == anchor),
                order.iter().position(|item| item == key),
            ) else {
                return true;
            };
            let (start, end) = if from <= to { (from, to) } else { (to, from) };
            self.selection.keys = order[start..=end].to_vec();
            self.selection.anchor = Some(anchor);
            cx.notify();
            return true;
        }
        if modifiers.secondary(cx) {
            if let Some(index) = self.selection.keys.iter().position(|item| item == key) {
                self.selection.keys.remove(index);
            } else {
                self.selection.keys.push(key.clone());
            }
            self.selection.anchor = Some(key.clone());
            cx.notify();
            return true;
        }
        self.clear_selection(cx);
        false
    }

    /// The cards a right-click on `key` acts on together: the selection when it holds `key` and another.
    pub fn bulk_targets(&self, key: &CardKey, cx: &App) -> Vec<(ServerRef, WorkspaceSummary)> {
        if self.selection.keys.len() < 2 || !self.is_selected(key) {
            return Vec::new();
        }
        self.card_order(cx)
            .into_iter()
            .filter(|(key, _)| self.is_selected(key))
            .map(|(key, workspace)| (key.server, workspace))
            .collect()
    }

    /// What dragging the card `key` carries: the selected cards of its server when it is selected.
    pub fn dragged_cards(&self, key: &CardKey, title: String) -> DraggedCards {
        let ids = if self.is_selected(key) {
            self.selection
                .keys
                .iter()
                .filter(|item| item.server == key.server)
                .map(|item| item.id.clone())
                .collect()
        } else {
            vec![key.id.clone()]
        };
        DraggedCards {
            server: key.server.clone(),
            ids,
            title,
        }
    }

    /// Orca's manual sort: the dropped cards take the target's place, the new order is stored on their
    /// server, and the list switches to Manual. Grouped by status, a drop into another status moves them there.
    pub fn drop_cards(&mut self, dragged: &DraggedCards, target: &CardKey, cx: &mut Context<Self>) {
        if dragged.server != target.server {
            return;
        }
        let cards: Vec<(CardKey, WorkspaceSummary)> = self
            .card_order(cx)
            .into_iter()
            .filter(|(key, _)| key.server == target.server)
            .collect();
        let order: Vec<String> = cards.iter().map(|(key, _)| key.id.clone()).collect();
        let Some(new_order) = order_after_drop(&order, &dragged.ids, &target.id) else {
            return;
        };
        if target.server == ServerRef::Primary
            && self.view.group_by == GroupBy::Status
            && let Some((_, target_workspace)) = cards.iter().find(|(key, _)| key.id == target.id)
        {
            let status = target_workspace.status.clone();
            for (key, workspace) in &cards {
                if dragged.ids.contains(&key.id) && workspace.status != status {
                    self.move_to_status(key.id.clone(), status.clone(), cx);
                }
            }
        }
        if target.server == ServerRef::Primary {
            let top = new_order.len() as i64;
            for workspace in &mut self.workspaces {
                if let Some(index) = new_order.iter().position(|id| id == &workspace.id) {
                    workspace.manual_order = Some((top - index as i64) * 1000);
                }
            }
        }
        if self.view.sort_by != SortBy::Manual {
            self.set_sort_by(SortBy::Manual, cx);
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.show_toast(
                        workspace::Toast::new(
                            workspace::notifications::NotificationId::unique::<SortChangedToast>(),
                            "Sort changed to Manual: workspaces stay where you drop them.",
                        ),
                        cx,
                    )
                })
                .log_err();
        }
        cx.notify();
        let client = crate::routing::client_for(&target.server, cx);
        let Some(request) = crate::review::request(
            client,
            "workspace.reorder",
            serde_json::json!({ "ids": new_order }),
            cx,
        ) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            request.await?;
            this.update(cx, |panel, cx| panel.refresh(cx))
        })
        .detach_and_log_err(cx);
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
            !defaults.shows_status_chip(),
            "grouped by status, the section header already says it"
        );
        let by_pr = CardView {
            group_by: GroupBy::PrStatus,
            ..defaults
        };
        assert!(
            by_pr.shows_status_chip(),
            "on by default in other groupings"
        );
        let hidden = CardView {
            local_fields: Some(by_pr.toggled(STATUS_FIELD)),
            ..by_pr
        };
        assert!(!hidden.shows_status_chip(), "turned off, it never shows");

        let old_alias = CardView::from_settings(
            &json!(["status", "pr"]),
            &json!({ "groupBy": "pr-status" }),
            &json!(null),
        );
        assert!(old_alias.shows(STATUS_FIELD), "older settings say `status`");
        assert_eq!(old_alias.toggled(STATUS_FIELD), ["pr"]);
        assert!(
            !CardView {
                fields: old_alias.toggled(STATUS_FIELD),
                ..old_alias
            }
            .shows_status_chip()
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
    fn every_card_field_has_a_default_hotkey_and_this_apps_fields_win() {
        for keymap in [
            include_str!("../../../assets/keymaps/default-macos.json"),
            include_str!("../../../assets/keymaps/default-linux.json"),
        ] {
            for field in CARD_FIELDS {
                let binding = format!(
                    r#"["icarus::ToggleCardField", {{ "field": "{}" }}]"#,
                    field.id
                );
                assert!(keymap.contains(&binding), "no hotkey for {}", field.id);
            }
        }

        let mut view = CardView::from_settings(&json!(["pr"]), &json!(null), &json!(null));
        assert_eq!(view.toggled("ports"), ["pr", "ports"]);
        view.local_fields = Some(vec!["ports".into()]);
        assert!(view.shows("ports") && !view.shows("pr"));
        assert_eq!(
            view.toggled("ports"),
            Vec::<String>::new(),
            "toggling edits this app's list"
        );
    }

    fn sections(panel: &Entity<WorkspacesPanel>, cx: &mut gpui::VisualTestContext) -> Vec<String> {
        panel.read_with(cx, |panel, cx| {
            panel
                .sections(cx)
                .into_iter()
                .map(|(_, title, items)| {
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
    fn drops_take_the_targets_place() {
        let order: Vec<String> = ["a", "b", "c", "d"].map(String::from).to_vec();
        let drop = |moving: &[&str], target: &str| {
            let moving: Vec<String> = moving.iter().map(|id| id.to_string()).collect();
            order_after_drop(&order, &moving, target).map(|order| order.join(","))
        };
        assert_eq!(
            drop(&["a"], "c").as_deref(),
            Some("b,c,a,d"),
            "from above, after it"
        );
        assert_eq!(
            drop(&["d"], "b").as_deref(),
            Some("a,d,b,c"),
            "from below, before it"
        );
        assert_eq!(
            drop(&["a"], "d").as_deref(),
            Some("b,c,d,a"),
            "so the end is reachable"
        );
        assert_eq!(
            drop(&["c", "a"], "d").as_deref(),
            Some("b,d,a,c"),
            "several keep their order"
        );
        assert_eq!(drop(&["b"], "b"), None);
        assert_eq!(count_label("Sleep", 1), "Sleep 1 Workspace");
        assert_eq!(count_label("Delete", 3), "Delete 3 Workspaces");
    }

    fn key(id: &str) -> CardKey {
        CardKey {
            server: ServerRef::Primary,
            id: format!("ws-{id}"),
        }
    }

    fn names(keys: &[CardKey]) -> Vec<&str> {
        keys.iter()
            .map(|key| key.id.trim_start_matches("ws-"))
            .collect()
    }

    #[gpui::test]
    async fn modifier_clicks_select_and_dragging_reorders_on_the_server(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_, panel, mut cx) = crate::tests::panel_window(listed(), cx).await;
        let cx = &mut cx;
        let calls = cx.update(|_, cx| crate::review::tests::stub_rpc(cx, |_, _| Ok(json!({}))));
        let secondary = cx.update(|_, cx| Modifiers::secondary_key(cx));
        panel.update(cx, |panel, cx| {
            assert!(panel.select_on_click(&key("beta"), secondary, cx));
            assert!(panel.select_on_click(&key("gamma"), Modifiers::shift(), cx));
        });
        assert_eq!(
            panel.read_with(cx, |panel, _| names(panel.selection()).join(",")),
            "beta,alpha,gamma",
            "⇧ selects the drawn range from the anchor"
        );
        let bulk = panel.read_with(cx, |panel, cx| {
            panel
                .bulk_targets(&key("alpha"), cx)
                .into_iter()
                .map(|(_, workspace)| workspace.name)
                .collect::<Vec<_>>()
        });
        assert_eq!(bulk, ["beta", "alpha", "gamma"]);
        assert!(
            panel.read_with(cx, |panel, cx| panel
                .bulk_targets(&key("delta"), cx)
                .is_empty()),
            "a right-click outside the selection is for that card alone"
        );
        panel.update(cx, |panel, cx| {
            assert!(panel.select_on_click(&key("alpha"), secondary, cx));
        });
        assert_eq!(
            panel.read_with(cx, |panel, _| names(panel.selection()).join(",")),
            "beta,gamma",
            "⌘ toggles one card"
        );

        cx.dispatch_action(SetWorkspaceSort {
            sort_by: "name".into(),
        });
        assert_eq!(sections(&panel, cx), ["todo: delta,alpha,beta,gamma"]);
        panel.update(cx, |panel, cx| {
            let dragged = panel.dragged_cards(&key("beta"), "beta".into());
            assert_eq!(
                names(
                    &dragged
                        .ids
                        .iter()
                        .map(|id| CardKey {
                            server: ServerRef::Primary,
                            id: id.clone()
                        })
                        .collect::<Vec<_>>()
                ),
                ["beta", "gamma"]
            );
            panel.drop_cards(&dragged, &key("delta"), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            crate::review::tests::calls_to(&calls, "workspace.reorder"),
            [json!({ "ids": ["ws-beta", "ws-gamma", "ws-delta", "ws-alpha"] })],
            "dragged up onto delta, the selected cards go before it"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.view().sort_by),
            SortBy::Manual,
            "a drag switches to Manual, as in Orca"
        );
        assert_eq!(sections(&panel, cx), ["todo: beta,gamma,delta,alpha"]);

        panel.update(cx, |panel, cx| {
            assert!(!panel.select_on_click(&key("alpha"), Modifiers::default(), cx));
            assert!(panel.selection().is_empty(), "a plain click clears it");
        });
    }

    #[gpui::test]
    async fn dropping_into_another_status_moves_the_card_there(cx: &mut gpui::TestAppContext) {
        let mut workspaces = listed();
        workspaces[1].status = Some("done".into());
        let (_, panel, mut cx) = crate::tests::panel_window(workspaces, cx).await;
        let cx = &mut cx;
        let calls = cx.update(|_, cx| crate::review::tests::stub_rpc(cx, |_, _| Ok(json!({}))));
        panel.update(cx, |panel, cx| {
            panel.set_status_order(vec!["todo".into(), "done".into()], cx);
            let dragged = panel.dragged_cards(&key("gamma"), "gamma".into());
            panel.drop_cards(&dragged, &key("alpha"), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| {
                panel
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.name == "gamma")
                    .and_then(|workspace| workspace.status.clone())
            }),
            Some("done".into())
        );
        assert_eq!(
            crate::review::tests::calls_to(&calls, "workspace.set"),
            [json!({ "id": "ws-gamma", "status": "done" })]
        );
        assert_eq!(
            crate::review::tests::calls_to(&calls, "workspace.reorder").len(),
            1
        );
    }

    #[gpui::test]
    async fn saved_server_sections_sort_and_lead_with_pins(cx: &mut gpui::TestAppContext) {
        let (_, panel, mut cx) = crate::tests::panel_window(Vec::new(), cx).await;
        let cx = &mut cx;
        let store = panel.read_with(cx, |panel, _| panel.remote_servers().clone());
        let mut remote = listed();
        remote[3].pinned = true;
        store.update(cx, |store, cx| {
            store.set_saved(
                &[crate::servers::tests::saved("desk", "100.64.0.2:4000")],
                cx,
            );
            store.set_workspaces("desk", remote, cx);
        });
        cx.dispatch_action(SetWorkspaceSort {
            sort_by: "name".into(),
        });
        let order = panel.read_with(cx, |panel, cx| {
            panel
                .card_order(cx)
                .into_iter()
                .map(|(key, workspace)| {
                    format!(
                        "{}:{}",
                        key.server.saved_name().unwrap_or("-"),
                        workspace.name
                    )
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            order,
            ["desk:delta", "desk:alpha", "desk:beta", "desk:gamma"],
            "the pinned card leads, the rest follow the sort"
        );
        panel.update(cx, |panel, cx| {
            let saved_key = |id: &str| CardKey {
                server: ServerRef::Saved("desk".into()),
                id: format!("ws-{id}"),
            };
            assert!(panel.select_on_click(&saved_key("alpha"), Modifiers::shift(), cx));
            assert!(panel.select_on_click(&saved_key("gamma"), Modifiers::shift(), cx));
            assert_eq!(panel.selection().len(), 3);
        });
    }

    #[test]
    fn card_numbers_and_agent_lines_read_like_orcas() {
        assert_eq!(with_thousands(0), "0");
        assert_eq!(with_thousands(999), "999");
        assert_eq!(with_thousands(1599), "1,599");
        assert_eq!(with_thousands(1_234_567), "1,234,567");
        let hour_ms = 3_600_000;
        assert_eq!(short_age(0, 30_000), "now");
        assert_eq!(short_age(0, 10 * hour_ms), "10h");
        assert_eq!(short_age(0, 50 * hour_ms), "2d");

        let agent = |state: &str, turns: u64, title: Option<&str>| AgentSummary {
            state: state.into(),
            turns,
            title: title.map(str::to_string),
            ..AgentSummary::default()
        };
        assert_eq!(
            agent("idle", 2, Some("Ready for review")).finished_summary(),
            Some("Ready for review")
        );
        assert_eq!(
            agent("working", 2, Some("Ready")).finished_summary(),
            None,
            "a working agent gets the pill"
        );
        assert_eq!(
            agent("idle", 0, Some("Agent")).finished_summary(),
            None,
            "nothing finished yet"
        );
        assert_eq!(agent("idle", 1, Some("  ")).finished_summary(), None);
        let pr = PrSummary {
            number: 4143,
            state: "open".into(),
            checks: Some("failure".into()),
            ..PrSummary::default()
        };
        assert_eq!(pr_tooltip(&pr), "#4143 · open · checks failing");
    }

    #[gpui::test]
    async fn the_agent_row_opens_the_list_and_quiet_found_worktrees_sleep(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::tests::summary;
        let mut busy = summary("busy", "/w/busy", None);
        busy.agents = vec![
            AgentSummary {
                id: "a".into(),
                state: "working".into(),
                provider: "claude".into(),
                ..AgentSummary::default()
            },
            AgentSummary {
                id: "b".into(),
                state: "idle".into(),
                provider: "codex".into(),
                ..AgentSummary::default()
            },
        ];
        let mut asleep = summary("asleep", "/w/asleep", None);
        asleep.sleeping = true;
        let found = summary("found", "/w/found", None);
        let mut found_with_pr = with_pr(summary("found-pr", "/w/found-pr", None), 3, "open");
        found_with_pr.id = "ws-found-pr".into();
        let (_, panel, mut cx) =
            crate::tests::panel_window(vec![busy, asleep, found, found_with_pr], cx).await;
        let cx = &mut cx;
        panel.update(cx, |panel, cx| {
            panel.discovered = crate::worktree_inbox::Discovered::new(
                &json!([
                    { "id": "ws-found", "origin": "claude" },
                    { "id": "ws-found-pr", "origin": "claude" },
                    { "id": "ws-busy", "origin": "native" },
                ]),
                json!([]),
            );
            cx.notify();
        });
        cx.run_until_parked();
        let inactive = panel.read_with(cx, |panel, _| {
            panel
                .workspaces
                .iter()
                .filter(|workspace| panel.is_inactive(workspace))
                .map(|workspace| workspace.name.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            inactive,
            ["asleep", "found"],
            "a found worktree with a PR is not dimmed"
        );

        cx.simulate_resize(gpui::size(px(1280.), px(800.)));
        panel.update_in(cx, |panel, window, cx| {
            panel
                .workspace
                .update(cx, |workspace, cx| {
                    workspace.open_panel::<WorkspacesPanel>(window, cx)
                })
                .ok();
        });
        cx.run_until_parked();
        let pill = cx
            .debug_bounds("icarus-agents-ws-busy")
            .expect("the agent pill is drawn");
        cx.simulate_click(pill.center(), Modifiers::none());
        cx.run_until_parked();
        assert!(panel.read_with(cx, |panel, _| panel.agents_expanded("ws-busy")));
        assert!(
            cx.update(|_, cx| cx
                .try_global::<crate::OpeningWorkspaces>()
                .is_none_or(|opening| opening.latest.is_none())),
            "the click stays on the agent row instead of opening the workspace"
        );
        let pill = cx
            .debug_bounds("icarus-agents-ws-busy")
            .expect("still drawn");
        cx.simulate_click(pill.center(), Modifiers::none());
        cx.run_until_parked();
        assert!(!panel.read_with(cx, |panel, _| panel.agents_expanded("ws-busy")));
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
