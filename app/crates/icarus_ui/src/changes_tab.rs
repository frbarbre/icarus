use collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use file_icons::FileIcons;
use git_ui::commit_view::{CommitView, CommitViewOptions};
use gpui::{
    Action, App, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, Pixels, PromptLevel, Render, ScrollStrategy, SharedString, Task,
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
        /// Throws away the selected file's uncommitted change, after asking.
        DiscardSelectedChange,
        /// Opens the commit dialog (⌘Enter commits).
        CommitChanges,
        /// Pushes the workspace's branch, publishing it if needed.
        PushBranch,
        /// Pushes with --force-with-lease, which refuses if the remote moved since the last fetch.
        ForcePushWithLease,
        /// Pulls the branch's own upstream.
        PullBranch,
        /// Pulls, then pushes.
        SyncBranch,
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
    #[serde(default)]
    pub unstaged: bool,
    #[serde(default)]
    pub untracked: bool,
    #[serde(default)]
    pub conflicted: bool,
}

/// The row's Discard button label, when it has one: only for an unstaged or untracked change that
/// isn't conflicted, and never while a turn or commit is shown (source-control.md §1.6).
pub(crate) fn discard_label(row: &Row, scope: &ChangesScope) -> Option<&'static str> {
    if !matches!(scope, ChangesScope::Base | ChangesScope::Uncommitted)
        || row.conflicted
        || !(row.unstaged || row.untracked)
    {
        return None;
    }
    Some(if row.untracked {
        "Delete untracked file"
    } else if row.status == "deleted" {
        "Restore file"
    } else {
        "Discard changes"
    })
}

/// One agent turn snapshot (`workspace.turns`), newest first.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TurnRow {
    pub name: String,
    #[serde(default)]
    pub meta: Option<TurnRowMeta>,
    #[serde(default)]
    pub files: usize,
    #[serde(default)]
    pub added: u64,
    #[serde(default)]
    pub removed: u64,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnRowMeta {
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub agent_id: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub completed_at: u64,
}

impl TurnRow {
    /// The prompt's first line, as the row's title.
    fn title(&self) -> String {
        self.meta
            .as_ref()
            .and_then(|meta| meta.prompt.lines().find(|line| !line.trim().is_empty()))
            .map(|line| line.trim().to_string())
            .unwrap_or_else(|| "Agent turn".to_string())
    }

    /// `agent · 3m ago · 2 files +4 −1`.
    fn detail(&self, now_ms: u64) -> String {
        let agent = self
            .meta
            .as_ref()
            .and_then(|meta| meta.provider.clone())
            .unwrap_or_else(|| "agent".to_string());
        let age = self
            .meta
            .as_ref()
            .map(|meta| relative_age(now_ms.saturating_sub(meta.completed_at)))
            .unwrap_or_default();
        let files = if self.files == 1 {
            "1 file".to_string()
        } else {
            format!("{} files", self.files)
        };
        format!(
            "{agent} · {age} · {files} +{} −{}",
            self.added, self.removed
        )
    }
}

/// Open Source Control panels, so a `git.changed` event refreshes them at once instead of on the next poll.
#[derive(Default)]
struct SourceControlPanels(Vec<WeakEntity<SourceControlPanel>>);

impl gpui::Global for SourceControlPanels {}

/// A git action ran on the server (from any client or agent): reload the panels and open diffs.
pub(crate) fn git_changed(payload: serde_json::Value, cx: &mut App) {
    let workspace = payload["workspace"].as_str().map(str::to_owned);
    let panels = cx
        .try_global::<SourceControlPanels>()
        .map(|panels| panels.0.clone())
        .unwrap_or_default();
    for panel in panels {
        panel
            .update(cx, |panel, cx| {
                if workspace.is_none() || panel.icarus_workspace == workspace {
                    panel.refresh(cx);
                }
            })
            .ok();
    }
    if cx.has_global::<SourceControlPanels>() {
        cx.update_global::<SourceControlPanels, _>(|panels, _| {
            panels.0.retain(|panel| panel.upgrade().is_some())
        });
    }
    crate::review::review_changed(cx);
}

/// The server's message without its error code, so git's words reach the user unchanged.
pub(crate) fn git_error_text(error: &anyhow::Error) -> String {
    match error.downcast_ref::<icarus_client::ClientError>() {
        Some(icarus_client::ClientError::Rpc(rpc)) => rpc.message.clone(),
        None => format!("{error:#}"),
    }
}

fn relative_age(elapsed_ms: u64) -> String {
    let minutes = elapsed_ms / 60_000;
    match minutes {
        0 => "just now".to_string(),
        1..=59 => format!("{minutes}m ago"),
        60..=1439 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1440),
    }
}

/// Where the branch pushes and how far it is from there (`git.status`).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct SyncInfo {
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub upstream: Option<String>,
    #[serde(default)]
    pub published: bool,
    #[serde(default)]
    pub ahead: u32,
    #[serde(default)]
    pub behind: u32,
    #[serde(default)]
    pub has_uncommitted_changes: bool,
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
    newest_turn: Option<&str>,
) -> Option<(String, Option<String>)> {
    let prompt = listing
        .and_then(|listing| listing.turn.as_ref())
        .and_then(|turn| turn.meta.as_ref())
        .map(|meta| meta.prompt.clone());
    match scope {
        ChangesScope::Base => None,
        ChangesScope::Uncommitted => Some(("Uncommitted".into(), None)),
        ChangesScope::LatestTurn => Some(("Latest turn".into(), prompt)),
        // A previewed turn reads as latest only while no newer turn has landed (§2.1).
        ChangesScope::Turn(name) if newest_turn == Some(name.as_str()) => {
            Some(("Latest turn".into(), prompt))
        }
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
    /// The workspace's agent turns, newest first.
    turns: Vec<TurnRow>,
    turns_collapsed: bool,
    sync: Option<SyncInfo>,
    /// The running git action's label, e.g. "Pushing…".
    git_busy: Option<SharedString>,
    /// The last git action's failure, in git's own words.
    git_error: Option<SharedString>,
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
        if crate::enabled(cx) {
            let this = cx.entity().downgrade();
            cx.default_global::<SourceControlPanels>().0.push(this);
        }
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
            turns: Vec::new(),
            turns_collapsed: false,
            sync: None,
            git_busy: None,
            git_error: None,
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
            turns: Vec::new(),
            turns_collapsed: false,
            sync: None,
            git_busy: None,
            git_error: None,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            refreshing: None,
            position: DockPosition::Right,
            active: true,
            _poll: Task::ready(()),
        }
    }

    /// Fills the turns and branch state a server would, for previews.
    pub fn set_git_preview(&mut self, turns: Vec<TurnRow>, sync: SyncInfo, cx: &mut Context<Self>) {
        self.turns = turns;
        self.sync = Some(sync);
        cx.notify();
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
            // Turns and the branch state are extras: a failure leaves those sections empty.
            let turns = client
                .request("workspace.turns", serde_json::json!({ "id": id }))
                .await
                .ok()
                .and_then(|value| serde_json::from_value::<Vec<TurnRow>>(value).ok())
                .unwrap_or_default();
            let sync = client
                .request("git.status", serde_json::json!({ "workspace": id }))
                .await
                .ok()
                .and_then(|value| serde_json::from_value::<SyncInfo>(value).ok());
            anyhow::Ok((id, listing, note_counts, turns, sync))
        });
        self.refreshing = Some(cx.spawn(async move |this, cx| {
            let result = fetch.await;
            this.update(cx, |this, cx| {
                this.refreshing = None;
                match result {
                    Ok((id, listing, note_counts, turns, sync)) => {
                        this.note_counts = note_counts;
                        this.turns = turns;
                        this.sync = sync;
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

    /// Asks, then throws away the row's uncommitted change (committed branch changes stay).
    pub fn discard_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows().get(index).cloned() else {
            return;
        };
        let Some(label) = discard_label(&row, &self.scope) else {
            return;
        };
        let Some(workspace) = self.icarus_workspace.clone() else {
            return;
        };
        let detail = format!(
            "{}\n\nOnly the uncommitted change is thrown away; what's committed on the branch stays. This can't be undone.",
            row.path
        );
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("{label}?"),
            Some(&detail),
            &[label, "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await? != 0 {
                return anyhow::Ok(());
            }
            this.update(cx, |this, cx| {
                this.run_git(
                    "git.discard",
                    serde_json::json!({ "workspace": workspace, "paths": [row.path] }),
                    "Discarding…",
                    cx,
                );
            })
        })
        .detach_and_log_err(cx);
    }

    /// Runs a `git.*` RPC for this workspace, showing its progress and git's own error text.
    pub fn run_git(
        &mut self,
        method: &'static str,
        params: serde_json::Value,
        busy: &str,
        cx: &mut Context<Self>,
    ) {
        if self.git_busy.is_some() {
            return;
        }
        let Some(task) = crate::review::request(Self::client(cx), method, params, cx) else {
            self.git_error = Some("Not connected to the Icarus server".into());
            cx.notify();
            return;
        };
        self.git_busy = Some(busy.to_string().into());
        self.git_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.git_busy = None;
                match result {
                    Ok(value) => {
                        if let Ok(sync) = serde_json::from_value::<SyncInfo>(value)
                            && sync.branch.is_some()
                        {
                            this.sync = Some(sync);
                        }
                        this.refresh(cx);
                    }
                    Err(error) => this.git_error = Some(git_error_text(&error).into()),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn workspace_params(&self) -> Option<serde_json::Value> {
        self.icarus_workspace
            .as_ref()
            .map(|id| serde_json::json!({ "workspace": id }))
    }

    pub fn push(&mut self, force_with_lease: bool, cx: &mut Context<Self>) {
        let Some(mut params) = self.workspace_params() else {
            return;
        };
        params["force_with_lease"] = serde_json::json!(force_with_lease);
        let busy = if force_with_lease {
            "Force pushing…"
        } else {
            "Pushing…"
        };
        self.run_git("git.push", params, busy, cx);
    }

    pub fn pull(&mut self, cx: &mut Context<Self>) {
        if let Some(params) = self.workspace_params() {
            self.run_git("git.pull", params, "Pulling…", cx);
        }
    }

    pub fn sync_branch(&mut self, cx: &mut Context<Self>) {
        if let Some(params) = self.workspace_params() {
            self.run_git("git.sync", params, "Syncing…", cx);
        }
    }

    pub fn commit(&mut self, message: String, amend: bool, cx: &mut Context<Self>) {
        let Some(mut params) = self.workspace_params() else {
            return;
        };
        params["message"] = serde_json::json!(message);
        params["amend"] = serde_json::json!(amend);
        self.run_git("git.commit", params, "Committing…", cx);
    }

    /// Shows what the turn changed: the turn scope plus its first file (source-control.md §2.1).
    pub fn open_turn(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(turn) = self.turns.get(index) else {
            return;
        };
        self.set_scope(ChangesScope::Turn(turn.name.clone()), cx);
        // The rows arrive with the refresh; open the first one once they're in.
        cx.spawn_in(window, async move |this, cx| {
            for _ in 0..50 {
                let opened = this.update_in(cx, |this, window, cx| {
                    if this.refreshing.is_some() {
                        return false;
                    }
                    if !this.rows().is_empty() {
                        this.select(0, cx);
                        this.open_selected(window, cx);
                    }
                    true
                })?;
                if opened {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
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
        let badge = scope_badge(
            &self.scope,
            listing,
            self.turns.first().map(|turn| turn.name.as_str()),
        );
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
        let discard = discard_label(row, &self.scope);
        h_flex()
            .id(("changes-row", index))
            .group("changes-row")
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
            .when_some(discard, |this, label| {
                this.child(
                    IconButton::new(("changes-row-discard", index), IconName::Undo)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .visible_on_hover("changes-row")
                        .tooltip(Tooltip::text(label))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            this.discard_row(index, window, cx);
                        })),
                )
            })
            .child(
                Label::new(status_letter(&row.status))
                    .size(LabelSize::XSmall)
                    .color(color),
            )
            .into_any_element()
    }

    fn render_turns(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or_default();
        let collapsed = self.turns_collapsed;
        let showing = match &self.scope {
            ChangesScope::Turn(name) => Some(name.clone()),
            ChangesScope::LatestTurn => self.turns.first().map(|turn| turn.name.clone()),
            _ => None,
        };
        v_flex()
            .w_full()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .id("changes-turns-header")
                    .h(px(28.))
                    .px_2()
                    .gap_1()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.turns_collapsed = !this.turns_collapsed;
                        cx.notify();
                    }))
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new("AGENT TURNS")
                            .size(LabelSize::XSmall)
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(self.turns.len().to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .when(!collapsed, |this| {
                this.child(
                    v_flex()
                        .id("changes-turns")
                        .max_h(px(220.))
                        .overflow_y_scroll()
                        .pb_1()
                        .children(self.turns.iter().enumerate().map(|(index, turn)| {
                            let current = showing.as_deref() == Some(turn.name.as_str());
                            let prompt = turn
                                .meta
                                .as_ref()
                                .map(|meta| meta.prompt.clone())
                                .unwrap_or_default();
                            h_flex()
                                .id(("changes-turn", index))
                                .px_2()
                                .py_0p5()
                                .gap_1p5()
                                .w_full()
                                .cursor_pointer()
                                .when(current, |this| {
                                    this.bg(cx.theme().colors().element_selected)
                                })
                                .hover(|style| style.bg(cx.theme().colors().element_hover))
                                .when(!prompt.is_empty(), |this| {
                                    this.tooltip(Tooltip::text(prompt))
                                })
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.open_turn(index, window, cx);
                                }))
                                .child(
                                    Icon::new(IconName::ZedAssistant)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            Label::new(turn.title())
                                                .size(LabelSize::Small)
                                                .single_line()
                                                .truncate(),
                                        )
                                        .child(
                                            Label::new(turn.detail(now_ms))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                                .single_line()
                                                .truncate(),
                                        ),
                                )
                        })),
                )
            })
    }

    fn render_git_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sync = self.sync.clone().unwrap_or_default();
        let busy = self.git_busy.clone();
        let idle = busy.is_none();
        let branch: SharedString = sync
            .branch
            .clone()
            .unwrap_or_else(|| "detached HEAD".to_string())
            .into();
        let position = if !sync.published {
            "not published".to_string()
        } else if sync.ahead == 0 && sync.behind == 0 {
            "up to date".to_string()
        } else {
            format!("↑{} ↓{}", sync.ahead, sync.behind)
        };
        let weak = cx.entity().downgrade();
        v_flex()
            .w_full()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .when_some(self.git_error.clone(), |this, error| {
                this.child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .items_start()
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::XSmall)
                                .color(Color::Error),
                        )
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(error)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Error),
                            ),
                        )
                        .child(
                            IconButton::new("changes-git-error-dismiss", IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.git_error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                h_flex()
                    .h(px(30.))
                    .px_2()
                    .gap_1()
                    .child(
                        Icon::new(IconName::GitBranch)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(match &busy {
                                Some(busy) => busy.clone(),
                                None => format!("{branch} · {position}").into(),
                            })
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line()
                            .truncate(),
                        ),
                    )
                    .child(
                        Button::new("changes-commit", "Commit…")
                            .label_size(LabelSize::XSmall)
                            .disabled(!idle)
                            .tooltip(Tooltip::for_action_title("Commit", &CommitChanges))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(CommitChanges), cx);
                            }),
                    )
                    .child(
                        IconButton::new("changes-pull", IconName::ArrowDown)
                            .icon_size(IconSize::XSmall)
                            .disabled(!idle || !sync.published)
                            .tooltip(Tooltip::text(if sync.behind > 0 {
                                format!("Pull {} commit(s)", sync.behind)
                            } else {
                                "Pull".to_string()
                            }))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.pull(cx))),
                    )
                    .child(
                        IconButton::new("changes-push", IconName::ArrowUp)
                            .icon_size(IconSize::XSmall)
                            .disabled(!idle)
                            .tooltip(Tooltip::text(if !sync.published {
                                "Publish branch".to_string()
                            } else if sync.ahead > 0 {
                                format!("Push {} commit(s)", sync.ahead)
                            } else {
                                "Push".to_string()
                            }))
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.push(false, cx)),
                            ),
                    )
                    .child(
                        PopoverMenu::new("changes-git-more")
                            .trigger(
                                IconButton::new("changes-git-more-trigger", IconName::Ellipsis)
                                    .icon_size(IconSize::XSmall)
                                    .disabled(!idle),
                            )
                            .menu(move |window, cx| {
                                let weak = weak.clone();
                                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                                    let sync_target = weak.clone();
                                    let force_target = weak;
                                    menu.entry("Sync (pull, then push)", None, move |_, cx| {
                                        sync_target
                                            .update(cx, |this, cx| this.sync_branch(cx))
                                            .log_err();
                                    })
                                    .entry(
                                        "Force push (with lease)",
                                        None,
                                        move |_, cx| {
                                            force_target
                                                .update(cx, |this, cx| this.push(true, cx))
                                                .log_err();
                                        },
                                    )
                                }))
                            }),
                    ),
            )
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
        workspace.register_action(|workspace, _: &DiscardSelectedChange, window, cx| {
            let Some(panel) = workspace.panel::<SourceControlPanel>(cx) else {
                return;
            };
            panel.update(cx, |panel, cx| {
                if let Some(selected) = panel.selected {
                    panel.discard_row(selected, window, cx);
                }
            });
        });
        workspace.register_action(|workspace, _: &CommitChanges, window, cx| {
            let Some(panel) = reveal_source_control(workspace, window, cx) else {
                return;
            };
            let panel = panel.downgrade();
            workspace.toggle_modal(window, cx, |window, cx| {
                crate::commit_modal::CommitModal::new(panel, window, cx)
            });
        });
        workspace.register_action(|workspace, _: &PushBranch, window, cx| {
            if let Some(panel) = reveal_source_control(workspace, window, cx) {
                panel.update(cx, |panel, cx| panel.push(false, cx));
            }
        });
        workspace.register_action(|workspace, _: &ForcePushWithLease, window, cx| {
            if let Some(panel) = reveal_source_control(workspace, window, cx) {
                panel.update(cx, |panel, cx| panel.push(true, cx));
            }
        });
        workspace.register_action(|workspace, _: &PullBranch, window, cx| {
            if let Some(panel) = reveal_source_control(workspace, window, cx) {
                panel.update(cx, |panel, cx| panel.pull(cx));
            }
        });
        workspace.register_action(|workspace, _: &SyncBranch, window, cx| {
            if let Some(panel) = reveal_source_control(workspace, window, cx) {
                panel.update(cx, |panel, cx| panel.sync_branch(cx));
            }
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
            .when(!self.turns.is_empty(), |this| {
                this.child(self.render_turns(cx))
            })
            .when(self.icarus_workspace.is_some(), |this| {
                this.child(self.render_git_bar(cx))
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
        assert_eq!(scope_badge(&ChangesScope::Base, None, None), None);
        assert_eq!(
            scope_badge(&ChangesScope::Commit("0123456789".into()), None, None)
                .map(|(text, _)| text),
            Some("Commit 0123456".into())
        );
        assert_eq!(
            scope_badge(&ChangesScope::Turn("t1".into()), None, Some("t2")).map(|(text, _)| text),
            Some("Historical changes".into())
        );
        assert_eq!(
            scope_badge(&ChangesScope::Turn("t2".into()), None, Some("t2")).map(|(text, _)| text),
            Some("Latest turn".into()),
            "a previewed turn is latest until a newer one lands"
        );
    }

    #[test]
    fn discard_is_offered_only_for_local_changes_outside_turns_and_commits() {
        let row = |status: &str, unstaged: bool, untracked: bool, conflicted: bool| Row {
            path: "a.txt".into(),
            status: status.into(),
            old_path: None,
            added: None,
            removed: None,
            binary: false,
            unstaged,
            untracked,
            conflicted,
        };
        let base = ChangesScope::Base;
        assert_eq!(
            discard_label(&row("modified", true, false, false), &base),
            Some("Discard changes")
        );
        assert_eq!(
            discard_label(&row("added", false, true, false), &base),
            Some("Delete untracked file")
        );
        assert_eq!(
            discard_label(&row("deleted", true, false, false), &base),
            Some("Restore file")
        );
        assert_eq!(
            discard_label(&row("modified", false, false, false), &base),
            None,
            "committed only"
        );
        assert_eq!(
            discard_label(&row("modified", true, false, true), &base),
            None,
            "conflicted"
        );
        assert_eq!(
            discard_label(
                &row("modified", true, false, false),
                &ChangesScope::Turn("t".into())
            ),
            None
        );
        assert_eq!(
            discard_label(
                &row("modified", true, false, false),
                &ChangesScope::Uncommitted
            ),
            Some("Discard changes")
        );
    }

    #[test]
    fn turn_rows_read_like_orca() {
        let turn = TurnRow {
            name: "t".into(),
            meta: Some(TurnRowMeta {
                prompt: "\nFix the login\nand more".into(),
                agent_id: "agent-1".into(),
                provider: Some("claude".into()),
                completed_at: 1_000_000,
            }),
            files: 2,
            added: 4,
            removed: 1,
        };
        assert_eq!(turn.title(), "Fix the login");
        assert_eq!(
            turn.detail(1_000_000 + 3 * 60_000),
            "claude · 3m ago · 2 files +4 −1"
        );
        // The server sends turn metadata in camelCase, as git stores it.
        let parsed: TurnRow = serde_json::from_value(json!({
            "name": "001794797580408-80175ea3", "oid": "80175ea3", "parent": "p",
            "files": 2, "added": 4, "removed": 1,
            "meta": { "agentId": "agent-1", "completedAt": 1_000_000, "prompt": "\nFix the login\nand more",
                      "provider": "claude", "startedAt": 999_000 },
        }))
        .expect("server turn");
        assert_eq!(parsed.meta, turn.meta);
        assert_eq!(relative_age(30_000), "just now");
        assert_eq!(relative_age(2 * 3_600_000), "2h ago");
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

    /// A Source Control panel in a real workspace, already loaded, with RPCs answered by `answer`.
    async fn git_panel(
        cx: &mut gpui::TestAppContext,
        answer: impl Fn(&str, &serde_json::Value) -> anyhow::Result<serde_json::Value>
        + Send
        + Sync
        + 'static,
    ) -> (
        Entity<Workspace>,
        Entity<SourceControlPanel>,
        std::sync::Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
        gpui::VisualTestContext,
    ) {
        let (workspace, mut cx) = crate::diff_tab::tests::git_workspace(cx).await;
        let calls = cx.update(|_, cx| crate::review::tests::stub_rpc(cx, answer));
        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = cx.new(|cx| SourceControlPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        panel.update(&mut cx, |panel, _| {
            let mut listing = sample_listing();
            listing.rows[0].unstaged = true;
            panel.listing = Some(listing);
            panel.icarus_workspace = Some("ws-1".into());
            panel.refreshing = Some(Task::ready(()));
            panel.sync = Some(SyncInfo {
                branch: Some("feature".into()),
                upstream: Some("origin/feature".into()),
                published: true,
                ahead: 2,
                behind: 0,
                has_uncommitted_changes: true,
            });
        });
        (workspace, panel, calls, cx)
    }

    #[gpui::test]
    async fn discarding_asks_first_and_sends_only_the_chosen_file(cx: &mut gpui::TestAppContext) {
        let (_workspace, panel, calls, mut cx) =
            git_panel(cx, |_, _| Ok(json!({ "discarded": [] }))).await;
        let cx = &mut cx;
        panel.update(cx, |panel, cx| panel.select(0, cx));

        cx.dispatch_action(DiscardSelectedChange);
        cx.run_until_parked();
        assert!(cx.has_pending_prompt(), "discarding asks first");
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(crate::review::tests::calls_to(&calls, "git.discard").is_empty());

        cx.dispatch_action(DiscardSelectedChange);
        cx.run_until_parked();
        cx.simulate_prompt_answer("Discard changes");
        cx.run_until_parked();
        assert_eq!(
            crate::review::tests::calls_to(&calls, "git.discard"),
            [json!({ "workspace": "ws-1", "paths": ["src/app.rs"] })]
        );

        // A committed-only row has nothing to discard, so nothing is asked.
        panel.update(cx, |panel, cx| panel.select(1, cx));
        cx.dispatch_action(DiscardSelectedChange);
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
    }

    #[gpui::test]
    async fn the_commit_dialog_commits_on_cmd_enter(cx: &mut gpui::TestAppContext) {
        let (workspace, _panel, calls, mut cx) = git_panel(cx, |method, _| match method {
            "git.commit" => {
                Ok(json!({ "oid": "abc", "summary": "Fix login", "committed_all": true }))
            }
            _ => Ok(json!({})),
        })
        .await;
        let cx = &mut cx;
        cx.update(|_, cx| {
            cx.bind_keys([gpui::KeyBinding::new(
                "cmd-enter",
                crate::commit_modal::ConfirmCommit,
                Some("IcarusCommit > Editor"),
            )])
        });

        cx.dispatch_action(CommitChanges);
        cx.run_until_parked();
        let modal = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<crate::commit_modal::CommitModal>(cx)
            })
            .expect("the commit dialog opens");
        cx.simulate_keystrokes("cmd-enter");
        cx.run_until_parked();
        assert!(
            crate::review::tests::calls_to(&calls, "git.commit").is_empty(),
            "no message, no commit"
        );

        cx.simulate_input("Fix login");
        modal.read_with(cx, |modal, cx| {
            assert_eq!(modal.message_text(cx), "Fix login")
        });
        cx.simulate_keystrokes("cmd-enter");
        cx.run_until_parked();
        assert_eq!(
            crate::review::tests::calls_to(&calls, "git.commit"),
            [json!({ "workspace": "ws-1", "message": "Fix login", "amend": false })]
        );
        assert!(
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .active_modal::<crate::commit_modal::CommitModal>(cx)
                    .is_none()
            }),
            "committing closes the dialog"
        );
    }

    #[gpui::test]
    async fn push_shows_progress_and_git_errors_verbatim(cx: &mut gpui::TestAppContext) {
        let reject = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let rejecting = reject.clone();
        let (_workspace, panel, calls, mut cx) = git_panel(cx, move |method, _| {
            if method == "git.push" && rejecting.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(anyhow::anyhow!(icarus_client::ClientError::Rpc(
                    icarus_protocol::RpcError::new(
                        "git_failed",
                        "! [rejected] feature -> feature (stale info)"
                    )
                )));
            }
            Ok(json!({
                "branch": "feature", "upstream": "origin/feature", "published": true,
                "ahead": 0, "behind": 0, "has_uncommitted_changes": false
            }))
        })
        .await;
        let cx = &mut cx;

        cx.dispatch_action(ForcePushWithLease);
        cx.run_until_parked();
        assert_eq!(
            crate::review::tests::calls_to(&calls, "git.push"),
            [json!({ "workspace": "ws-1", "force_with_lease": true })]
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.git_error.as_deref(),
                Some("! [rejected] feature -> feature (stale info)"),
                "git's own words, without the error code"
            );
            assert!(panel.git_busy.is_none());
            assert_eq!(
                panel.sync.as_ref().map(|sync| sync.ahead),
                Some(2),
                "unchanged"
            );
        });

        reject.store(false, std::sync::atomic::Ordering::SeqCst);
        cx.dispatch_action(PushBranch);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.git_error.is_none(),
                "a new attempt clears the old error"
            );
            assert_eq!(panel.sync.as_ref().map(|sync| sync.ahead), Some(0));
        });

        // While one runs, a second press is ignored rather than queued.
        panel.update(cx, |panel, cx| {
            panel.git_busy = Some("Pulling…".into());
            panel.push(false, cx);
        });
        assert_eq!(crate::review::tests::calls_to(&calls, "git.push").len(), 2);
    }

    #[gpui::test]
    async fn clicking_a_turn_filters_the_list_to_it(cx: &mut gpui::TestAppContext) {
        let (_workspace, panel, _calls, mut cx) = git_panel(cx, |_, _| Ok(json!({}))).await;
        let cx = &mut cx;
        let turn = |name: &str, prompt: &str| TurnRow {
            name: name.into(),
            meta: Some(TurnRowMeta {
                prompt: prompt.into(),
                agent_id: "agent-1".into(),
                provider: Some("claude".into()),
                completed_at: 0,
            }),
            files: 1,
            added: 1,
            removed: 0,
        };
        panel.update(cx, |panel, _| {
            panel.turns = vec![turn("t2", "Second"), turn("t1", "First")];
        });

        panel.update_in(cx, |panel, window, cx| panel.open_turn(1, window, cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.scope(), &ChangesScope::Turn("t1".into()));
            assert_eq!(
                scope_badge(
                    panel.scope(),
                    None,
                    panel.turns.first().map(|t| t.name.as_str())
                )
                .map(|(text, _)| text),
                Some("Historical changes".into())
            );
        });
        panel.update_in(cx, |panel, window, cx| panel.open_turn(0, window, cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.scope(), &ChangesScope::Turn("t2".into()));
            assert_eq!(
                scope_badge(
                    panel.scope(),
                    None,
                    panel.turns.first().map(|t| t.name.as_str())
                )
                .map(|(text, _)| text),
                Some("Latest turn".into())
            );
        });
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
