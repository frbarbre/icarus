use collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use file_icons::FileIcons;
use git_ui::commit_view::{CommitView, CommitViewOptions};
use gpui::{
    Action, App, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, Pixels, Render, ScrollStrategy, SharedString, Task,
    UniformListScrollHandle, WeakEntity, Window, actions, px, uniform_list,
};
use gpui_tokio::Tokio;
use icarus_client::Client;
use serde::Deserialize;
use ui::{ButtonLike, ContextMenu, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent, PanelSizeState};

use crate::AppConnection;
use crate::diff_tab::{DiffBase, DiffTabMode, open_diff_tab};

actions!(
    icarus,
    [
        /// Shows and focuses the Source Control panel.
        OpenChanges,
        /// Shows or hides the Source Control panel.
        ToggleSourceControlPanel,
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
/// Key repeat arrives far faster than two deliberate presses, so steps closer than this are a held key.
const HOLD_REPEAT: Duration = Duration::from_millis(150);

/// The row a file step lands on: `None` at an end while the key is held, as Orca stops there.
pub(crate) fn next_file(
    current: Option<usize>,
    count: usize,
    forward: bool,
    held: bool,
) -> Option<usize> {
    if count == 0 {
        return None;
    }
    match (current, forward) {
        (None, true) => Some(0),
        (None, false) => Some(count - 1),
        (Some(current), true) if current + 1 < count => Some(current + 1),
        (Some(current), false) if current > 0 => Some(current - 1),
        (Some(_), true) => (!held).then_some(0),
        (Some(_), false) => (!held).then_some(count - 1),
    }
}

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

/// Open notes per file from `note.list` (which leaves out resolved notes by default).
fn note_counts(notes: &serde_json::Value) -> HashMap<String, usize> {
    let mut counts = HashMap::default();
    for note in notes.as_array().into_iter().flatten() {
        if note["resolved"].as_bool() == Some(true) {
            continue;
        }
        if let Some(path) = note["path"].as_str() {
            *counts.entry(path.to_owned()).or_default() += 1;
        }
    }
    counts
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

/// The Source Control panel in the right sidebar: every file that differs from the workspace's base, one row
/// each. Picking a row opens that file's diff tab.
pub struct SourceControlPanel {
    workspace: WeakEntity<Workspace>,
    project: WeakEntity<project::Project>,
    root: PathBuf,
    icarus_workspace: Option<String>,
    scope: ChangesScope,
    listing: Option<Listing>,
    error: Option<SharedString>,
    selected: Option<usize>,
    /// The last file step's direction and time, to tell a held key's repeats from single presses.
    last_step: Option<(bool, Instant)>,
    /// Open notes per changed file, shown as a badge on its row.
    note_counts: HashMap<String, usize>,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
    refreshing: Option<Task<()>>,
    position: DockPosition,
    active: bool,
    _poll: Task<()>,
}

impl SourceControlPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                cx.new(|cx| Self::new(workspace, window, cx))
            })
        })
    }

    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().downgrade();
        let root = worktree_root(&project, cx).unwrap_or_default();
        let workspace = workspace.weak_handle();
        // Polls only while the panel shows; the shortcuts reveal it first.
        let poll = if !crate::enabled(cx) {
            Task::ready(())
        } else {
            cx.spawn_in(window, async move |this, cx| {
                loop {
                    let refreshed = this.update(cx, |this, cx| {
                        if this.active {
                            this.refresh(cx);
                        }
                    });
                    if refreshed.is_err() {
                        break;
                    }
                    cx.background_executor().timer(REFRESH_INTERVAL).await;
                }
            })
        };
        Self {
            workspace,
            project,
            root,
            icarus_workspace: None,
            scope: ChangesScope::Base,
            listing: None,
            error: None,
            selected: None,
            last_step: None,
            note_counts: HashMap::default(),
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            refreshing: None,
            position: DockPosition::Right,
            active: false,
            _poll: poll,
        }
    }

    /// A panel showing `listing` without a server, for visual tests.
    pub fn preview(
        listing: Listing,
        scope: ChangesScope,
        selected: Option<usize>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            workspace: WeakEntity::new_invalid(),
            project: WeakEntity::new_invalid(),
            root: PathBuf::new(),
            icarus_workspace: Some("preview".into()),
            scope,
            listing: Some(listing),
            error: None,
            selected,
            last_step: None,
            note_counts: HashMap::default(),
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            refreshing: None,
            position: DockPosition::Right,
            active: true,
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
        // The project may still be loading its worktree when the panel is built.
        if self.root.as_os_str().is_empty()
            && let Some(root) = worktree_root(&self.project, cx)
        {
            self.root = root;
        }
        if self.root.as_os_str().is_empty() {
            return;
        }
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
            let listing = serde_json::from_value::<Listing>(value)?;
            // A server without notes, or a failed lookup, just shows no badges.
            let note_counts = client
                .request("note.list", serde_json::json!({ "workspace": id }))
                .await
                .ok()
                .map(|value| note_counts(&value))
                .unwrap_or_default();
            anyhow::Ok((id, listing, note_counts))
        });
        self.refreshing = Some(cx.spawn(async move |this, cx| {
            let result = fetch.await;
            this.update(cx, |this, cx| {
                this.refreshing = None;
                match result {
                    Ok((id, listing, note_counts)) => {
                        this.note_counts = note_counts;
                        this.icarus_workspace = Some(id);
                        this.error = None;
                        if let Some(selected) = this.selected
                            && selected >= listing.rows.len()
                        {
                            this.selected = listing.rows.len().checked_sub(1);
                        }
                        if this.listing.as_ref() != Some(&listing) {
                            this.listing = Some(listing);
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
        self.step_from(forward, None, window, cx);
    }

    /// Steps from `current` (the open diff tab's file) when it's in the list, else from the selection.
    /// A single press wraps; a held key's repeats stop at the ends (Orca's changed-file-hold-navigation).
    pub fn step_from(
        &mut self,
        forward: bool,
        current: Option<&Path>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = cx.background_executor().now();
        let held = self.last_step.is_some_and(|(direction, at)| {
            direction == forward && now.saturating_duration_since(at) < HOLD_REPEAT
        });
        self.last_step = Some((forward, now));
        let count = self.rows().len();
        if count == 0 {
            return;
        }
        let from_tab = current.and_then(|current| {
            self.rows()
                .iter()
                .position(|row| self.root.join(&row.path) == current)
        });
        let Some(next) = next_file(from_tab.or(self.selected), count, forward, held) else {
            return;
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
        let root = self.root.clone();
        // Deferred: the step actions run inside a Workspace update, which would double-lease it.
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                open_row(&row, &scope, &listing, &root, workspace, window, cx);
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
            .when_some(
                self.note_counts
                    .get(&row.path)
                    .copied()
                    .filter(|count| *count > 0),
                |this, count| {
                    this.child(
                        h_flex()
                            .id(("changes-row-notes", index))
                            .gap_0p5()
                            .tooltip(Tooltip::text(if count == 1 {
                                "1 open note".to_owned()
                            } else {
                                format!("{count} open notes")
                            }))
                            .child(
                                Icon::new(IconName::Notepad)
                                    .size(IconSize::XSmall)
                                    .color(Color::Accent),
                            )
                            .child(
                                Label::new(count.to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Accent),
                            ),
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

fn filter_menu(
    menu: ContextMenu,
    tab: &WeakEntity<SourceControlPanel>,
    cx: &mut App,
) -> ContextMenu {
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

/// What a row's diff tab compares against in `scope` (§5.1): the merge base when the compare is ready,
/// otherwise the uncommitted change, which is the only diff there is (step 4). Turns and commits have none.
pub(crate) fn diff_base_for(scope: &ChangesScope, listing: &Listing) -> Option<DiffBase> {
    match scope {
        ChangesScope::Uncommitted => Some(DiffBase::Uncommitted),
        ChangesScope::Base => Some(
            listing
                .compare
                .base
                .clone()
                .filter(|_| listing.compare.status == "ready")
                .map(DiffBase::MergeBase)
                .unwrap_or(DiffBase::Uncommitted),
        ),
        ChangesScope::LatestTurn | ChangesScope::Turn(_) | ChangesScope::Commit(_) => None,
    }
}

/// Opens a row the way its scope defines it: a diff tab against the base or the uncommitted change, or the
/// turn's or commit's snapshot in the commit view.
fn open_row(
    row: &Row,
    scope: &ChangesScope,
    listing: &Listing,
    root: &Path,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(base) = diff_base_for(scope, listing) {
        open_diff_tab(
            root.join(&row.path),
            base,
            DiffTabMode::Diff,
            None,
            workspace,
            window,
            cx,
        )
        .detach_and_log_err(cx);
        return;
    }
    let sha = match scope {
        ChangesScope::Commit(sha) => Some(sha.clone()),
        _ => listing.turn.as_ref().map(|turn| turn.oid.clone()),
    };
    let (Some(sha), Some(repo)) = (sha, workspace.project().read(cx).active_repository(cx)) else {
        return;
    };
    CommitView::open(
        sha,
        repo.downgrade(),
        workspace.weak_handle(),
        CommitViewOptions {
            reveal_file: git::repository::RepoPath::new(&row.path).log_err(),
            ..Default::default()
        },
        window,
        cx,
    );
}

/// The project's first worktree; read through the project because panels activate inside Workspace updates.
fn worktree_root(project: &WeakEntity<project::Project>, cx: &App) -> Option<PathBuf> {
    let project = project.upgrade()?;
    let project = project.read(cx);
    let worktree = project.visible_worktrees(cx).next()?;
    Some(worktree.read(cx).abs_path().to_path_buf())
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

/// The workspace's Source Control panel, revealed in the sidebar (Orca's shortcuts open it at once, PR 77).
pub fn reveal_source_control(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<SourceControlPanel>> {
    workspace.open_panel::<SourceControlPanel>(window, cx);
    workspace.panel::<SourceControlPanel>(cx)
}

/// The file keys step from the open diff tab's file (Orca's published review order position).
fn step_changed_file(
    forward: bool,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let current = workspace
        .active_item(cx)
        .and_then(|item| item.downcast::<crate::diff_tab::DiffTab>())
        .map(|tab| tab.read(cx).path().to_path_buf());
    if let Some(changes) = changes_tab(workspace, window, cx) {
        changes.update(cx, |changes, cx| {
            changes.step_from(forward, current.as_deref(), window, cx)
        });
    }
}

/// Whether Source Control is the panel showing in any dock.
pub(crate) fn source_control_showing(workspace: &Workspace, _window: &Window, cx: &App) -> bool {
    let Some(panel) = workspace.panel::<SourceControlPanel>(cx) else {
        return false;
    };
    workspace.all_docks().iter().any(|dock| {
        dock.read(cx)
            .visible_panel()
            .is_some_and(|visible| visible.panel_id() == panel.entity_id())
    })
}

fn changes_tab(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<SourceControlPanel>> {
    reveal_source_control(workspace, window, cx)
}

pub fn register(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenChanges, window, cx| {
            workspace.focus_panel::<SourceControlPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &ToggleSourceControlPanel, window, cx| {
            workspace.toggle_panel_focus::<SourceControlPanel>(window, cx);
        });
        // Diff tabs handle ⌥⌘D themselves; from a plain editor it opens that file's diff (§5.2).
        workspace.register_action(|workspace, _: &crate::diff_tab::ShowDiff, window, cx| {
            let Some(path) = workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<editor::Editor>())
                .and_then(|editor| editor.read(cx).buffer().read(cx).as_singleton())
                .and_then(|buffer| {
                    use language::LocalFile as _;
                    project::File::from_dyn(buffer.read(cx).file()).map(|file| file.abs_path(cx))
                })
            else {
                return;
            };
            let base = workspace
                .panel::<SourceControlPanel>(cx)
                .and_then(|panel| {
                    let panel = panel.read(cx);
                    panel
                        .listing
                        .as_ref()
                        .and_then(|listing| diff_base_for(&panel.scope, listing))
                })
                .unwrap_or(DiffBase::Uncommitted);
            open_diff_tab(path, base, DiffTabMode::Diff, None, workspace, window, cx)
                .detach_and_log_err(cx);
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
            step_changed_file(true, workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &PreviousChangedFile, window, cx| {
            step_changed_file(false, workspace, window, cx);
        });
    })
    .detach();
}

impl EventEmitter<PanelEvent> for SourceControlPanel {}

impl Focusable for SourceControlPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for SourceControlPanel {
    fn persistent_name() -> &'static str {
        SOURCE_CONTROL_PANEL
    }

    fn panel_key() -> &'static str {
        SOURCE_CONTROL_PANEL
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(300.)
    }

    fn initial_size_state(&self, _window: &Window, _cx: &App) -> PanelSizeState {
        PanelSizeState {
            size: None,
            flex: None,
        }
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::GitBranch)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Source Control")
    }

    fn icon_label(&self, _window: &Window, _cx: &App) -> Option<String> {
        self.listing
            .as_ref()
            .map(|listing| listing.totals.files)
            .filter(|files| *files > 0)
            .map(|files| files.to_string())
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        ToggleSourceControlPanel.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        8
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        let became_active = active && !self.active;
        self.active = active;
        if became_active {
            self.refresh(cx);
        }
    }
}

/// The Source Control panel's persistent name, which the sidebar switcher also looks it up by.
pub const SOURCE_CONTROL_PANEL: &str = "IcarusSourceControlPanel";

impl Render for SourceControlPanel {
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

impl SourceControlPanel {
    fn render_rows_processor(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.render_rows(range, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rows_count_only_open_notes() {
        let counts = note_counts(&json!([
            { "path": "src/app.rs", "resolved": false },
            { "path": "src/app.rs", "resolved": false },
            { "path": "src/app.rs", "resolved": true },
            { "path": "README.md", "resolved": false },
        ]));
        assert_eq!(counts.get("src/app.rs"), Some(&2));
        assert_eq!(counts.get("README.md"), Some(&1));
        assert!(note_counts(&json!({ "error": "no notes" })).is_empty());
    }

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

    #[test]
    fn rows_diff_against_the_ready_base_or_else_their_uncommitted_change() {
        let mut listing = sample_listing();
        assert_eq!(
            diff_base_for(&ChangesScope::Base, &listing),
            Some(DiffBase::MergeBase("origin/main".into()))
        );
        assert_eq!(
            diff_base_for(&ChangesScope::Uncommitted, &listing),
            Some(DiffBase::Uncommitted)
        );
        assert_eq!(diff_base_for(&ChangesScope::LatestTurn, &listing), None);
        listing.compare.status = "no_base".into();
        assert_eq!(
            diff_base_for(&ChangesScope::Base, &listing),
            Some(DiffBase::Uncommitted),
            "§5.1 step 4"
        );
    }

    #[gpui::test]
    async fn stepping_by_action_opens_and_reuses_diff_tabs_without_a_double_lease(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, mut cx) = crate::diff_tab::tests::git_workspace(cx).await;
        let cx = &mut cx;
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| SourceControlPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        panel.update(cx, |panel, _| {
            let mut listing = sample_listing();
            listing.rows.truncate(2);
            panel.listing = Some(listing);
            panel.refreshing = Some(Task::ready(()));
        });

        let opened = |cx: &mut gpui::VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .items_of_type::<crate::DiffTab>(cx)
                    .map(|tab| tab.read(cx).path().to_path_buf())
                    .collect::<Vec<_>>()
            })
        };
        cx.dispatch_action(NextChangedFile);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| assert_eq!(panel.selected, Some(0)));
        assert_eq!(opened(cx), [PathBuf::from("/project/src/app.rs")]);
        workspace.read_with(cx, |workspace, cx| {
            let dock = workspace.right_dock().read(cx);
            assert!(dock.is_open(), "stepping reveals Source Control");
        });

        cx.dispatch_action(NextChangedFile);
        cx.run_until_parked();
        cx.dispatch_action(PreviousChangedFile);
        cx.run_until_parked();
        assert_eq!(
            opened(cx).len(),
            2,
            "stepping back returns to the first file's tab"
        );
        let active = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<crate::DiffTab>())
                .map(|tab| tab.read(cx).path().to_path_buf())
        });
        assert_eq!(active, Some(PathBuf::from("/project/src/app.rs")));
    }

    #[gpui::test]
    async fn a_held_file_key_stops_at_the_last_file_but_a_press_wraps(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, mut cx) = crate::diff_tab::tests::git_workspace(cx).await;
        let cx = &mut cx;
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| SourceControlPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        panel.update(cx, |panel, _| {
            let mut listing = sample_listing();
            listing.rows.truncate(2);
            panel.listing = Some(listing);
            panel.refreshing = Some(Task::ready(()));
        });
        let selected =
            |cx: &mut gpui::VisualTestContext| panel.read_with(cx, |panel, _| panel.selected);
        let press = |pause: Duration, cx: &mut gpui::VisualTestContext| {
            cx.executor().advance_clock(pause);
            cx.dispatch_action(NextChangedFile);
            cx.run_until_parked();
        };

        press(Duration::from_secs(1), cx);
        press(Duration::from_secs(1), cx);
        assert_eq!(selected(cx), Some(1), "on the last file");
        press(Duration::from_millis(40), cx);
        assert_eq!(
            selected(cx),
            Some(1),
            "a held key's repeat stops at the end"
        );
        press(Duration::from_secs(1), cx);
        assert_eq!(
            selected(cx),
            Some(0),
            "a fresh press wraps to the first file"
        );
    }

    #[gpui::test]
    async fn toggles_flip_back_to_everything_and_stepping_wraps(cx: &mut gpui::TestAppContext) {
        let window = cx.add_empty_window();
        let changes = window.update(|_, cx| {
            cx.new(|cx| SourceControlPanel::preview(sample_listing(), ChangesScope::Base, None, cx))
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
