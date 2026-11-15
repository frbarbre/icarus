use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use file_icons::FileIcons;
use git_ui::branch_diff::BranchDiff;
use git_ui::commit_view::{CommitView, CommitViewOptions};
use git_ui::project_diff::ProjectDiff;
use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Render,
    ScrollStrategy, SharedString, Task, UniformListScrollHandle, WeakEntity, Window, actions,
    uniform_list,
};
use gpui_tokio::Tokio;
use icarus_client::Client;
use project::Project;
use serde::Deserialize;
use ui::{ButtonLike, ContextMenu, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::item::{Item, ItemEvent, SerializableItem};
use workspace::{ItemId, Pane, SplitDirection, Workspace, WorkspaceId};

use crate::AppConnection;

actions!(
    icarus,
    [
        /// Shows the Changes tab, creating it if needed.
        OpenChanges,
        /// Narrows the Changes list to uncommitted changes, or back to everything (⌥⌘U).
        ToggleUncommittedChanges,
        /// Shows what the newest agent turn changed, or back to everything (⌥⇧⌘T).
        OpenLatestAgentTurn,
        /// Opens the next file in the Changes list (⌥F7).
        NextChangedFile,
        /// Opens the previous file in the Changes list (⌥⇧F7).
        PreviousChangedFile,
    ]
);

/// How often an open Changes tab re-reads the list; the server coalesces the git reads behind it.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// Which rows the list shows (docs/source-control.md §2.1). One per tab, so the shortcuts flip it in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangesScope {
    Base,
    Uncommitted,
    LatestTurn,
    Turn(String),
    Commit(String),
}

impl ChangesScope {
    fn wire(&self) -> String {
        match self {
            Self::Base => "base".into(),
            Self::Uncommitted => "uncommitted".into(),
            Self::LatestTurn => "turn:latest".into(),
            Self::Turn(name) => format!("turn:{name}"),
            Self::Commit(sha) => format!("commit:{sha}"),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Listing {
    #[serde(default)]
    pub compare: Compare,
    #[serde(default)]
    pub turn: Option<TurnInfo>,
    #[serde(default)]
    pub commit: Option<CommitRef>,
    #[serde(default)]
    pub rows: Vec<Row>,
    #[serde(default)]
    pub totals: Totals,
    #[serde(default)]
    pub unfiltered: Totals,
    #[serde(default)]
    pub facets: Vec<Facet>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Compare {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub base: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TurnInfo {
    pub name: String,
    pub oid: String,
    #[serde(default)]
    pub meta: Option<TurnMeta>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TurnMeta {
    #[serde(default)]
    pub prompt: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CommitRef {
    pub oid: String,
    #[serde(default)]
    pub subject: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Row {
    pub path: String,
    pub status: String,
    #[serde(default)]
    pub old_path: Option<String>,
    #[serde(default)]
    pub added: Option<u64>,
    #[serde(default)]
    pub removed: Option<u64>,
    #[serde(default)]
    pub binary: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
pub struct Totals {
    #[serde(default)]
    pub files: usize,
    #[serde(default)]
    pub added: u64,
    #[serde(default)]
    pub removed: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Facet {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub values: Vec<FacetValue>,
    #[serde(default)]
    pub active: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct FacetValue {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub present: bool,
    #[serde(default)]
    pub hidden: bool,
}

impl Facet {
    /// The pill text while this facet hides something present: the names still shown (§2.2).
    fn shown_label(&self) -> Option<String> {
        if !self.active {
            return None;
        }
        let shown: Vec<&str> = self
            .values
            .iter()
            .filter(|value| value.present && !value.hidden)
            .map(|value| value.label.as_str())
            .collect();
        Some(if shown.is_empty() {
            format!("No {}", self.label.to_lowercase())
        } else {
            shown.join(", ")
        })
    }
}

/// The trigger's label while any facet is active, e.g. `Implementation · Frontend`.
fn filter_label(facets: &[Facet]) -> Option<String> {
    let parts: Vec<String> = facets.iter().filter_map(Facet::shown_label).collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn status_letter(status: &str) -> &'static str {
    match status {
        "added" => "A",
        "deleted" => "D",
        "renamed" => "R",
        "copied" => "C",
        _ => "M",
    }
}

fn status_color(status: &str) -> Color {
    match status {
        "added" => Color::Created,
        "deleted" => Color::Deleted,
        "renamed" | "copied" => Color::Accent,
        _ => Color::Modified,
    }
}

/// The scope badge's text and tooltip (§1.5), or `None` for the default scope.
fn scope_badge(
    scope: &ChangesScope,
    listing: Option<&Listing>,
) -> Option<(String, Option<String>)> {
    let prompt = listing
        .and_then(|listing| listing.turn.as_ref())
        .and_then(|turn| turn.meta.as_ref())
        .map(|meta| meta.prompt.clone());
    match scope {
        ChangesScope::Base => None,
        ChangesScope::Uncommitted => Some(("Uncommitted".into(), None)),
        ChangesScope::LatestTurn => Some(("Latest turn".into(), prompt)),
        ChangesScope::Turn(_) => Some(("Historical changes".into(), prompt)),
        ChangesScope::Commit(sha) => Some((
            format!("Commit {}", &sha[..sha.len().min(7)]),
            listing
                .and_then(|listing| listing.commit.as_ref())
                .map(|commit| commit.subject.clone()),
        )),
    }
}

/// The Source Control "Changes" list as a tab: every file that differs from the workspace's base, one row each.
pub struct ChangesTab {
    workspace: WeakEntity<Workspace>,
    root: PathBuf,
    icarus_workspace: Option<String>,
    scope: ChangesScope,
    listing: Option<Listing>,
    error: Option<SharedString>,
    selected: Option<usize>,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
    refreshing: Option<Task<()>>,
    _poll: Task<()>,
}

impl ChangesTab {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(REFRESH_INTERVAL).await;
            }
        });
        Self {
            workspace,
            root,
            icarus_workspace: None,
            scope: ChangesScope::Base,
            listing: None,
            error: None,
            selected: None,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            refreshing: None,
            _poll: poll,
        }
    }

    /// A tab showing `listing` without a server, for visual tests.
    pub fn preview(
        listing: Listing,
        scope: ChangesScope,
        selected: Option<usize>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            workspace: WeakEntity::new_invalid(),
            root: PathBuf::new(),
            icarus_workspace: Some("preview".into()),
            scope,
            listing: Some(listing),
            error: None,
            selected,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            refreshing: None,
            _poll: Task::ready(()),
        }
    }

    pub fn scope(&self) -> &ChangesScope {
        &self.scope
    }

    pub fn listing(&self) -> Option<&Listing> {
        self.listing.as_ref()
    }

    pub fn set_scope(&mut self, scope: ChangesScope, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            self.selected = None;
            cx.emit(ItemEvent::UpdateTab);
        }
        self.refresh(cx);
        cx.notify();
    }

    /// Flips `scope` on, or back to everything when it's already showing (Orca's toggles).
    pub fn toggle_scope(&mut self, scope: ChangesScope, cx: &mut Context<Self>) {
        let next = if self.scope == scope {
            ChangesScope::Base
        } else {
            scope
        };
        self.set_scope(next, cx);
    }

    fn client(cx: &App) -> Option<Arc<Client>> {
        cx.try_global::<AppConnection>()
            .and_then(|connection| connection.client.clone())
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing.is_some() {
            return;
        }
        let Some(client) = Self::client(cx) else {
            self.error = Some("Not connected to the Icarus server".into());
            cx.notify();
            return;
        };
        let root = self.root.clone();
        let known = self.icarus_workspace.clone();
        let scope = self.scope.clone();
        let fetch = Tokio::spawn_result(cx, async move {
            let id = match known {
                Some(id) => id,
                None => workspace_id_for_root(client.clone(), &root)
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!("{} isn't an Icarus workspace", root.display())
                    })?,
            };
            let value = client
                .request(
                    "workspace.changes",
                    serde_json::json!({ "id": id, "scope": scope.wire() }),
                )
                .await?;
            anyhow::Ok((id, serde_json::from_value::<Listing>(value)?))
        });
        self.refreshing = Some(cx.spawn(async move |this, cx| {
            let result = fetch.await;
            this.update(cx, |this, cx| {
                this.refreshing = None;
                match result {
                    Ok((id, listing)) => {
                        this.icarus_workspace = Some(id);
                        this.error = None;
                        if let Some(selected) = this.selected
                            && selected >= listing.rows.len()
                        {
                            this.selected = listing.rows.len().checked_sub(1);
                        }
                        if this.listing.as_ref() != Some(&listing) {
                            this.listing = Some(listing);
                            cx.emit(ItemEvent::UpdateTab);
                        }
                    }
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn rows(&self) -> &[Row] {
        self.listing
            .as_ref()
            .map(|listing| listing.rows.as_slice())
            .unwrap_or_default()
    }

    /// Hides or shows one facet value for every client (the server persists it), updating the list at once.
    fn toggle_facet_value(&mut self, facet: String, value: String, cx: &mut Context<Self>) {
        let Some(id) = self.icarus_workspace.clone() else {
            return;
        };
        let mut now_hidden = true;
        if let Some(listing) = &mut self.listing
            && let Some(entry) = listing.facets.iter_mut().find(|entry| entry.id == facet)
        {
            if let Some(item) = entry.values.iter_mut().find(|item| item.id == value) {
                item.hidden = !item.hidden;
                now_hidden = item.hidden;
            }
            entry.active = entry
                .values
                .iter()
                .any(|value| value.present && value.hidden);
        }
        cx.notify();
        let Some(client) = Self::client(cx) else {
            return;
        };
        let mut change = serde_json::Map::new();
        change.insert(facet, serde_json::json!([value]));
        let params = if now_hidden {
            serde_json::json!({ "id": id, "hide": change })
        } else {
            serde_json::json!({ "id": id, "show": change })
        };
        let request = Tokio::spawn_result(cx, async move {
            client.request("workspace.changes.filter", params).await
        });
        cx.spawn(async move |this, cx| {
            request.await.log_err();
            this.update(cx, |this, cx| {
                this.refreshing = None;
                this.refresh(cx)
            })
            .log_err();
        })
        .detach();
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected = Some(index);
        self.scroll.scroll_to_item(index, ScrollStrategy::Center);
        cx.notify();
    }

    /// Steps through the rows and opens the file there; a single press wraps (§5.3).
    pub fn step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.rows().len();
        if count == 0 {
            return;
        }
        let next = match (self.selected, forward) {
            (None, true) => 0,
            (None, false) => count - 1,
            (Some(current), true) => (current + 1) % count,
            (Some(current), false) => (current + count - 1) % count,
        };
        self.select(next, cx);
        self.open_selected(window, cx);
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self
            .selected
            .and_then(|index| self.rows().get(index))
            .cloned()
        else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let scope = self.scope.clone();
        let listing = self.listing.clone().unwrap_or_default();
        let this = cx.entity();
        // Deferred: the step actions run inside a Workspace update, which would double-lease it.
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                open_row(&row, &scope, &listing, &this, workspace, window, cx);
            });
        });
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let listing = self.listing.as_ref();
        let totals = listing.map(|listing| listing.totals).unwrap_or_default();
        let unfiltered = listing
            .map(|listing| listing.unfiltered)
            .unwrap_or_default();
        let base = listing.and_then(|listing| listing.compare.base.clone());
        let count_tooltip = (totals.files == unfiltered.files
            && matches!(self.scope, ChangesScope::Base | ChangesScope::Uncommitted))
        .then(|| {
            format!(
                "{} files changed vs {}",
                totals.files,
                base.clone().unwrap_or_else(|| "the base".into())
            )
        });
        let badge = scope_badge(&self.scope, listing);
        let facets = listing
            .map(|listing| listing.facets.clone())
            .unwrap_or_default();
        let present_keys = facets
            .iter()
            .flat_map(|facet| facet.values.iter())
            .filter(|value| value.present)
            .count();
        let label = filter_label(&facets);
        let weak = cx.entity().downgrade();

        h_flex()
            .h(px(32.))
            .px_2()
            .gap_1p5()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new("CHANGES")
                    .size(LabelSize::XSmall)
                    .weight(FontWeight::SEMIBOLD)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .id("changes-count")
                    .child(
                        Label::new(totals.files.to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .when_some(count_tooltip, |this, tooltip| {
                        this.tooltip(Tooltip::text(tooltip))
                    }),
            )
            .when(totals.added > 0 || totals.removed > 0, |this| {
                this.child(
                    h_flex()
                        .id("changes-total")
                        .gap_1()
                        .child(
                            Label::new(format!("+{}", totals.added))
                                .size(LabelSize::XSmall)
                                .color(Color::Created),
                        )
                        .child(
                            Label::new(format!("−{}", totals.removed))
                                .size(LabelSize::XSmall)
                                .color(Color::Deleted),
                        )
                        .tooltip(Tooltip::text(format!(
                            "{} lines added, {} lines deleted in the listed files",
                            totals.added, totals.removed
                        ))),
                )
            })
            .child(div().flex_1())
            .when_some(badge, |this, (text, tooltip)| {
                this.child(
                    h_flex()
                        .id("changes-scope")
                        .gap_0p5()
                        .pl_1p5()
                        .rounded_md()
                        .bg(cx.theme().colors().element_selected)
                        .child(Label::new(text).size(LabelSize::XSmall))
                        .child(
                            IconButton::new("changes-scope-clear", IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Show all changes"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_scope(ChangesScope::Base, cx)
                                })),
                        )
                        .when_some(tooltip, |this, tooltip| {
                            this.tooltip(Tooltip::text(tooltip))
                        }),
                )
            })
            .when(present_keys > 1 || label.is_some(), |this| {
                this.child(
                    PopoverMenu::new("changes-filter")
                        .anchor(gpui::Anchor::TopRight)
                        .trigger(
                            ButtonLike::new("changes-filter-trigger")
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .px_1()
                                        .child(
                                            Icon::new(IconName::Filter)
                                                .size(IconSize::XSmall)
                                                .color(if label.is_some() {
                                                    Color::Accent
                                                } else {
                                                    Color::Muted
                                                }),
                                        )
                                        .when_some(label, |this, label| {
                                            this.child(Label::new(label).size(LabelSize::XSmall))
                                        }),
                                )
                                .tooltip(Tooltip::text("Show file types and areas")),
                        )
                        .menu(move |window, cx| {
                            let weak = weak.clone();
                            Some(ContextMenu::build_persistent(
                                window,
                                cx,
                                move |menu, _, cx| filter_menu(menu, &weak, cx),
                            ))
                        }),
                )
            })
    }

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let rows = self.rows().to_vec();
        range
            .filter_map(|index| rows.get(index).cloned().map(|row| (index, row)))
            .map(|(index, row)| self.render_row(index, &row, cx))
            .collect()
    }

    fn render_row(&self, index: usize, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let path = Path::new(&row.path);
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| row.path.clone());
        let directory = path
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
            .filter(|parent| !parent.is_empty());
        let icon = FileIcons::get_icon(path, cx);
        let selected = self.selected == Some(index);
        let color = status_color(&row.status);
        h_flex()
            .id(("changes-row", index))
            .h(px(26.))
            .px_2()
            .gap_1p5()
            .w_full()
            .cursor_pointer()
            .when(selected, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.select(index, cx);
                this.open_selected(window, cx);
            }))
            .children(icon.map(|icon| Icon::from_path(icon).size(IconSize::Small).color(color)))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .overflow_hidden()
                    .child(Label::new(name).size(LabelSize::Small).single_line())
                    .when_some(row.old_path.as_ref(), |this, old_path| {
                        this.child(
                            Label::new(format!("← {old_path}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .single_line()
                                .truncate(),
                        )
                    })
                    .when_some(directory, |this, directory| {
                        this.child(
                            Label::new(directory)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .single_line()
                                .truncate(),
                        )
                    }),
            )
            .when_some(row.added.filter(|added| *added > 0), |this, added| {
                this.child(
                    Label::new(format!("+{added}"))
                        .size(LabelSize::XSmall)
                        .color(Color::Created),
                )
            })
            .when_some(
                row.removed.filter(|removed| *removed > 0),
                |this, removed| {
                    this.child(
                        Label::new(format!("−{removed}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Deleted),
                    )
                },
            )
            .child(
                Label::new(status_letter(&row.status))
                    .size(LabelSize::XSmall)
                    .color(color),
            )
            .into_any_element()
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let listing = self.listing.as_ref();
        let message: SharedString = if let Some(error) = &self.error {
            error.clone()
        } else if let Some(listing) = listing {
            match listing.compare.status.as_str() {
                "no_base" => "No base branch to compare against".into(),
                "invalid_base" => "The base branch doesn't exist".into(),
                "no_merge_base" => "The branch shares no history with its base".into(),
                _ if listing.unfiltered.files > 0 => "The filters hide every change".into(),
                _ => match &listing.compare.base {
                    Some(base) => format!("No changes vs {base}").into(),
                    None => "No changes".into(),
                },
            }
        } else {
            "Loading changes…".into()
        };
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_1()
            .child(
                Icon::new(IconName::FileDiff)
                    .size(IconSize::Medium)
                    .color(Color::Muted),
            )
            .child(Label::new(message).color(Color::Muted))
            .when(self.scope != ChangesScope::Base, |this| {
                this.child(
                    Button::new("changes-show-all", "Show all changes")
                        .label_size(LabelSize::Small)
                        .on_click(
                            cx.listener(|this, _, _, cx| this.set_scope(ChangesScope::Base, cx)),
                        ),
                )
            })
    }
}

fn filter_menu(menu: ContextMenu, tab: &WeakEntity<ChangesTab>, cx: &mut App) -> ContextMenu {
    let Some(entity) = tab.upgrade() else {
        return menu;
    };
    let facets = entity
        .read(cx)
        .listing
        .as_ref()
        .map(|listing| listing.facets.clone())
        .unwrap_or_default();
    let mut menu = menu;
    let mut first = true;
    for facet in facets {
        let present: Vec<FacetValue> = facet
            .values
            .iter()
            .filter(|value| value.present)
            .cloned()
            .collect();
        if present.is_empty() {
            continue;
        }
        if !first {
            menu = menu.separator();
        }
        first = false;
        menu = menu.header(format!("Show {}", facet.label.to_lowercase()));
        for value in present {
            let tab = tab.clone();
            let facet_id = facet.id.clone();
            let value_id = value.id.clone();
            menu = menu.toggleable_entry(
                value.label.clone(),
                !value.hidden,
                IconPosition::Start,
                None,
                move |_, cx| {
                    let (facet_id, value_id) = (facet_id.clone(), value_id.clone());
                    tab.update(cx, |tab, cx| tab.toggle_facet_value(facet_id, value_id, cx))
                        .log_err();
                },
            );
        }
    }
    menu
}

/// The pane a row's diff opens in: one beside the Changes tab, so the list stays in view.
fn diff_pane(
    changes: &Entity<ChangesTab>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<Pane> {
    let home = workspace
        .panes()
        .iter()
        .find(|pane| pane.read(cx).index_for_item(changes).is_some())
        .cloned();
    let Some(home) = home else {
        return workspace.active_pane().clone();
    };
    let active = workspace.active_pane().clone();
    if active != home {
        return active;
    }
    if let Some(other) = workspace.panes().iter().find(|pane| **pane != home) {
        return other.clone();
    }
    workspace.split_pane(home, SplitDirection::Right, window, cx)
}

/// Moves a just-opened item into `pane` when the opener put it elsewhere.
fn move_into(
    item_id: gpui::EntityId,
    pane: &Entity<Pane>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let source = workspace
        .panes()
        .iter()
        .find(|candidate| {
            candidate
                .read(cx)
                .items()
                .any(|item| item.item_id() == item_id)
        })
        .cloned();
    if let Some(source) = source
        && &source != pane
    {
        let index = pane.read(cx).items_len();
        workspace::move_item(&source, pane, item_id, index, true, window, cx);
    }
}

fn open_uncommitted_diff(
    project_path: project::ProjectPath,
    pane: &Entity<Pane>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    ProjectDiff::deploy_at_project_path(workspace, project_path, window, cx);
    let diff = workspace.items_of_type::<ProjectDiff>(cx).next();
    if let Some(diff) = diff {
        move_into(diff.entity_id(), pane, workspace, window, cx);
    }
}

/// Opens a row's diff the way its scope defines it (§5.1): base → disk, the uncommitted change, or the turn or
/// commit snapshot.
fn open_row(
    row: &Row,
    scope: &ChangesScope,
    listing: &Listing,
    changes: &Entity<ChangesTab>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let root = changes.read(cx).root.clone();
    let project_path = workspace
        .project()
        .read(cx)
        .find_project_path(root.join(&row.path), cx);
    let pane = diff_pane(changes, workspace, window, cx);
    match scope {
        ChangesScope::Uncommitted => {
            let Some(project_path) = project_path else {
                return;
            };
            open_uncommitted_diff(project_path, &pane, workspace, window, cx);
        }
        ChangesScope::LatestTurn | ChangesScope::Turn(_) | ChangesScope::Commit(_) => {
            let sha = match scope {
                ChangesScope::Commit(sha) => Some(sha.clone()),
                _ => listing.turn.as_ref().map(|turn| turn.oid.clone()),
            };
            let Some(sha) = sha else {
                return;
            };
            let Some(repo) = workspace.project().read(cx).active_repository(cx) else {
                return;
            };
            let reveal = git::repository::RepoPath::new(&row.path).log_err();
            let before: Vec<gpui::EntityId> =
                pane.read(cx).items().map(|item| item.item_id()).collect();
            CommitView::open(
                sha,
                repo.downgrade(),
                workspace.weak_handle(),
                CommitViewOptions {
                    reveal_file: reveal,
                    ..Default::default()
                },
                window,
                cx,
            );
            let pane = pane.downgrade();
            cx.spawn_in(window, async move |workspace, cx| {
                // The commit view opens once its diff has loaded; then it joins the diff pane.
                for _ in 0..40 {
                    cx.background_executor()
                        .timer(Duration::from_millis(50))
                        .await;
                    let moved = workspace.update_in(cx, |workspace, window, cx| {
                        let Some(pane) = pane.upgrade() else {
                            return true;
                        };
                        let opened = workspace
                            .items_of_type::<CommitView>(cx)
                            .map(|view| view.entity_id())
                            .find(|id| !before.contains(id));
                        match opened {
                            Some(id) => {
                                move_into(id, &pane, workspace, window, cx);
                                true
                            }
                            None => false,
                        }
                    })?;
                    if moved {
                        break;
                    }
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        }
        ChangesScope::Base => {
            let base = listing
                .compare
                .base
                .clone()
                .filter(|_| listing.compare.status == "ready");
            match base {
                Some(base) => {
                    BranchDiff::deploy_merge_base_at(
                        workspace,
                        base.into(),
                        project_path,
                        pane,
                        window,
                        cx,
                    );
                }
                // With no usable base, the row's uncommitted change is the only diff there is (§5.1 step 4).
                None => {
                    if let Some(project_path) = project_path {
                        open_uncommitted_diff(project_path, &pane, workspace, window, cx);
                    }
                }
            }
        }
    }
}

/// The Icarus workspace whose worktree is `root`, from the server's list.
async fn workspace_id_for_root(client: Arc<Client>, root: &Path) -> anyhow::Result<Option<String>> {
    let workspaces = client
        .request("workspace.list", serde_json::json!({}))
        .await?;
    Ok(workspaces
        .as_array()
        .into_iter()
        .flatten()
        .find(|workspace| {
            workspace["path"]
                .as_str()
                .is_some_and(|path| Path::new(path) == root)
        })
        .and_then(|workspace| workspace["id"].as_str().map(str::to_string)))
}

/// Adds a Changes tab for the workspace's first worktree to `pane`.
pub fn open_changes_tab(
    pane: &Entity<Pane>,
    focus: bool,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<ChangesTab>> {
    let root = workspace.root_paths(cx).first()?.to_path_buf();
    let weak = workspace.weak_handle();
    let changes = cx.new(|cx| ChangesTab::new(weak, root, window, cx));
    workspace.add_item(
        pane.clone(),
        Box::new(changes.clone()),
        None,
        focus,
        focus,
        window,
        cx,
    );
    Some(changes)
}

/// The workspace's Changes tab, opened in the active pane if there is none (the shortcuts work without one).
fn changes_tab(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<ChangesTab>> {
    let existing = workspace.items_of_type::<ChangesTab>(cx).next();
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, false, window, cx);
        return Some(existing);
    }
    let pane = workspace.active_pane().clone();
    open_changes_tab(&pane, false, workspace, window, cx)
}

pub fn register(cx: &mut App) {
    workspace::register_serializable_item::<ChangesTab>(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenChanges, window, cx| {
            if let Some(changes) = changes_tab(workspace, window, cx) {
                window.focus(&changes.focus_handle(cx), cx);
            }
        });
        workspace.register_action(|workspace, _: &ToggleUncommittedChanges, window, cx| {
            if let Some(changes) = changes_tab(workspace, window, cx) {
                changes.update(cx, |changes, cx| {
                    changes.toggle_scope(ChangesScope::Uncommitted, cx)
                });
            }
        });
        workspace.register_action(|workspace, _: &OpenLatestAgentTurn, window, cx| {
            let Some(changes) = changes_tab(workspace, window, cx) else {
                return;
            };
            changes.update(cx, |changes, cx| {
                changes.toggle_scope(ChangesScope::LatestTurn, cx)
            });
            // The turn's first file opens once its rows have loaded.
            let changes = changes.downgrade();
            cx.spawn_in(window, async move |_, cx| {
                for _ in 0..40 {
                    cx.background_executor()
                        .timer(Duration::from_millis(100))
                        .await;
                    let opened = changes.update_in(cx, |changes, window, cx| {
                        if changes.scope != ChangesScope::LatestTurn {
                            return true;
                        }
                        if changes.refreshing.is_some() || changes.listing.is_none() {
                            return false;
                        }
                        if changes
                            .listing
                            .as_ref()
                            .is_some_and(|listing| listing.turn.is_none())
                        {
                            return true;
                        }
                        changes.step(true, window, cx);
                        true
                    })?;
                    if opened {
                        break;
                    }
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        });
        workspace.register_action(|workspace, _: &NextChangedFile, window, cx| {
            if let Some(changes) = changes_tab(workspace, window, cx) {
                changes.update(cx, |changes, cx| changes.step(true, window, cx));
            }
        });
        workspace.register_action(|workspace, _: &PreviousChangedFile, window, cx| {
            if let Some(changes) = changes_tab(workspace, window, cx) {
                changes.update(cx, |changes, cx| changes.step(false, window, cx));
            }
        });
    })
    .detach();
}

impl EventEmitter<ItemEvent> for ChangesTab {}

impl Focusable for ChangesTab {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for ChangesTab {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        let files = self.listing.as_ref().map(|listing| listing.totals.files);
        match (scope_badge(&self.scope, self.listing.as_ref()), files) {
            (Some((badge, _)), Some(files)) => format!("Changes · {badge} ({files})").into(),
            (Some((badge, _)), None) => format!("Changes · {badge}").into(),
            (None, Some(files)) => format!("Changes ({files})").into(),
            (None, None) => "Changes".into(),
        }
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for ChangesTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.rows().len();
        v_flex()
            .key_context("IcarusChanges")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| {
                let next = this.selected.map_or(0, |current| {
                    (current + 1).min(this.rows().len().saturating_sub(1))
                });
                if !this.rows().is_empty() {
                    this.select(next, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &menu::SelectPrevious, _, cx| {
                let previous = this.selected.map_or(0, |current| current.saturating_sub(1));
                if !this.rows().is_empty() {
                    this.select(previous, cx);
                }
            }))
            .on_action(
                cx.listener(|this, _: &menu::Confirm, window, cx| this.open_selected(window, cx)),
            )
            .child(self.render_header(cx))
            .map(|this| {
                if count == 0 {
                    this.child(self.render_empty(cx))
                } else {
                    this.child(
                        uniform_list(
                            "changes-rows",
                            count,
                            cx.processor(Self::render_rows_processor),
                        )
                        .track_scroll(&self.scroll)
                        .flex_1()
                        .py_1(),
                    )
                }
            })
    }
}

impl ChangesTab {
    fn render_rows_processor(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.render_rows(range, cx)
    }
}

fn serialized_changes_key(workspace_id: WorkspaceId, item_id: ItemId) -> String {
    format!("icarus-changes-tab:{}:{item_id}", i64::from(workspace_id))
}

impl SerializableItem for ChangesTab {
    fn serialized_item_kind() -> &'static str {
        "IcarusChangesTab"
    }

    fn cleanup(
        _workspace_id: WorkspaceId,
        _alive_items: Vec<ItemId>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        Task::ready(Ok(()))
    }

    fn deserialize(
        _project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let saved = db::kvp::KeyValueStore::global(cx)
            .read_kvp(&serialized_changes_key(workspace_id, item_id));
        window.spawn(cx, async move |cx| {
            let root: PathBuf = saved?
                .map(PathBuf::from)
                .ok_or_else(|| anyhow::anyhow!("no worktree recorded for this Changes tab"))?;
            cx.update(|window, cx| cx.new(|cx| ChangesTab::new(workspace, root, window, cx)))
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let root = self.root.to_string_lossy().into_owned();
        let store = db::kvp::KeyValueStore::global(cx);
        Some(cx.background_spawn(async move {
            store
                .write_kvp(serialized_changes_key(workspace_id, item_id), root)
                .await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(event, ItemEvent::UpdateTab)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn sample_listing() -> Listing {
        serde_json::from_value(json!({
            "compare": { "status": "ready", "base": "origin/main" },
            "rows": [
                { "path": "src/app.rs", "status": "modified", "added": 12, "removed": 3 },
                { "path": "src/new.rs", "status": "added", "added": 40 },
                { "path": "README.md", "status": "deleted", "removed": 7 },
            ],
            "totals": { "files": 3, "added": 52, "removed": 10 },
            "unfiltered": { "files": 4, "added": 60, "removed": 10 },
            "facets": [
                { "id": "type", "label": "File types", "active": true, "values": [
                    { "id": "implementation", "label": "Implementation", "present": true, "hidden": false },
                    { "id": "test", "label": "Tests", "present": true, "hidden": true },
                    { "id": "documentation", "label": "Documentation", "present": true, "hidden": false },
                    { "id": "generated", "label": "Generated", "present": false, "hidden": true },
                ]},
                { "id": "area", "label": "Areas", "active": false, "values": [
                    { "id": "backend", "label": "Backend", "present": true, "hidden": false },
                ]},
            ],
        }))
        .expect("listing")
    }

    #[test]
    fn the_filter_pill_names_what_is_still_shown() {
        let listing = sample_listing();
        assert_eq!(
            filter_label(&listing.facets).as_deref(),
            Some("Implementation, Documentation")
        );
        let mut all_hidden = listing.facets[0].clone();
        for value in &mut all_hidden.values {
            value.hidden = true;
        }
        assert_eq!(all_hidden.shown_label().as_deref(), Some("No file types"));
        assert_eq!(filter_label(&listing.facets[1..]), None);
    }

    #[test]
    fn scopes_go_on_the_wire_and_badge_as_orca_does() {
        assert_eq!(ChangesScope::LatestTurn.wire(), "turn:latest");
        assert_eq!(ChangesScope::Commit("abc".into()).wire(), "commit:abc");
        assert_eq!(scope_badge(&ChangesScope::Base, None), None);
        assert_eq!(
            scope_badge(&ChangesScope::Commit("0123456789".into()), None).map(|(text, _)| text),
            Some("Commit 0123456".into())
        );
        assert_eq!(
            scope_badge(&ChangesScope::Turn("t1".into()), None).map(|(text, _)| text),
            Some("Historical changes".into())
        );
    }

    #[gpui::test]
    async fn a_changes_tab_restores_its_worktree(cx: &mut gpui::TestAppContext) {
        let (workspace, mut cx) = crate::browser::tests::workspace_with_database(cx).await;
        let cx = &mut cx;
        let (project, workspace_id, item_id, save) =
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.set_random_database_id();
                let pane = workspace.active_pane().clone();
                let tab =
                    open_changes_tab(&pane, true, workspace, window, cx).expect("changes tab");
                let item_id = tab.entity_id().as_u64() as ItemId;
                let save = tab.update(cx, |tab, cx| tab.serialize(workspace, item_id, false, cx));
                (
                    workspace.project().clone(),
                    workspace.database_id().expect("database id"),
                    item_id,
                    save,
                )
            });
        save.expect("saved").await.expect("save");
        let restore = workspace.update_in(cx, |workspace, window, cx| {
            ChangesTab::deserialize(
                project,
                workspace.weak_handle(),
                workspace_id,
                item_id,
                window,
                cx,
            )
        });
        let restored = restore.await.expect("restore");
        restored.read_with(cx, |tab, _| assert_eq!(tab.root, PathBuf::from("/project")));
    }

    #[gpui::test]
    async fn diffs_open_beside_the_list_without_splitting_again(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "a.txt": "" })).await;
        let project = project::Project::test(fs, [Path::new("/project")], cx).await;
        let multi_workspace = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let cx = &mut gpui::VisualTestContext::from_window(multi_workspace.into(), cx);

        let (changes, home, first, second) = workspace.update_in(cx, |workspace, window, cx| {
            let home = workspace.active_pane().clone();
            let changes =
                open_changes_tab(&home, true, workspace, window, cx).expect("changes tab");
            let first = diff_pane(&changes, workspace, window, cx);
            let second = diff_pane(&changes, workspace, window, cx);
            (changes, home, first, second)
        });
        assert_ne!(first, home, "the diff never covers the list");
        assert_eq!(first, second, "the split is made once");
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.panes().len(), 2);
            assert!(home.read(cx).index_for_item(&changes).is_some());
        });
    }

    #[gpui::test]
    async fn stepping_through_files_by_action_opens_them_without_a_double_lease(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            register(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "a.txt": "" })).await;
        let project = project::Project::test(fs, [Path::new("/project")], cx).await;
        let multi_workspace = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let cx = &mut gpui::VisualTestContext::from_window(multi_workspace.into(), cx);
        let changes = workspace.update_in(cx, |workspace, window, cx| {
            let pane = workspace.active_pane().clone();
            open_changes_tab(&pane, true, workspace, window, cx).expect("changes tab")
        });
        changes.update(cx, |changes, _| {
            changes.listing = Some(sample_listing());
            changes.refreshing = Some(Task::ready(()));
        });

        cx.dispatch_action(NextChangedFile);
        cx.run_until_parked();

        changes.read_with(cx, |changes, _| assert_eq!(changes.selected, Some(0)));
    }

    #[gpui::test]
    async fn toggles_flip_back_to_everything_and_stepping_wraps(cx: &mut gpui::TestAppContext) {
        let window = cx.add_empty_window();
        let changes = window.update(|_, cx| {
            cx.new(|cx| ChangesTab::preview(sample_listing(), ChangesScope::Base, None, cx))
        });
        changes.update(window, |changes, cx| {
            changes.refreshing = Some(Task::ready(()));
            changes.toggle_scope(ChangesScope::Uncommitted, cx);
            assert_eq!(changes.scope(), &ChangesScope::Uncommitted);
            changes.toggle_scope(ChangesScope::Uncommitted, cx);
            assert_eq!(changes.scope(), &ChangesScope::Base);
            changes.toggle_scope(ChangesScope::LatestTurn, cx);
            changes.toggle_scope(ChangesScope::Uncommitted, cx);
            assert_eq!(
                changes.scope(),
                &ChangesScope::Uncommitted,
                "one scope replaces the other"
            );
        });
        changes.update_in(window, |changes, window, cx| {
            changes.step(false, window, cx);
            assert_eq!(
                changes.selected,
                Some(2),
                "previous from nothing enters at the end"
            );
            changes.step(true, window, cx);
            assert_eq!(changes.selected, Some(0), "a single press wraps");
        });
    }
}
