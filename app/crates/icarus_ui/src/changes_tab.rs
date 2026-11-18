use collections::HashMap;
use editor::Editor;
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
use schemars::JsonSchema;
use serde::Deserialize;
use ui::{
    ButtonLike, ContextMenu, ContextMenuSubmenu, PopoverMenu, Tooltip, prelude::*, right_click_menu,
};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent, PanelSizeState};

use crate::AppConnection;
use crate::diff_tab::{DiffBase, DiffTabMode, ReadyDiff, open_diff_tab};
use crate::github_panel::{GitHubPanel, PrLifecycle, PullRequest};
use crate::review::ReviewSection;
use crate::source_control_settings::{
    OpenInApp, Section, SectionLayout, SourceControlSettings, reveal_label,
};

/// Shows or hides a Source Control section (`pending-review`, `changes`, `agent-turns` or `commits`).
#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Action)]
#[action(namespace = icarus)]
#[serde(deny_unknown_fields)]
pub struct ToggleSourceControlSection {
    pub section: String,
}

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
/// How often a window that isn't in front re-reads the list; each read is a `git status` on the server.
const BACKGROUND_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
/// Key repeat arrives far faster than two deliberate presses, so steps closer than this are a held key.
const HOLD_REPEAT: Duration = Duration::from_millis(150);
/// How long a step's diff may take to load before the active tab counts as where to step from again.
const STEP_LOAD_GRACE: Duration = Duration::from_secs(2);
/// Files on each side of the selection whose diffs are kept loaded, so stepping shows them in the same frame.
const READY_AROUND: usize = 8;
/// How long a loaded diff is trusted before it's reloaded; a rebase or fetch can move the merge base.
const READY_TTL: Duration = Duration::from_secs(20);

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

/// Which rows the list shows (spec/source-control.md §2.1). One per tab, so the shortcuts flip it in place.
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
    /// Every row in scope, hidden ones included, so facet toggles re-filter without the server.
    #[serde(default)]
    pub all_rows: Option<Vec<Row>>,
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
    /// Commits on the branch the base doesn't have, and the other way round.
    #[serde(default)]
    pub ahead: Option<u64>,
    #[serde(default)]
    pub behind: Option<u64>,
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
    pub committed: bool,
    #[serde(default)]
    pub staged: bool,
    #[serde(default)]
    pub unstaged: bool,
    #[serde(default)]
    pub untracked: bool,
    #[serde(default)]
    pub conflicted: bool,
    #[serde(default)]
    pub facets: HashMap<String, Option<String>>,
}

impl Listing {
    /// Re-filters `all_rows` by the facets' hidden values, as the server would.
    fn apply_hidden(&mut self) -> bool {
        let Some(all_rows) = &self.all_rows else {
            return false;
        };
        let hidden: HashMap<&str, Vec<&str>> = self
            .facets
            .iter()
            .map(|facet| {
                let values = facet
                    .values
                    .iter()
                    .filter(|value| value.hidden)
                    .map(|value| value.id.as_str())
                    .collect();
                (facet.id.as_str(), values)
            })
            .collect();
        let rows: Vec<Row> = all_rows
            .iter()
            .filter(|row| {
                row.facets.iter().all(|(facet, value)| {
                    value.as_deref().is_none_or(|value| {
                        !hidden
                            .get(facet.as_str())
                            .is_some_and(|hidden| hidden.contains(&value))
                    })
                })
            })
            .cloned()
            .collect();
        self.totals = Totals {
            files: rows.len(),
            added: rows.iter().filter_map(|row| row.added).sum(),
            removed: rows.iter().filter_map(|row| row.removed).sum(),
        };
        self.rows = rows;
        true
    }

    /// Carries `from`'s hidden values over, for a reply that was asked before the user toggled one.
    fn take_hidden_from(&mut self, from: &Listing) {
        for facet in &mut self.facets {
            let Some(source) = from.facets.iter().find(|source| source.id == facet.id) else {
                continue;
            };
            for value in &mut facet.values {
                if let Some(source) = source.values.iter().find(|source| source.id == value.id) {
                    value.hidden = source.hidden;
                }
            }
            facet.active = facet
                .values
                .iter()
                .any(|value| value.present && value.hidden);
        }
        self.apply_hidden();
    }
}

fn local_scope(scope: &ChangesScope) -> bool {
    matches!(scope, ChangesScope::Base | ChangesScope::Uncommitted)
}

/// Staging clears a conflict's unresolved state, so conflicted files are left to the merge tools.
pub(crate) fn can_stage(row: &Row, scope: &ChangesScope) -> bool {
    local_scope(scope) && !row.conflicted && (row.unstaged || row.untracked)
}

pub(crate) fn can_unstage(row: &Row, scope: &ChangesScope) -> bool {
    local_scope(scope) && !row.conflicted && row.staged
}

/// The row's index state, as Orca's Staged / Changes sections would place it: `None` when nothing is staged.
pub(crate) fn staged_label(row: &Row) -> Option<&'static str> {
    if !row.staged {
        None
    } else if row.unstaged || row.untracked {
        Some("Partly staged")
    } else {
        Some("Staged")
    }
}

/// The paths a stage or unstage of `row` covers: a staged rename brings its old path back too.
fn index_paths(row: &Row, unstage: bool) -> Vec<String> {
    let mut paths = vec![row.path.clone()];
    if unstage
        && row.status == "renamed"
        && !row.committed
        && let Some(old_path) = &row.old_path
    {
        paths.push(old_path.clone());
    }
    paths
}

/// What a changed file's right-click menu offers, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    OpenDiff,
    OpenFile,
    Reveal,
    CopyPath,
    CopyRelativePath,
    /// The OS file manager; the menu puts the "Open in" apps right before it.
    RevealInFileManager,
    Stage,
    Unstage,
    Discard(&'static str),
}

impl RowAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenDiff => "Open Diff",
            Self::OpenFile => "Open File",
            Self::Reveal => "Reveal in Project Panel",
            Self::CopyPath => "Copy Path",
            Self::CopyRelativePath => "Copy Relative Path",
            Self::RevealInFileManager => reveal_label(),
            Self::Stage => "Stage",
            Self::Unstage => "Unstage",
            Self::Discard(label) => label,
        }
    }

    /// The menu puts a separator between groups.
    fn group(self) -> u8 {
        match self {
            Self::OpenDiff | Self::OpenFile | Self::Reveal => 0,
            Self::CopyPath | Self::CopyRelativePath => 1,
            Self::RevealInFileManager => 2,
            Self::Stage | Self::Unstage => 3,
            Self::Discard(_) => 4,
        }
    }
}

/// The row's menu: a deleted file has nothing on disk to open or reveal, and only local changes can be
/// staged, unstaged or discarded.
pub(crate) fn row_actions(row: &Row, scope: &ChangesScope) -> Vec<RowAction> {
    let on_disk = row.status != "deleted";
    let mut actions = vec![RowAction::OpenDiff];
    if on_disk {
        actions.extend([RowAction::OpenFile, RowAction::Reveal]);
    }
    actions.extend([RowAction::CopyPath, RowAction::CopyRelativePath]);
    if on_disk {
        actions.push(RowAction::RevealInFileManager);
    }
    if can_stage(row, scope) {
        actions.push(RowAction::Stage);
    }
    if can_unstage(row, scope) {
        actions.push(RowAction::Unstage);
    }
    if let Some(label) = discard_label(row, scope) {
        actions.push(RowAction::Discard(label));
    }
    actions
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
    pub oid: String,
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

/// One commit on the workspace branch since its base (`git.commits`), newest first.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CommitRow {
    pub oid: String,
    #[serde(default)]
    pub short: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub author: String,
    /// Seconds since the epoch.
    #[serde(default)]
    pub time: i64,
}

impl CommitRow {
    /// `abc1234 · Ada · 3h ago`.
    fn detail(&self, now_ms: u64) -> String {
        let at_ms = u64::try_from(self.time).unwrap_or_default() * 1000;
        format!(
            "{} · {} · {}",
            self.short,
            self.author,
            relative_age(now_ms.saturating_sub(at_ms))
        )
    }
}

/// One file of an expanded commit (`git.commit_files`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CommitFile {
    pub path: String,
    pub status: String,
    #[serde(default)]
    pub old_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct CommitFilesReply {
    files: Vec<CommitFile>,
}

/// An expanded commit's files, fetched on first expand.
#[derive(Debug, Clone, PartialEq)]
pub enum CommitFiles {
    Loading,
    Loaded(Vec<CommitFile>),
    Failed(SharedString),
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
struct BranchCommits {
    #[serde(default)]
    commits: Vec<CommitRow>,
    #[serde(default)]
    truncated: bool,
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

/// One line of the Changes tree: a folder, or a file by its index in the listing's rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeEntry {
    Folder {
        path: String,
        name: String,
        depth: usize,
        files: usize,
        collapsed: bool,
    },
    File {
        index: usize,
        depth: usize,
    },
}

/// The rows as a folder tree, folders before files at each level, skipping what folded folders hide.
pub fn tree_entries(paths: &[&str], collapsed: &collections::HashSet<String>) -> Vec<TreeEntry> {
    #[derive(Default)]
    struct Folder {
        folders: std::collections::BTreeMap<String, Folder>,
        files: Vec<(String, usize)>,
        count: usize,
    }
    let mut root = Folder::default();
    for (index, path) in paths.iter().enumerate() {
        let mut parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        let Some(name) = parts.pop() else {
            continue;
        };
        let mut folder = &mut root;
        folder.count += 1;
        for part in parts {
            folder = folder.folders.entry(part.to_owned()).or_default();
            folder.count += 1;
        }
        folder.files.push((name.to_owned(), index));
    }
    fn walk(
        folder: &Folder,
        prefix: &str,
        depth: usize,
        collapsed: &collections::HashSet<String>,
        out: &mut Vec<TreeEntry>,
    ) {
        for (name, child) in &folder.folders {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let folded = collapsed.contains(&path);
            out.push(TreeEntry::Folder {
                path: path.clone(),
                name: name.clone(),
                depth,
                files: child.count,
                collapsed: folded,
            });
            if !folded {
                walk(child, &path, depth + 1, collapsed, out);
            }
        }
        let mut files = folder.files.clone();
        files.sort();
        out.extend(
            files
                .into_iter()
                .map(|(_, index)| TreeEntry::File { index, depth }),
        );
    }
    let mut out = Vec::new();
    walk(&root, "", 0, collapsed, &mut out);
    out
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
    /// The file the last step opened and when, while its diff may still be loading.
    stepped_to: Option<(PathBuf, Instant)>,
    /// Diffs of the files around the selection, loaded so a step shows its file at once.
    ready_diffs: HashMap<(PathBuf, DiffBase), (ReadyDiff, Instant)>,
    warming: HashMap<(PathBuf, DiffBase), Task<()>>,
    /// Bumped by each facet toggle, so a list asked for before it doesn't undo it.
    filter_epoch: u64,
    /// Facet toggles the server hasn't saved yet; lists read meanwhile keep the local choice.
    filter_writes: usize,
    /// Folders folded shut in the tree view, by path.
    collapsed_dirs: collections::HashSet<String>,
    /// Open notes per changed file, shown as a badge on its row.
    note_counts: HashMap<String, usize>,
    /// Unresolved PR review threads per changed file.
    pr_thread_counts: HashMap<String, usize>,
    /// The workspace's agent turns, newest first.
    turns: Vec<TurnRow>,
    turns_collapsed: bool,
    /// The branch's commits since its base, newest first.
    commits: Vec<CommitRow>,
    commits_truncated: bool,
    /// Collapsed by default, as Orca's Commits section is.
    commits_collapsed: bool,
    /// Commits expanded to list their files, by oid.
    expanded_commits: HashMap<String, CommitFiles>,
    changes_collapsed: bool,
    review_collapsed: bool,
    /// Where the PR header and Review section read the pull request; the workspace's Checks panel
    /// unless a preview names one.
    pull_request_panel: Option<WeakEntity<GitHubPanel>>,
    /// The header's file search while it's open, and the rows it leaves.
    file_filter: Option<Entity<Editor>>,
    filtered_rows: Option<Vec<Row>>,
    _file_filter_edits: Option<gpui::Subscription>,
    /// The user's `sourceControl` settings, from the app's own server.
    settings: SourceControlSettings,
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
    _window_activation: Option<gpui::Subscription>,
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
                let mut last_refresh: Option<Instant> = None;
                loop {
                    let now = Instant::now();
                    let refreshed = this.update_in(cx, |this, window, cx| {
                        // A window in the back still catches up now and then; coming forward refreshes it at once.
                        let due = window.is_window_active()
                            || last_refresh.is_none_or(|at| {
                                now.duration_since(at) >= BACKGROUND_REFRESH_INTERVAL
                            });
                        if this.active && due {
                            last_refresh = Some(now);
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
            stepped_to: None,
            ready_diffs: HashMap::default(),
            warming: HashMap::default(),
            filter_epoch: 0,
            filter_writes: 0,
            collapsed_dirs: Default::default(),
            note_counts: HashMap::default(),
            pr_thread_counts: HashMap::default(),
            turns: Vec::new(),
            turns_collapsed: false,
            commits: Vec::new(),
            commits_truncated: false,
            commits_collapsed: true,
            expanded_commits: HashMap::default(),
            changes_collapsed: false,
            review_collapsed: false,
            pull_request_panel: None,
            file_filter: None,
            filtered_rows: None,
            _file_filter_edits: None,
            settings: SourceControlSettings::default(),
            sync: None,
            git_busy: None,
            git_error: None,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            refreshing: None,
            position: DockPosition::Right,
            active: false,
            _poll: poll,
            _window_activation: crate::enabled(cx).then(|| {
                cx.observe_window_activation(window, |this, window, cx| {
                    if this.active && window.is_window_active() {
                        this.refresh(cx);
                    }
                })
            }),
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
            stepped_to: None,
            ready_diffs: HashMap::default(),
            warming: HashMap::default(),
            filter_epoch: 0,
            filter_writes: 0,
            collapsed_dirs: Default::default(),
            note_counts: HashMap::default(),
            pr_thread_counts: HashMap::default(),
            turns: Vec::new(),
            turns_collapsed: false,
            commits: Vec::new(),
            commits_truncated: false,
            commits_collapsed: true,
            expanded_commits: HashMap::default(),
            changes_collapsed: false,
            review_collapsed: false,
            pull_request_panel: None,
            file_filter: None,
            filtered_rows: None,
            _file_filter_edits: None,
            settings: SourceControlSettings::default(),
            sync: None,
            git_busy: None,
            git_error: None,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            refreshing: None,
            position: DockPosition::Right,
            active: true,
            _poll: Task::ready(()),
            _window_activation: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn show_listing_for_tests(&mut self, listing: Listing, cx: &mut Context<Self>) {
        self.listing = Some(listing);
        self.refilter(cx);
        self.refreshing = Some(Task::ready(()));
        self.warm_diffs(cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn ready_diff_count_for_tests(&self) -> usize {
        self.ready_diffs.len()
    }

    #[cfg(test)]
    pub(crate) fn selected_for_tests(&self) -> Option<usize> {
        self.selected
    }

    /// Fills the turns and branch state a server would, for previews.
    pub fn set_git_preview(&mut self, turns: Vec<TurnRow>, sync: SyncInfo, cx: &mut Context<Self>) {
        self.turns = turns;
        self.sync = Some(sync);
        cx.notify();
    }

    /// Fills the Commits section, open, with the given commits expanded to their files, for previews.
    pub fn set_commits_preview(
        &mut self,
        commits: Vec<CommitRow>,
        expanded: Vec<(String, Vec<CommitFile>)>,
        cx: &mut Context<Self>,
    ) {
        self.commits = commits;
        self.commits_collapsed = false;
        self.expanded_commits = expanded
            .into_iter()
            .map(|(oid, files)| (oid, CommitFiles::Loaded(files)))
            .collect();
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

    /// The server that owns this window's workspace (a saved one, or the app's own connection).
    fn client(&self, cx: &App) -> Option<Arc<Client>> {
        // Through the project, not the workspace: this runs inside workspace updates.
        crate::routing::client_for_weak_project(&self.project, cx).or_else(|| {
            cx.try_global::<AppConnection>()
                .and_then(|connection| connection.client.clone())
        })
    }

    /// Reads `sourceControl` from the app's own server, which holds the user's settings.
    fn load_settings(&mut self, cx: &mut Context<Self>) {
        let Some(task) = crate::review::request(
            crate::app_client(cx),
            "config.get",
            serde_json::json!({ "key": "sourceControl" }),
            cx,
        ) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let Some(value) = task.await.log_err() else {
                return;
            };
            this.update(cx, |this, cx| {
                let settings = SourceControlSettings::from_settings(&value);
                if this.settings != settings {
                    this.settings = settings;
                    cx.notify();
                }
            })
            .log_err();
        })
        .detach();
    }

    pub fn settings(&self) -> &SourceControlSettings {
        &self.settings
    }

    /// Saves a new section layout to the user's settings; the panel shows it at once.
    pub fn set_section_layout(&mut self, layout: SectionLayout, cx: &mut Context<Self>) {
        if self.settings.sections == layout {
            return;
        }
        self.settings.sections = layout.clone();
        cx.notify();
        if let Some(task) = crate::review::request(
            crate::app_client(cx),
            "config.set",
            serde_json::json!({ "key": "sourceControl.sections", "value": layout.to_value() }),
            cx,
        ) {
            task.detach_and_log_err(cx);
        }
    }

    pub fn toggle_section(&mut self, section: Section, cx: &mut Context<Self>) {
        let layout = self.settings.sections.toggled_hidden(section);
        self.set_section_layout(layout, cx);
    }

    pub fn move_section(
        &mut self,
        section: Section,
        before: Option<Section>,
        cx: &mut Context<Self>,
    ) {
        let layout = self.settings.sections.moved(section, before);
        self.set_section_layout(layout, cx);
    }

    pub fn move_section_by_one(&mut self, section: Section, up: bool, cx: &mut Context<Self>) {
        if let Some(layout) = self.settings.sections.moved_by_one(section, up) {
            self.set_section_layout(layout, cx);
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing.is_some() {
            return;
        }
        self.load_settings(cx);
        let Some(client) = self.client(cx) else {
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
        let epoch = self.filter_epoch;
        let fetch = Tokio::spawn_result(cx, {
            let client = client.clone();
            async move {
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
                        serde_json::json!({ "id": id, "scope": scope.wire(), "all_rows": true }),
                    )
                    .await?;
                anyhow::Ok((id, serde_json::from_value::<Listing>(value)?))
            }
        });
        let refresh = cx.spawn(async move |this, cx| {
            let result = fetch.await;
            // The list shows as soon as it arrives; notes, turns, branch state and commits follow.
            let id = this.update(cx, |this, cx| {
                cx.notify();
                match result {
                    Ok((id, mut listing)) => {
                        this.icarus_workspace = Some(id.clone());
                        this.error = None;
                        if (this.filter_epoch != epoch || this.filter_writes > 0)
                            && let Some(current) = &this.listing
                        {
                            listing.take_hidden_from(current);
                        }
                        if this.listing.as_ref() != Some(&listing) {
                            this.show_listing(listing, cx);
                        }
                        Some(id)
                    }
                    Err(error) => {
                        this.refreshing = None;
                        this.error = Some(format!("{error:#}").into());
                        None
                    }
                }
            })?;
            let Some(id) = id else {
                return anyhow::Ok(());
            };
            let extras = this.update(cx, |_, cx| {
                Tokio::spawn_result(cx, async move {
                    // Asked together, so one slow extra (a long branch's commits) holds back none of the others.
                    let (notes, turns, sync, commits, threads) = futures::join!(
                        client.request("note.list", serde_json::json!({ "workspace": id })),
                        client.request("workspace.turns", serde_json::json!({ "id": id })),
                        client.request("git.status", serde_json::json!({ "workspace": id })),
                        client.request("git.commits", serde_json::json!({ "workspace": id })),
                        client.request(
                            "review.threads",
                            serde_json::json!({ "workspace": id, "unresolved": true })
                        ),
                    );
                    anyhow::Ok((notes, turns, sync, commits, threads))
                })
            })?;
            let (notes, turns, sync, commits, threads) = extras.await?;
            this.update(cx, |this, cx| {
                this.refreshing = None;
                // A failed extra just leaves its part empty.
                this.note_counts = notes
                    .ok()
                    .map(|value| note_counts(&value))
                    .unwrap_or_default();
                this.pr_thread_counts = threads
                    .ok()
                    .map(|value| crate::pr_comments::open_thread_counts(&value))
                    .unwrap_or_default();
                this.turns = turns
                    .ok()
                    .and_then(|value| serde_json::from_value::<Vec<TurnRow>>(value).ok())
                    .unwrap_or_default();
                this.sync = sync
                    .ok()
                    .and_then(|value| serde_json::from_value::<SyncInfo>(value).ok());
                let commits = commits
                    .ok()
                    .and_then(|value| serde_json::from_value::<BranchCommits>(value).ok())
                    .unwrap_or_default();
                this.commits = commits.commits;
                this.commits_truncated = commits.truncated;
                let commits = &this.commits;
                this.expanded_commits
                    .retain(|oid, _| commits.iter().any(|commit| &commit.oid == oid));
                cx.notify();
            })
        });
        self.refreshing = Some(cx.spawn(async move |_, _| {
            refresh.await.log_err();
        }));
    }

    fn rows(&self) -> &[Row] {
        if let Some(rows) = &self.filtered_rows {
            return rows;
        }
        self.listing
            .as_ref()
            .map(|listing| listing.rows.as_slice())
            .unwrap_or_default()
    }

    /// Hides or shows one facet value for every client (the server persists it), updating the list at once.
    pub(crate) fn toggle_facet_value(
        &mut self,
        facet: String,
        value: String,
        cx: &mut Context<Self>,
    ) {
        let mut now_hidden = true;
        let Some(mut listing) = self.listing.clone() else {
            return;
        };
        if let Some(entry) = listing.facets.iter_mut().find(|entry| entry.id == facet) {
            if let Some(item) = entry.values.iter_mut().find(|item| item.id == value) {
                item.hidden = !item.hidden;
                now_hidden = item.hidden;
            }
            entry.active = entry
                .values
                .iter()
                .any(|value| value.present && value.hidden);
        }
        listing.apply_hidden();
        self.filter_epoch += 1;
        self.show_listing(listing, cx);
        cx.notify();
        let (Some(id), Some(client)) = (self.icarus_workspace.clone(), self.client(cx)) else {
            return;
        };
        self.filter_writes += 1;
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
                this.filter_writes = this.filter_writes.saturating_sub(1);
                this.refreshing = None;
                this.refresh(cx)
            })
            .log_err();
        })
        .detach();
    }

    /// Shows `listing`, keeping the selected file selected, and keeps the diffs around it ready.
    fn show_listing(&mut self, listing: Listing, cx: &mut Context<Self>) {
        let selected_path = self
            .selected
            .and_then(|index| self.rows().get(index))
            .map(|row| row.path.clone());
        // A file that changed kind (deleted, renamed, re-added) needs its diff read again.
        let kinds: HashMap<PathBuf, (&str, Option<&str>)> = listing
            .all_rows
            .as_ref()
            .unwrap_or(&listing.rows)
            .iter()
            .map(|row| {
                (
                    self.root.join(&row.path),
                    (row.status.as_str(), row.old_path.as_deref()),
                )
            })
            .collect();
        let previous_kinds: HashMap<PathBuf, (&str, Option<&str>)> = self
            .listing
            .iter()
            .flat_map(|previous| previous.all_rows.as_ref().unwrap_or(&previous.rows))
            .map(|row| {
                (
                    self.root.join(&row.path),
                    (row.status.as_str(), row.old_path.as_deref()),
                )
            })
            .collect();
        self.ready_diffs.retain(|(path, _), _| {
            kinds
                .get(path)
                .is_some_and(|kind| previous_kinds.get(path) == Some(kind))
        });
        self.listing = Some(listing);
        self.refilter(cx);
        if let Some(selected) = self.selected {
            let rows = self.rows();
            self.selected = selected_path
                .and_then(|path| rows.iter().position(|row| row.path == path))
                .or_else(|| rows.len().checked_sub(1).map(|last| selected.min(last)));
        }
        self.warm_diffs(cx);
        cx.notify();
    }

    /// Orca's header search: narrows the listed files to those whose path holds the typed text.
    fn refilter(&mut self, cx: &App) {
        let query = self
            .file_filter
            .as_ref()
            .map(|editor| editor.read(cx).text(cx).trim().to_lowercase())
            .unwrap_or_default();
        self.filtered_rows = (!query.is_empty()).then(|| {
            self.listing
                .iter()
                .flat_map(|listing| listing.rows.iter())
                .filter(|row| row.path.to_lowercase().contains(&query))
                .cloned()
                .collect()
        });
        let count = self.rows().len();
        self.selected = self
            .selected
            .and_then(|selected| count.checked_sub(1).map(|last| selected.min(last)));
    }

    pub fn open_file_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = match &self.file_filter {
            Some(editor) => editor.clone(),
            None => {
                let editor = cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text("Filter files…", window, cx);
                    editor
                });
                self._file_filter_edits = Some(cx.subscribe(
                    &editor,
                    |this, _, event: &editor::EditorEvent, cx| {
                        if matches!(event, editor::EditorEvent::BufferEdited) {
                            this.refilter(cx);
                            cx.notify();
                        }
                    },
                ));
                self.file_filter = Some(editor.clone());
                editor
            }
        };
        window.focus(&editor.focus_handle(cx), cx);
        cx.notify();
    }

    pub fn close_file_filter(&mut self, cx: &mut Context<Self>) {
        self.file_filter = None;
        self._file_filter_edits = None;
        self.refilter(cx);
        cx.notify();
    }

    /// Loads the diffs of the files within `READY_AROUND` steps of the selection and lets the rest go.
    fn warm_diffs(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project.upgrade() else {
            return;
        };
        let Some(base) = self
            .listing
            .as_ref()
            .and_then(|listing| diff_base_for(&self.scope, listing))
        else {
            self.ready_diffs.clear();
            self.warming.clear();
            return;
        };
        let order = self.step_order(cx);
        let count = order.len();
        let position = self
            .selected
            .and_then(|index| order.iter().position(|&row| row == index));
        // With nothing selected the first step lands on either end.
        let window: Vec<usize> = match position {
            Some(position) => (position.saturating_sub(READY_AROUND)
                ..(position + READY_AROUND + 1).min(count))
                .collect(),
            None => (0..READY_AROUND.min(count))
                .chain(count.saturating_sub(READY_AROUND)..count)
                .collect(),
        };
        let wanted: Vec<(PathBuf, DiffBase)> = window
            .into_iter()
            .filter_map(|position| order.get(position))
            .filter_map(|&index| self.rows().get(index))
            .map(|row| (self.root.join(&row.path), base.clone()))
            .collect();
        self.ready_diffs.retain(|key, _| wanted.contains(key));
        self.warming.retain(|key, _| wanted.contains(key));
        let now = cx.background_executor().now();
        for key in wanted {
            let fresh = self
                .ready_diffs
                .get(&key)
                .is_some_and(|(_, at)| now.saturating_duration_since(*at) < READY_TTL);
            if fresh || self.warming.contains_key(&key) {
                continue;
            }
            let load = crate::diff_tab::load_ready_diff(&project, &key.0, &key.1, cx);
            let task = cx.spawn({
                let key = key.clone();
                async move |this, cx| {
                    let ready = load.await;
                    this.update(cx, |this, cx| {
                        this.warming.remove(&key);
                        match ready {
                            Ok(ready) => {
                                let now = cx.background_executor().now();
                                this.ready_diffs.insert(key, (ready, now));
                            }
                            Err(error) => log::debug!("preloading a diff failed: {error:#}"),
                        }
                    })
                    .log_err();
                }
            });
            self.warming.insert(key, task);
        }
    }

    fn ready_diff(&self, path: &Path, base: &DiffBase) -> Option<ReadyDiff> {
        self.ready_diffs
            .get(&(path.to_path_buf(), base.clone()))
            .map(|(ready, _)| ready.clone())
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected = Some(index);
        let line = if self.tree_view(cx) {
            // A file inside a folded folder shows once its folders open.
            if let Some(path) = self.rows().get(index).map(|row| row.path.clone()) {
                let mut folder = Path::new(&path).parent();
                while let Some(parent) = folder.filter(|parent| !parent.as_os_str().is_empty()) {
                    self.collapsed_dirs
                        .remove(parent.to_string_lossy().as_ref());
                    folder = parent.parent();
                }
            }
            self.tree()
                .iter()
                .position(
                    |entry| matches!(entry, TreeEntry::File { index: file, .. } if *file == index),
                )
                .unwrap_or(index)
        } else {
            index
        };
        self.scroll.scroll_to_item(line, ScrollStrategy::Center);
        self.warm_diffs(cx);
        cx.notify();
    }

    /// Whether the Changes list shows a folder tree; this app remembers the choice.
    pub fn tree_view(&self, cx: &App) -> bool {
        crate::client_settings::get(Some(crate::client_settings::CHANGES_VIEW), cx) == "tree"
    }

    pub fn toggle_tree_view(&mut self, cx: &mut Context<Self>) {
        let next = if self.tree_view(cx) { "list" } else { "tree" };
        crate::client_settings::set(
            crate::client_settings::CHANGES_VIEW,
            Some(serde_json::json!(next)),
            cx,
        )
        .log_err();
        cx.notify();
    }

    /// Row indices in the order the list shows them, folded folders included, so stepping follows the view.
    fn step_order(&self, cx: &App) -> Vec<usize> {
        if !self.tree_view(cx) {
            return (0..self.rows().len()).collect();
        }
        let paths: Vec<&str> = self.rows().iter().map(|row| row.path.as_str()).collect();
        tree_entries(&paths, &Default::default())
            .into_iter()
            .filter_map(|entry| match entry {
                TreeEntry::File { index, .. } => Some(index),
                TreeEntry::Folder { .. } => None,
            })
            .collect()
    }

    pub fn tree(&self) -> Vec<TreeEntry> {
        let paths: Vec<&str> = self.rows().iter().map(|row| row.path.as_str()).collect();
        tree_entries(&paths, &self.collapsed_dirs)
    }

    pub fn toggle_folder(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.collapsed_dirs.remove(path) {
            self.collapsed_dirs.insert(path.to_owned());
        }
        cx.notify();
    }

    /// Steps through the rows and opens the file there; a single press wraps (§5.3).
    pub fn step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.step_from(forward, None, DiffTabMode::Diff, window, cx);
    }

    /// Steps from `current` (the open diff tab's file) when it's in the list, else from the selection.
    /// A single press wraps; a held key's repeats stop at the ends (Orca's changed-file-hold-navigation).
    /// The file opens as the preview tab, in the view (`mode`) the current one showed.
    pub fn step_from(
        &mut self,
        forward: bool,
        current: Option<&Path>,
        mode: DiffTabMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = cx.background_executor().now();
        let held = self.last_step.is_some_and(|(direction, at)| {
            direction == forward && now.saturating_duration_since(at) < HOLD_REPEAT
        });
        self.last_step = Some((forward, now));
        let order = self.step_order(cx);
        if order.is_empty() {
            return;
        }
        // While the last step's diff still loads the active tab is the file before it, so step on from there.
        let loading = self.stepped_to.as_ref().is_some_and(|(path, at)| {
            current != Some(path.as_path()) && now.saturating_duration_since(*at) < STEP_LOAD_GRACE
        });
        let from_tab = current.filter(|_| !loading).and_then(|current| {
            self.rows()
                .iter()
                .position(|row| self.root.join(&row.path) == current)
        });
        let position = from_tab
            .or(self.selected)
            .and_then(|index| order.iter().position(|&row| row == index));
        let Some(next) = next_file(position, order.len(), forward, held)
            .and_then(|position| order.get(position).copied())
        else {
            return;
        };
        self.stepped_to = self
            .rows()
            .get(next)
            .map(|row| (self.root.join(&row.path), now));
        self.select(next, cx);
        self.open_selected_as(Opening::Preview(mode), window, cx);
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_selected_as(Opening::Preview(DiffTabMode::Diff), window, cx);
    }

    fn open_selected_as(&mut self, opening: Opening, window: &mut Window, cx: &mut Context<Self>) {
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
        let root = self.root.clone();
        let listing = self.listing.as_ref();
        let base = listing.and_then(|listing| diff_base_for(&scope, listing));
        let ready = base
            .as_ref()
            .and_then(|base| self.ready_diff(&root.join(&row.path), base));
        let sha = match &scope {
            ChangesScope::Commit(sha) => Some(sha.clone()),
            _ => listing
                .and_then(|listing| listing.turn.as_ref())
                .map(|turn| turn.oid.clone()),
        };
        // Deferred: the step actions run inside a Workspace update, which would double-lease it.
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                open_row(
                    &row, base, sha, ready, &root, opening, workspace, window, cx,
                );
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
        let detail = if row.untracked {
            format!(
                "{}\n\nThis file isn't tracked by git, so discarding deletes it from disk. This can't be undone.",
                row.path
            )
        } else {
            format!(
                "{}\n\nOnly the uncommitted change is thrown away; what's committed on the branch stays. This can't be undone.",
                row.path
            )
        };
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

    /// Runs one of the row menu's actions on the row at `index`.
    pub fn run_row_action(
        &mut self,
        index: usize,
        action: RowAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.rows().get(index).cloned() else {
            return;
        };
        let absolute = self.root.join(&row.path);
        match action {
            RowAction::OpenDiff => {
                self.select(index, cx);
                self.open_selected(window, cx);
            }
            RowAction::OpenFile => {
                let Some(workspace) = self.workspace.upgrade() else {
                    return;
                };
                // A file's editor and diff are one tab's two views (features §2.9), so this is its edit view.
                let base = self
                    .listing
                    .as_ref()
                    .and_then(|listing| diff_base_for(&self.scope, listing))
                    .unwrap_or(DiffBase::Uncommitted);
                // Deferred: the menu that ran this may still be inside a Workspace update.
                window.defer(cx, move |window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        open_diff_tab(
                            absolute,
                            base,
                            DiffTabMode::Edit,
                            None,
                            workspace,
                            window,
                            cx,
                        )
                        .detach_and_log_err(cx);
                    });
                });
            }
            RowAction::Reveal => {
                let Some(project) = self.project.upgrade() else {
                    return;
                };
                window.defer(cx, move |_, cx| {
                    project.update(cx, |project, cx| {
                        let entry = project
                            .find_project_path(&absolute, cx)
                            .and_then(|path| project.entry_for_path(&path, cx))
                            .map(|entry| entry.id);
                        if let Some(entry) = entry {
                            cx.emit(project::Event::RevealInProjectPanel(entry));
                        }
                    });
                });
            }
            RowAction::CopyPath => cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                absolute.to_string_lossy().into_owned(),
            )),
            RowAction::CopyRelativePath => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(row.path))
            }
            RowAction::RevealInFileManager => cx.reveal_path(&absolute),
            RowAction::Stage => self.stage_rows(vec![index], true, cx),
            RowAction::Unstage => self.stage_rows(vec![index], false, cx),
            RowAction::Discard(_) => self.discard_row(index, window, cx),
        }
    }

    /// Opens the row's file in an external app from the "Open in" menu; a failure shows in the git bar.
    pub fn open_row_in(&mut self, index: usize, app: OpenInApp, cx: &mut Context<Self>) {
        let Some(row) = self.rows().get(index) else {
            return;
        };
        let task = crate::source_control_settings::open_in(&app, &self.root.join(&row.path), cx);
        cx.spawn(async move |this, cx| {
            if let Err(error) = task.await {
                this.update(cx, |this, cx| {
                    this.git_error =
                        Some(format!("Couldn't open in {}: {error:#}", app.label).into());
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }

    /// Whether this window's files are on this machine, so external apps and the file manager can open them.
    fn files_are_local(&self, cx: &App) -> bool {
        self.project
            .upgrade()
            .is_some_and(|project| project.read(cx).is_local())
    }

    /// Stages (or unstages) the rows at `indices` that can be, in one `git.stage` / `git.unstage` call.
    pub fn stage_rows(&mut self, indices: Vec<usize>, stage: bool, cx: &mut Context<Self>) {
        let Some(workspace) = self.icarus_workspace.clone() else {
            return;
        };
        let paths: Vec<String> = indices
            .into_iter()
            .filter_map(|index| self.rows().get(index))
            .filter(|row| {
                if stage {
                    can_stage(row, &self.scope)
                } else {
                    can_unstage(row, &self.scope)
                }
            })
            .flat_map(|row| index_paths(row, !stage))
            .collect();
        if paths.is_empty() {
            return;
        }
        let (method, busy) = if stage {
            ("git.stage", "Staging…")
        } else {
            ("git.unstage", "Unstaging…")
        };
        self.run_git(
            method,
            serde_json::json!({ "workspace": workspace, "paths": paths }),
            busy,
            cx,
        );
    }

    /// Stages every listed change that can be (Orca's Stage all, over the rows the filters leave).
    pub fn stage_all(&mut self, stage: bool, cx: &mut Context<Self>) {
        let indices = (0..self.rows().len()).collect();
        self.stage_rows(indices, stage, cx);
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
        let Some(task) = crate::review::request(self.client(cx), method, params, cx) else {
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

    /// Shows what the turn changed: the turn scope plus the turn's snapshot in the commit view.
    pub fn open_turn(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(turn) = self.turns.get(index).cloned() else {
            return;
        };
        self.set_scope(ChangesScope::Turn(turn.name.clone()), cx);
        if !turn.oid.is_empty() {
            self.open_commit_view(turn.oid, window, cx);
        }
    }

    /// Shows a branch commit: the commit scope plus the whole commit in the commit view.
    pub fn open_commit(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(commit) = self.commits.get(index).cloned() else {
            return;
        };
        self.set_scope(ChangesScope::Commit(commit.oid.clone()), cx);
        self.open_commit_view(commit.oid, window, cx);
    }

    /// Orca's commit row click: shows or hides the commit's files, loading them on first expand.
    pub fn toggle_commit(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(oid) = self.commits.get(index).map(|commit| commit.oid.clone()) else {
            return;
        };
        if self.expanded_commits.remove(&oid).is_some() {
            cx.notify();
            return;
        }
        let Some(workspace) = self.icarus_workspace.clone() else {
            return;
        };
        let Some(task) = crate::review::request(
            self.client(cx),
            "git.commit_files",
            serde_json::json!({ "workspace": workspace, "sha": oid }),
            cx,
        ) else {
            self.expanded_commits.insert(
                oid,
                CommitFiles::Failed("Not connected to the Icarus server".into()),
            );
            cx.notify();
            return;
        };
        self.expanded_commits
            .insert(oid.clone(), CommitFiles::Loading);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let files = task
                .await
                .and_then(|value| Ok(serde_json::from_value::<CommitFilesReply>(value)?.files));
            this.update(cx, |this, cx| {
                // Collapsed again while loading: stay collapsed.
                let Some(entry) = this.expanded_commits.get_mut(&oid) else {
                    return;
                };
                *entry = match files {
                    Ok(files) => CommitFiles::Loaded(files),
                    Err(error) => CommitFiles::Failed(git_error_text(&error).into()),
                };
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    pub fn commit_files(&self, oid: &str) -> Option<&CommitFiles> {
        self.expanded_commits.get(oid)
    }

    /// One file's diff in the commit (against its first parent), in the commit view.
    pub fn open_commit_file(
        &mut self,
        oid: String,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                open_commit_file_view(oid, &path, workspace, window, cx);
            });
        });
    }

    fn open_commit_view(&self, sha: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        // Deferred: a click lands inside this panel's update, and the commit view reads the workspace.
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                open_commit_view(sha, None, workspace, window, cx);
            });
        });
    }

    /// The Changes header's controls: the line totals, unstage all, the scope chip and the filter.
    fn render_changes_controls(&self, cx: &mut Context<Self>) -> AnyElement {
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
        let any_unstageable = self.rows().iter().any(|row| can_unstage(row, &self.scope));
        let idle = self.git_busy.is_none();

        h_flex()
            .id("changes-count")
            .min_w_0()
            .overflow_hidden()
            .gap_1p5()
            .when_some(count_tooltip, |this, tooltip| {
                this.tooltip(Tooltip::text(tooltip))
            })
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
            .when(any_unstageable, |this| {
                this.child(
                    IconButton::new("changes-unstage-all", IconName::Dash)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .disabled(!idle)
                        .tooltip(Tooltip::text("Unstage all"))
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| this.stage_all(false, cx)),
                        ),
                )
            })
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
            .into_any_element()
    }

    /// The panel's Checks panel, where the PR header and Review section read the pull request.
    fn pull_request_panel(&self, cx: &App) -> Option<Entity<GitHubPanel>> {
        if let Some(panel) = &self.pull_request_panel {
            return panel.upgrade();
        }
        self.workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).panel::<GitHubPanel>(cx))
    }

    /// Previews and visual tests read the pull request from a standalone Checks panel.
    pub fn set_pull_request_panel(&mut self, panel: &Entity<GitHubPanel>) {
        self.pull_request_panel = Some(panel.downgrade());
    }

    /// The Review section's view, kept on the current PR by the Checks panel that owns it.
    fn review_section(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ReviewSection>> {
        let panel = self.pull_request_panel(cx)?;
        panel.update(cx, |panel, cx| {
            panel.sync_review_section(window, cx);
            panel.review_section().cloned()
        })
    }

    /// Orca's Source Control header: the PR with search and more, then the branch and where it merges.
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pull_request: Option<PullRequest> = self
            .pull_request_panel(cx)
            .and_then(|panel| panel.read(cx).pull_request().cloned());
        let branch = self.sync.as_ref().and_then(|sync| sync.branch.clone());
        let compare = self.listing.as_ref().map(|listing| listing.compare.clone());
        let base = compare.as_ref().and_then(|compare| compare.base.clone());
        let weak = cx.entity().downgrade();
        let layout = self.settings.sections.clone();
        let tree = self.tree_view(cx);
        let mono = |text: String| {
            Label::new(text)
                .size(LabelSize::Custom(rems_from_px(10.5_f32)))
                .weight(FontWeight::MEDIUM)
                .buffer_font(cx)
                .single_line()
                .truncate()
        };
        let pr_row = h_flex()
            .h(px(28.))
            .gap_1()
            .child(match &pull_request {
                Some(pull_request) => {
                    let color = match pull_request.lifecycle() {
                        PrLifecycle::Open => Color::Success,
                        PrLifecycle::Merged => Color::Custom(gpui::rgb(0xa371f7).into()),
                        PrLifecycle::Closed => Color::Error,
                        PrLifecycle::Draft | PrLifecycle::Unknown => Color::Muted,
                    };
                    h_flex()
                        .id("scm-pr-link")
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .cursor_pointer()
                        .child(
                            Icon::new(IconName::PullRequest)
                                .size(IconSize::XSmall)
                                .color(color),
                        )
                        .child(
                            Label::new(format!("PR #{}", pull_request.number))
                                .size(LabelSize::Custom(rems_from_px(11.5_f32)))
                                .single_line(),
                        )
                        .tooltip(Tooltip::text(pull_request.title.clone()))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            let Some(workspace) = this.workspace.upgrade() else {
                                return;
                            };
                            // Deferred: opening Checks deactivates this panel, which is mid-update here.
                            window.defer(cx, move |window, cx| {
                                workspace.update(cx, |workspace, cx| {
                                    workspace.open_panel::<GitHubPanel>(window, cx);
                                });
                            });
                        }))
                        .into_any_element()
                }
                None => div().flex_1().into_any_element(),
            })
            .child(
                IconButton::new("scm-filter-files", IconName::MagnifyingGlass)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Filter files by name"))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_file_filter(window, cx)
                    })),
            )
            .child(
                PopoverMenu::new("scm-header-more")
                    .anchor(gpui::Anchor::TopRight)
                    .trigger(
                        IconButton::new("scm-header-more-trigger", IconName::Ellipsis)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("More actions")),
                    )
                    .menu(move |window, cx| {
                        let weak = weak.clone();
                        let layout = layout.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            let toggle = weak.clone();
                            let menu = menu.entry(
                                if tree { "View as List" } else { "View as Tree" },
                                None,
                                move |_, cx| {
                                    toggle
                                        .update(cx, |this, cx| this.toggle_tree_view(cx))
                                        .log_err();
                                },
                            );
                            let sections = weak.clone();
                            menu.separator().header("Sections").map(|mut menu| {
                                for item in layout.ordered() {
                                    let sections = sections.clone();
                                    menu = menu.toggleable_entry(
                                        item.label(),
                                        !layout.is_hidden(item),
                                        IconPosition::Start,
                                        None,
                                        move |_, cx| {
                                            sections
                                                .update(cx, |this, cx| {
                                                    this.toggle_section(item, cx)
                                                })
                                                .log_err();
                                        },
                                    );
                                }
                                menu
                            })
                        }))
                    }),
            );
        let ahead = compare.as_ref().and_then(|compare| compare.ahead);
        let behind = compare.as_ref().and_then(|compare| compare.behind);
        let base_row = base.map(|base| {
            h_flex()
                .min_w_0()
                .gap(px(6.))
                .child(
                    Label::new("→")
                        .size(LabelSize::Custom(rems_from_px(10.5_f32)))
                        .color(Color::Muted),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(mono(base.clone()).underline()),
                )
                .when_some(ahead, |row, ahead| {
                    row.child(
                        div()
                            .id("scm-ahead")
                            .child(
                                Label::new(format!("↑{ahead}"))
                                    .size(LabelSize::Custom(rems_from_px(10.5_f32)))
                                    .color(Color::Muted),
                            )
                            .tooltip(Tooltip::text(format!(
                                "{ahead} commit{} ahead of {base}",
                                if ahead == 1 { "" } else { "s" }
                            ))),
                    )
                })
                .when_some(behind, |row, behind| {
                    row.child(
                        div()
                            .id("scm-behind")
                            .child(
                                Label::new(format!("↓{behind}"))
                                    .size(LabelSize::Custom(rems_from_px(10.5_f32)))
                                    .color(Color::Muted),
                            )
                            .tooltip(Tooltip::text(format!(
                                "{behind} commit{} behind {base}",
                                if behind == 1 { "" } else { "s" }
                            ))),
                    )
                })
                .when_some(
                    pull_request
                        .as_ref()
                        .map(|pull_request| pull_request.url.clone()),
                    |row, url| {
                        row.child(
                            IconButton::new("scm-open-review", IconName::ArrowUpRight)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Open review page in browser"))
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        )
                    },
                )
        });
        v_flex()
            .flex_none()
            .px(px(12.))
            .pt(px(6.))
            .pb(px(4.))
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(match self.file_filter.clone() {
                // The search takes the whole row while it's open, as in Orca.
                Some(editor) => h_flex()
                    .h(px(28.))
                    .gap(px(6.))
                    .on_action(
                        cx.listener(|this, _: &menu::Cancel, _, cx| this.close_file_filter(cx)),
                    )
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1().min_w_0().child(editor))
                    .child(
                        IconButton::new("scm-filter-files-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Clear and close filter"))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.close_file_filter(cx)
                            })),
                    )
                    .into_any_element(),
                None => pr_row.into_any_element(),
            })
            .when(branch.is_some() || base_row.is_some(), |column| {
                column.child(
                    v_flex()
                        .mt_1()
                        .gap(px(2.))
                        .when_some(branch, |column, branch| {
                            column.child(h_flex().min_h(px(18.)).child(mono(branch)))
                        })
                        .children(base_row),
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
            .filter(|parent| !parent.is_empty() && !self.tree_view(cx));
        let icon = FileIcons::get_icon(path, cx);
        let selected = self.selected == Some(index);
        let color = status_color(&row.status);
        let discard = discard_label(row, &self.scope);
        let stage = can_stage(row, &self.scope);
        let unstage = can_unstage(row, &self.scope);
        let staged = staged_label(row).filter(|_| local_scope(&self.scope));
        let mut actions = row_actions(row, &self.scope);
        if !self.files_are_local(cx) {
            actions.retain(|action| *action != RowAction::RevealInFileManager);
        }
        let open_in = self.settings.open_in.clone();
        let weak = cx.entity().downgrade();
        let element = h_flex()
            .id(("changes-row", index))
            .debug_selector(|| format!("scm-row:{}", row.path))
            .group("changes-row")
            .relative()
            .h(px(26.))
            .px_2()
            .gap_1p5()
            .w_full()
            .cursor_pointer()
            .when(selected, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.select(index, cx);
                let opening = if event.click_count() >= 2 {
                    Opening::Kept
                } else {
                    Opening::Preview(DiffTabMode::Diff)
                };
                this.open_selected_as(opening, window, cx);
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
            .children(crate::pr_comments::open_thread_badge(
                index,
                self.pr_thread_counts.get(&row.path).copied(),
            ))
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
            .when(discard.is_some() || stage || unstage, |this| {
                // Overlaid like Orca's row actions, so rows keep their counts aligned while not hovered.
                this.child(
                    h_flex()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right(px(40.))
                        .pl_1()
                        .gap_0p5()
                        .bg(cx.theme().colors().element_hover)
                        .invisible()
                        .group_hover("changes-row", |style| style.visible())
                        .when_some(discard, |this, label| {
                            this.child(
                                IconButton::new(("changes-row-discard", index), IconName::Undo)
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text(label))
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            cx.stop_propagation();
                                            this.discard_row(index, window, cx);
                                        },
                                    )),
                            )
                        })
                        .when(unstage, |this| {
                            this.child(
                                IconButton::new(("changes-row-unstage", index), IconName::Dash)
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Unstage"))
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        cx.stop_propagation();
                                        this.stage_rows(vec![index], false, cx);
                                    })),
                            )
                        })
                        .when(stage, |this| {
                            this.child(
                                IconButton::new(("changes-row-stage", index), IconName::Plus)
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Stage"))
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        cx.stop_propagation();
                                        this.stage_rows(vec![index], true, cx);
                                    })),
                            )
                        }),
                )
            })
            .when(local_scope(&self.scope), |this| {
                this.child(
                    div()
                        .id(("changes-row-staged", index))
                        .w(px(12.))
                        .flex_none()
                        .when_some(staged, |this, label| {
                            this.tooltip(Tooltip::text(label)).child(
                                Icon::new(if label == "Staged" {
                                    IconName::Check
                                } else {
                                    IconName::Dash
                                })
                                .size(IconSize::XSmall)
                                .color(Color::Created),
                            )
                        }),
                )
            })
            .child(
                Label::new(status_letter(&row.status))
                    .size(LabelSize::XSmall)
                    .color(color),
            );
        right_click_menu(("changes-row-menu", index))
            .trigger(move |_, _, _| element)
            .menu(move |window, cx| {
                row_menu(
                    weak.clone(),
                    index,
                    actions.clone(),
                    open_in.clone(),
                    window,
                    cx,
                )
            })
            .into_any_element()
    }

    /// Orca's Review section: the reviewers, the diff base, and the summary with its verdicts.
    fn render_review(
        &self,
        review: &Entity<ReviewSection>,
        first: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed = self.review_collapsed;
        let pencil = crate::review::reviewer_picker_button(review, cx);
        v_flex()
            .debug_selector(|| "scm-section:review".into())
            .w_full()
            .when(!first, |this| {
                this.border_t_1()
                    .border_color(cx.theme().colors().border_variant)
            })
            .child(self.render_section_header(Section::Review, None, collapsed, vec![pencil], cx))
            .when(!collapsed, |this| this.child(review.clone()))
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
            .child(self.render_section_header(
                Section::AgentTurns,
                Some(self.turns.len().to_string()),
                collapsed,
                Vec::new(),
                cx,
            ))
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

    fn collapsed(&self, section: Section) -> bool {
        match section {
            Section::Review => self.review_collapsed,
            Section::Changes => self.changes_collapsed,
            Section::AgentTurns => self.turns_collapsed,
            Section::Commits => self.commits_collapsed,
        }
    }

    pub fn toggle_collapsed(&mut self, section: Section, cx: &mut Context<Self>) {
        let collapsed = match section {
            Section::Review => &mut self.review_collapsed,
            Section::Changes => &mut self.changes_collapsed,
            Section::AgentTurns => &mut self.turns_collapsed,
            Section::Commits => &mut self.commits_collapsed,
        };
        *collapsed = !*collapsed;
        cx.notify();
    }

    /// A section's header: click collapses it, dragging it onto another header moves it there, and its
    /// right-click menu moves or hides it (Orca's section chrome).
    fn render_section_header(
        &self,
        section: Section,
        count: Option<String>,
        collapsed: bool,
        trailing: Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let toggle = cx.entity().downgrade();
        let target = toggle.clone();
        let weak = toggle.clone();
        let layout = self.settings.sections.clone();
        let mut header = crate::sections::SectionHeader::new(
            format!("changes-{}-header", section.id()),
            section.label(),
            collapsed,
        );
        if let Some(count) = count {
            header = header.count(count);
        }
        for element in trailing {
            header = header.trailing(element);
        }
        header.render(
            DraggedSection(section),
            move |_, cx| {
                toggle
                    .update(cx, |this, cx| this.toggle_collapsed(section, cx))
                    .log_err();
            },
            move |dragged: &DraggedSection, _, cx| {
                target
                    .update(cx, |this, cx| {
                        let layout = this.settings.sections.dropped(dragged.0, section);
                        this.set_section_layout(layout, cx);
                    })
                    .log_err();
            },
            move |window, cx| {
                sections_menu(weak.clone(), Some(section), layout.clone(), window, cx)
            },
        )
    }

    /// The Changes section: its header, then the file list (or why it's empty).
    fn render_changes_section(&mut self, first: bool, cx: &mut Context<Self>) -> AnyElement {
        let count = self.rows().len();
        let collapsed = self.collapsed(Section::Changes);
        v_flex()
            .debug_selector(|| "scm-section:changes".into())
            .w_full()
            .when(!collapsed, |this| this.flex_1().min_h_0())
            .when(!first, |this| {
                this.border_t_1()
                    .border_color(cx.theme().colors().border_variant)
            })
            .child({
                let controls = vec![self.render_changes_controls(cx)];
                self.render_section_header(
                    Section::Changes,
                    Some(count.to_string()),
                    collapsed,
                    controls,
                    cx,
                )
            })
            .when(!collapsed, |this| {
                if count == 0 {
                    this.child(self.render_empty(cx))
                } else {
                    let lines = if self.tree_view(cx) {
                        self.tree().len()
                    } else {
                        count
                    };
                    this.child(
                        uniform_list(
                            "changes-rows",
                            lines,
                            cx.processor(Self::render_rows_processor),
                        )
                        .track_scroll(&self.scroll)
                        .flex_1()
                        .py_1(),
                    )
                }
            })
            .into_any_element()
    }

    fn render_commits(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or_default();
        let collapsed = self.commits_collapsed;
        let showing = match &self.scope {
            ChangesScope::Commit(sha) => Some(sha.clone()),
            _ => None,
        };
        let count = if self.commits_truncated {
            format!("{}+", self.commits.len())
        } else {
            self.commits.len().to_string()
        };
        v_flex()
            .w_full()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(self.render_section_header(
                Section::Commits,
                Some(count),
                collapsed,
                Vec::new(),
                cx,
            ))
            .when(!collapsed && self.commits.is_empty(), |this| {
                this.child(
                    div().px_2().pb_1().child(
                        Label::new("No commits on this branch yet")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
            .when(!collapsed && !self.commits.is_empty(), |this| {
                this.child(
                    v_flex()
                        .id("changes-commits")
                        .max_h(px(220.))
                        .overflow_y_scroll()
                        .pb_1()
                        .children(self.commits.iter().enumerate().map(|(index, commit)| {
                            let current = showing.as_deref() == Some(commit.oid.as_str());
                            let files = self.expanded_commits.get(&commit.oid);
                            let row = h_flex()
                                .id(("changes-commit", index))
                                .px_2()
                                .py_0p5()
                                .gap_1p5()
                                .w_full()
                                .cursor_pointer()
                                .when(current, |this| {
                                    this.bg(cx.theme().colors().element_selected)
                                })
                                .hover(|style| style.bg(cx.theme().colors().element_hover))
                                .tooltip(Tooltip::text(if files.is_some() {
                                    format!(
                                        "Hide files in commit {}: {}",
                                        commit.short, commit.subject
                                    )
                                } else {
                                    format!(
                                        "Show files in commit {}: {}",
                                        commit.short, commit.subject
                                    )
                                }))
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.toggle_commit(index, cx);
                                }))
                                .child(
                                    Icon::new(if files.is_some() {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                                )
                                .child(
                                    Icon::new(IconName::GitCommit)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            Label::new(commit.subject.clone())
                                                .size(LabelSize::Small)
                                                .single_line()
                                                .truncate(),
                                        )
                                        .child(
                                            Label::new(commit.detail(now_ms))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                                .single_line()
                                                .truncate(),
                                        ),
                                );
                            v_flex()
                                .w_full()
                                .child(row)
                                .when_some(files, |this, files| {
                                    this.child(self.render_commit_files(index, commit, files, cx))
                                })
                        })),
                )
            })
    }

    /// An expanded commit: its files (each opens that file's diff in the commit), then Orca's "Show in Changes".
    fn render_commit_files(
        &self,
        index: usize,
        commit: &CommitRow,
        files: &CommitFiles,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let note = |text: SharedString, color: Color| {
            div()
                .pl(px(40.))
                .py_0p5()
                .child(Label::new(text).size(LabelSize::XSmall).color(color))
                .into_any_element()
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        match files {
            CommitFiles::Loading => rows.push(note("Loading files…".into(), Color::Muted)),
            CommitFiles::Failed(error) => rows.push(note(error.clone(), Color::Error)),
            CommitFiles::Loaded(files) if files.is_empty() => {
                rows.push(note("No file changes in this commit".into(), Color::Muted))
            }
            CommitFiles::Loaded(files) => {
                for (file_index, file) in files.iter().enumerate() {
                    let path = Path::new(&file.path);
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| file.path.clone());
                    let directory = path
                        .parent()
                        .map(|parent| parent.to_string_lossy().into_owned())
                        .filter(|parent| !parent.is_empty());
                    let color = status_color(&file.status);
                    let oid = commit.oid.clone();
                    let file_path = file.path.clone();
                    rows.push(
                        h_flex()
                            .id(SharedString::from(format!(
                                "changes-commit-file-{index}-{file_index}"
                            )))
                            .pl(px(40.))
                            .pr_2()
                            .py_0p5()
                            .gap_1p5()
                            .w_full()
                            .cursor_pointer()
                            .hover(|style| style.bg(cx.theme().colors().element_hover))
                            .tooltip(Tooltip::text(match &file.old_path {
                                Some(old_path) => format!("{old_path} → {}", file.path),
                                None => file.path.clone(),
                            }))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.open_commit_file(oid.clone(), file_path.clone(), window, cx);
                            }))
                            .children(FileIcons::get_icon(path, cx).map(|icon| {
                                Icon::from_path(icon).size(IconSize::Small).color(color)
                            }))
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .overflow_hidden()
                                    .child(Label::new(name).size(LabelSize::Small).single_line())
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
                            .child(
                                Label::new(status_letter(&file.status))
                                    .size(LabelSize::XSmall)
                                    .color(color),
                            )
                            .into_any_element(),
                    );
                }
            }
        }
        rows.push(
            h_flex()
                .id(("changes-commit-show", index))
                .pl(px(40.))
                .py_0p5()
                .gap_1()
                .cursor_pointer()
                .hover(|style| style.bg(cx.theme().colors().element_hover))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.open_commit(index, window, cx);
                }))
                .child(
                    Label::new("Show in Changes")
                        .size(LabelSize::XSmall)
                        .color(Color::Accent),
                )
                .into_any_element(),
        );
        v_flex().w_full().children(rows)
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

/// A section header being dragged to a new place.
#[derive(Clone)]
pub struct DraggedSection(pub Section);

impl Render for DraggedSection {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_0p5()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().elevated_surface_background)
            .child(
                Label::new(self.0.label().to_uppercase())
                    .size(LabelSize::XSmall)
                    .weight(FontWeight::SEMIBOLD),
            )
    }
}

/// A section header's menu (or the panel header's, with no section): move or hide the section, and show
/// or hide any section, which is how a hidden one comes back.
fn sections_menu(
    panel: WeakEntity<SourceControlPanel>,
    section: Option<Section>,
    layout: SectionLayout,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |mut menu, _, _| {
        if let Some(section) = section {
            for (label, up) in [("Move Up", true), ("Move Down", false)] {
                let panel = panel.clone();
                menu = menu.item(
                    ui::ContextMenuEntry::new(label)
                        .disabled(layout.moved_by_one(section, up).is_none())
                        .handler(move |_, cx| {
                            panel
                                .update(cx, |panel, cx| panel.move_section_by_one(section, up, cx))
                                .log_err();
                        }),
                );
            }
            let hide = panel.clone();
            menu = menu
                .entry("Hide Section", None, move |_, cx| {
                    hide.update(cx, |panel, cx| panel.toggle_section(section, cx))
                        .log_err();
                })
                .separator();
        }
        menu = menu.header("Sections");
        for item in layout.ordered() {
            let panel = panel.clone();
            menu = menu.toggleable_entry(
                item.label(),
                !layout.is_hidden(item),
                IconPosition::Start,
                None,
                move |_, cx| {
                    panel
                        .update(cx, |panel, cx| panel.toggle_section(item, cx))
                        .log_err();
                },
            );
        }
        menu
    })
}

/// A changed file's right-click menu: open, reveal and copy, then its index and discard actions.
fn row_menu(
    panel: WeakEntity<SourceControlPanel>,
    index: usize,
    actions: Vec<RowAction>,
    open_in: Vec<OpenInApp>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |mut menu, _, _| {
        let mut previous_group = None;
        for action in actions {
            let group = action.group();
            if previous_group.is_some_and(|previous| previous != group) {
                menu = menu.separator();
            }
            previous_group = Some(group);
            if action == RowAction::RevealInFileManager {
                let apps = open_in.clone();
                let panel = panel.clone();
                menu = menu.item(
                    ContextMenuSubmenu::new("Open in", move |mut submenu, _, _| {
                        for app in &apps {
                            let panel = panel.clone();
                            let app = app.clone();
                            submenu = submenu.entry(app.label.clone(), None, move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| {
                                        panel.open_row_in(index, app.clone(), cx)
                                    })
                                    .log_err();
                            });
                        }
                        if !apps.is_empty() {
                            submenu = submenu.separator();
                        }
                        submenu.action(
                            "Customize apps…",
                            Box::new(crate::settings_view::CustomizeOpenInApps),
                        )
                    })
                    .id("changes-row-open-in"),
                );
            }
            let panel = panel.clone();
            menu = menu.entry(action.label(), None, move |window, cx| {
                panel
                    .update(cx, |panel, cx| {
                        panel.run_row_action(index, action, window, cx)
                    })
                    .log_err();
            });
        }
        menu
    })
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
/// How a row's diff opens: as the preview tab (a click, a step) or as a kept tab (a double-click).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Opening {
    Preview(DiffTabMode),
    Kept,
}

#[allow(clippy::too_many_arguments)]
fn open_row(
    row: &Row,
    base: Option<DiffBase>,
    sha: Option<String>,
    ready: Option<ReadyDiff>,
    root: &Path,
    opening: Opening,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(base) = base {
        let path = root.join(&row.path);
        let open = match opening {
            Opening::Preview(mode) => {
                crate::diff_tab::open_diff_preview(path, base, mode, ready, workspace, window, cx)
            }
            Opening::Kept => {
                open_diff_tab(path, base, DiffTabMode::Diff, None, workspace, window, cx)
            }
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = open.await
                && !error.is::<crate::diff_tab::Superseded>()
            {
                log::error!("{error:#}");
            }
        })
        .detach();
        return;
    }
    if let Some(sha) = sha {
        open_commit_view(sha, Some(&row.path), workspace, window, cx);
    }
}

/// Opens `sha` (a branch commit or a turn snapshot) in the commit view, scrolled to `reveal` when given.
fn open_commit_view(
    sha: String,
    reveal: Option<&str>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(repo) = workspace.project().read(cx).active_repository(cx) else {
        return;
    };
    CommitView::open(
        sha,
        repo.downgrade(),
        workspace.weak_handle(),
        CommitViewOptions {
            reveal_file: reveal.and_then(|path| git::repository::RepoPath::new(path).log_err()),
            ..Default::default()
        },
        window,
        cx,
    );
}

/// The commit view narrowed to one file: that file's diff in the commit.
fn open_commit_file_view(
    sha: String,
    path: &str,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(repo) = workspace.project().read(cx).active_repository(cx) else {
        return;
    };
    let Some(file) = git::repository::RepoPath::new(path).log_err() else {
        return;
    };
    CommitView::open(
        sha,
        repo.downgrade(),
        workspace.weak_handle(),
        CommitViewOptions {
            file_filter: Some(file),
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

/// Quick Open's changed files: the Source Control panel's scoped rows as already listed, so no git runs.
pub struct SourceControlChanges;

impl file_finder::ChangedFilesSource for SourceControlChanges {
    fn changed_files(
        &self,
        workspace: &Workspace,
        cx: &App,
    ) -> HashMap<PathBuf, file_finder::ChangedFile> {
        let Some(panel) = workspace.panel::<SourceControlPanel>(cx) else {
            return HashMap::default();
        };
        let panel = panel.read(cx);
        panel
            .rows()
            .iter()
            .map(|row| {
                (
                    panel.root.join(&row.path),
                    file_finder::ChangedFile {
                        status: status_letter(&row.status).into(),
                        status_color: status_color(&row.status),
                        added: row.added,
                        removed: row.removed,
                    },
                )
            })
            .collect()
    }

    fn open_diff(
        &self,
        abs_path: PathBuf,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Option<Task<anyhow::Result<Entity<Editor>>>> {
        let panel = workspace.panel::<SourceControlPanel>(cx)?;
        let base = {
            let panel = panel.read(cx);
            diff_base_for(&panel.scope, panel.listing.as_ref()?)?
        };
        let open = crate::diff_tab::open_diff_preview(
            abs_path,
            base,
            DiffTabMode::Diff,
            None,
            workspace,
            window,
            cx,
        );
        Some(cx.spawn(async move |_, cx| {
            let tab = open.await?;
            Ok(tab.read_with(cx, |tab, _| tab.editor().clone()))
        }))
    }
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
        .map(|tab| {
            let tab = tab.read(cx);
            (tab.path().to_path_buf(), tab.mode())
        });
    let mode = current
        .as_ref()
        .map_or(DiffTabMode::Diff, |(_, mode)| *mode);
    if let Some(changes) = changes_tab(workspace, window, cx) {
        changes.update(cx, |changes, cx| {
            changes.step_from(
                forward,
                current.as_ref().map(|(path, _)| path.as_path()),
                mode,
                window,
                cx,
            )
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
            crate::github_panel::toggle_panel::<SourceControlPanel>(workspace, window, cx);
        });
        workspace.register_action(|workspace, action: &ToggleSourceControlSection, _, cx| {
            let Some(section) = Section::parse(&action.section) else {
                return;
            };
            if let Some(panel) = workspace.panel::<SourceControlPanel>(cx) {
                panel.update(cx, |panel, cx| panel.toggle_section(section, cx));
            }
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
        4
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        let became_active = active && !self.active;
        self.active = active;
        if became_active {
            // Deferred: the workspace is mid-update while it activates a panel.
            cx.defer_in(window, |this, _, cx| this.refresh(cx));
        }
    }
}

/// The Source Control panel's persistent name, which the sidebar switcher also looks it up by.
pub const SOURCE_CONTROL_PANEL: &str = "IcarusSourceControlPanel";

impl Render for SourceControlPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let review = self.review_section(window, cx);
        v_flex()
            .key_context("IcarusChanges")
            .debug_selector(|| "scm-panel".into())
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
            .child({
                let header = self.render_header(cx).into_any_element();
                let weak = cx.entity().downgrade();
                let layout = self.settings.sections.clone();
                right_click_menu("changes-header-menu")
                    .trigger(move |_, _, _| header)
                    .menu(move |window, cx| {
                        sections_menu(weak.clone(), None, layout.clone(), window, cx)
                    })
            })
            .map(|mut this| {
                let mut first = true;
                let mut list_fills = false;
                for section in self.settings.sections.visible() {
                    match section {
                        Section::Changes => {
                            list_fills = !self.changes_collapsed;
                            this = this.child(self.render_changes_section(first, cx));
                        }
                        Section::Review if review.is_some() => {
                            if let Some(review) = &review {
                                this = this.child(self.render_review(review, first, cx));
                            }
                        }
                        Section::AgentTurns if !self.turns.is_empty() => {
                            this = this.child(self.render_turns(cx));
                        }
                        Section::Commits if self.icarus_workspace.is_some() => {
                            this = this.child(self.render_commits(cx));
                        }
                        _ => continue,
                    }
                    first = false;
                }
                // Keeps the git bar at the bottom when no file list takes the space.
                this.when(!list_fills, |this| this.child(div().flex_1()))
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
        if !self.tree_view(cx) {
            return self.render_rows(range, cx);
        }
        let rows = self.rows().to_vec();
        let tree = self.tree();
        range
            .filter_map(|line| tree.get(line).cloned())
            .map(|entry| match entry {
                TreeEntry::Folder {
                    path,
                    name,
                    depth,
                    files,
                    collapsed,
                } => {
                    h_flex()
                        .id(SharedString::from(format!("changes-folder-{path}")))
                        .h(px(26.))
                        .pl(px(8. + depth as f32 * 12.))
                        .pr_2()
                        .gap_1()
                        .w_full()
                        .cursor_pointer()
                        .hover(|style| style.bg(cx.theme().colors().element_hover))
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
                            Icon::new(IconName::Folder)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Label::new(name).size(LabelSize::Small).truncate())
                        .child(
                            Label::new(files.to_string())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.toggle_folder(&path, cx)
                        }))
                        .into_any_element()
                }
                TreeEntry::File { index, depth } => match rows.get(index) {
                    Some(row) => div()
                        .pl(px(depth as f32 * 12. + 12.))
                        .child(self.render_row(index, row, cx))
                        .into_any_element(),
                    None => div().into_any_element(),
                },
            })
            .collect()
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

    #[gpui::test]
    async fn review_renders_inside_source_control_under_the_pr_header(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fixture = json!({
            "current": "ws-1",
            "workspaces": [{ "id": "ws-1", "path": "/project", "branch": "feature" }],
            "pr_status": {
                "known": true,
                "pull_request": {
                    "number": 4209, "title": "Feature", "url": "https://github.com/acme/app/pull/4209",
                    "base_ref_name": "main", "head_ref_name": "feature",
                    "latest_reviews": [{ "login": "vitaliy", "state": "APPROVED" }],
                },
            },
            "review": {
                "workspace": "ws-1",
                "pr": { "repo": "acme/app", "number": 4209, "base_ref_name": "main", "author": "me" },
                "verdicts": ["comment"],
                "can_submit": { "comment": false },
                "base": { "selected": "whole", "options": [
                    { "value": "whole", "label": "main · target branch", "available": true },
                ] },
            },
        });
        let checks = cx.new(|cx| GitHubPanel::preview(WeakEntity::new_invalid(), fixture, cx));
        let (panel, cx) = cx.add_window_view(|_, cx| {
            let mut panel =
                SourceControlPanel::preview(sample_listing(), ChangesScope::Base, None, cx);
            panel.set_pull_request_panel(&checks);
            panel
        });
        cx.run_until_parked();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(
            checks.read_with(cx, |checks, _| checks.review_section().is_some()),
            "Source Control asks the Checks panel for the PR's review"
        );
        let review = cx.debug_bounds("scm-section:review").expect("Review drawn");
        let changes = cx
            .debug_bounds("scm-section:changes")
            .expect("Changes drawn");
        assert!(
            review.origin.y < changes.origin.y,
            "Review sits above Changes, as in Orca"
        );
        assert!(cx.debug_bounds("review-section").is_some());
        panel.update(cx, |panel, cx| panel.toggle_collapsed(Section::Review, cx));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("review-section").is_none(),
            "the header collapses it like any section"
        );
    }

    #[gpui::test]
    async fn the_header_search_narrows_the_files_by_path(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let (panel, cx) = cx.add_window_view(|_, cx| {
            SourceControlPanel::preview(sample_listing(), ChangesScope::Base, Some(2), cx)
        });
        panel.update_in(cx, |panel, window, cx| panel.open_file_filter(window, cx));
        let editor = panel
            .read_with(cx, |panel, _| panel.file_filter.clone())
            .expect("the search opens");
        editor.update_in(cx, |editor, window, cx| editor.set_text("SRC/", window, cx));
        cx.run_until_parked();
        let paths = |panel: &SourceControlPanel| -> Vec<String> {
            panel.rows().iter().map(|row| row.path.clone()).collect()
        };
        panel.read_with(cx, |panel, _| {
            assert_eq!(paths(panel), ["src/app.rs", "src/new.rs"]);
            assert_eq!(panel.selected, Some(1), "the selection stays in the list");
        });
        panel.update(cx, |panel, cx| panel.close_file_filter(cx));
        panel.read_with(cx, |panel, _| assert_eq!(paths(panel).len(), 3));
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
    fn hidden_values_filter_every_row_locally_and_survive_an_older_reply() {
        let listing = |hidden: bool| -> Listing {
            let rows = serde_json::json!([
                { "path": "a.rs", "status": "modified", "added": 3, "facets": { "type": "implementation" } },
                { "path": "a_test.rs", "status": "modified", "added": 5, "facets": { "type": "test" } },
                { "path": "README.md", "status": "modified", "added": 1, "facets": { "type": null } },
            ]);
            serde_json::from_value(serde_json::json!({
                "rows": rows, "all_rows": rows,
                "facets": [{ "id": "type", "label": "Types", "values": [
                    { "id": "implementation", "label": "Implementation", "present": true },
                    { "id": "test", "label": "Tests", "present": true, "hidden": hidden },
                ] }],
            }))
            .expect("listing")
        };
        let mut local = listing(true);
        assert!(local.apply_hidden());
        let paths = |listing: &Listing| -> Vec<String> {
            listing.rows.iter().map(|row| row.path.clone()).collect()
        };
        assert_eq!(paths(&local), ["a.rs", "README.md"]);
        assert_eq!((local.totals.files, local.totals.added), (2, 4));

        let mut stale = listing(false);
        stale.take_hidden_from(&local);
        assert_eq!(paths(&stale), ["a.rs", "README.md"]);
        assert!(stale.facets[0].active);
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
            committed: false,
            staged: false,
            unstaged,
            untracked,
            conflicted,
            facets: HashMap::default(),
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
            oid: "80175ea3".into(),
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
        assert_eq!(parsed.oid, turn.oid);
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
        let (workspace, panel, _calls, mut cx) = git_panel(cx, |_, _| Ok(json!({}))).await;
        let cx = &mut cx;
        let turn = |name: &str, prompt: &str| TurnRow {
            name: name.into(),
            oid: if name == "t1" { "1" } else { "2" }.repeat(40),
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
        cx.run_until_parked();
        let (commit_views, diff_tabs) = open_views(&workspace, cx);
        assert_eq!(commit_views, 2, "each turn opens its own commit view");
        assert_eq!(diff_tabs, 0, "a turn opens no single-file diff tab");
    }

    /// How many commit views and diff tabs the workspace has open.
    fn open_views(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
    ) -> (usize, usize) {
        workspace.read_with(cx, |workspace, cx| {
            (
                workspace.items_of_type::<CommitView>(cx).count(),
                workspace.items_of_type::<crate::DiffTab>(cx).count(),
            )
        })
    }

    #[gpui::test]
    async fn quick_open_reads_the_listing_and_opens_the_diff(cx: &mut gpui::TestAppContext) {
        use file_finder::ChangedFilesSource as _;

        let (workspace, _panel, _calls, mut cx) =
            git_panel(cx, |_, _| Ok(json!({ "ok": true }))).await;
        let cx = &mut cx;
        let changed = workspace.read_with(cx, |workspace, cx| {
            SourceControlChanges.changed_files(workspace, cx)
        });
        assert_eq!(changed.len(), 3);
        let app = &changed[&PathBuf::from("/project/src/app.rs")];
        assert_eq!(
            (app.status.as_ref(), app.added, app.removed),
            ("M", Some(12), Some(3))
        );

        let open = workspace.update_in(cx, |workspace, window, cx| {
            SourceControlChanges.open_diff(
                PathBuf::from("/project/src/app.rs"),
                workspace,
                window,
                cx,
            )
        });
        let editor = open.expect("the listing has a base").await.expect("opens");
        cx.run_until_parked();
        let active = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<crate::DiffTab>())
                .map(|tab| (tab.read(cx).base().clone(), tab.read(cx).editor().clone()))
        });
        let (base, tab_editor) = active.expect("a diff tab is active");
        assert_eq!(base, DiffBase::MergeBase("origin/main".into()));
        assert_eq!(tab_editor, editor);
    }

    #[test]
    fn row_menus_offer_what_the_file_allows() {
        let row = |status: &str, staged: bool, unstaged: bool, untracked: bool| Row {
            path: "src/a.rs".into(),
            status: status.into(),
            old_path: None,
            added: None,
            removed: None,
            binary: false,
            committed: !(staged || unstaged || untracked),
            staged,
            unstaged,
            untracked,
            conflicted: false,
            facets: HashMap::default(),
        };
        let base = ChangesScope::Base;
        assert_eq!(
            row_actions(&row("modified", false, true, false), &base),
            [
                RowAction::OpenDiff,
                RowAction::OpenFile,
                RowAction::Reveal,
                RowAction::CopyPath,
                RowAction::CopyRelativePath,
                RowAction::RevealInFileManager,
                RowAction::Stage,
                RowAction::Discard("Discard changes"),
            ]
        );
        assert_eq!(
            row_actions(&row("modified", true, true, false), &base)[6..],
            [
                RowAction::Stage,
                RowAction::Unstage,
                RowAction::Discard("Discard changes")
            ],
            "partly staged: both ways"
        );
        assert_eq!(
            row_actions(&row("modified", true, false, false), &base)[6..],
            [RowAction::Unstage],
            "fully staged: nothing left to stage or discard"
        );
        assert_eq!(
            row_actions(&row("deleted", false, false, false), &base),
            [
                RowAction::OpenDiff,
                RowAction::CopyPath,
                RowAction::CopyRelativePath
            ],
            "a committed deletion has no file to open or reveal, and nothing local"
        );
        assert_eq!(
            row_actions(
                &row("added", false, false, true),
                &ChangesScope::Commit("c".into())
            )
            .last(),
            Some(&RowAction::RevealInFileManager),
            "no index actions under a commit"
        );
        assert_eq!(
            staged_label(&row("modified", true, false, false)),
            Some("Staged")
        );
        assert_eq!(
            staged_label(&row("modified", true, true, false)),
            Some("Partly staged")
        );
        assert_eq!(staged_label(&row("modified", false, true, false)), None);

        let mut renamed = row("renamed", true, false, false);
        renamed.old_path = Some("src/old.rs".into());
        assert_eq!(index_paths(&renamed, true), ["src/a.rs", "src/old.rs"]);
        assert_eq!(index_paths(&renamed, false), ["src/a.rs"]);
        renamed.committed = true;
        assert_eq!(
            index_paths(&renamed, true),
            ["src/a.rs"],
            "a committed rename's old path is not in the index to restore"
        );
    }

    /// The row menu at `index`, as a right-click would build it.
    fn open_row_menu(
        panel: &Entity<SourceControlPanel>,
        index: usize,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<ContextMenu> {
        let weak = panel.downgrade();
        let actions = panel.read_with(cx, |panel, _| {
            row_actions(&panel.rows()[index], &panel.scope)
        });
        let open_in = crate::source_control_settings::default_open_in();
        cx.update(|window, cx| row_menu(weak, index, actions, open_in, window, cx))
    }

    #[gpui::test]
    async fn the_row_menu_opens_copies_reveals_and_stages(cx: &mut gpui::TestAppContext) {
        let (workspace, panel, calls, mut cx) =
            git_panel(cx, |_, _| Ok(json!({ "ok": true }))).await;
        let cx = &mut cx;
        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
        let revealed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let _subscription = cx.update(|_, cx| {
            let revealed = revealed.clone();
            cx.subscribe(&project, move |_, event, _| {
                if let project::Event::RevealInProjectPanel(entry) = event {
                    revealed.borrow_mut().push(*entry);
                }
            })
        });

        // The first entry, Open Diff, opens the row like a click.
        let menu = open_row_menu(&panel, 0, cx);
        menu.update_in(cx, |menu, window, cx| {
            menu.select_first(&menu::SelectFirst, window, cx);
            menu.confirm(&menu::Confirm, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            open_views(&workspace, cx).1,
            1,
            "Open Diff opens a diff tab"
        );

        let run = |action: RowAction, cx: &mut gpui::VisualTestContext| {
            panel.update_in(cx, |panel, window, cx| {
                panel.run_row_action(0, action, window, cx)
            });
            cx.run_until_parked();
        };
        run(RowAction::CopyPath, cx);
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some("/project/src/app.rs".into())
        );
        run(RowAction::CopyRelativePath, cx);
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some("src/app.rs".into())
        );
        run(RowAction::Reveal, cx);
        assert_eq!(revealed.borrow().len(), 1, "the file is revealed");
        run(RowAction::OpenFile, cx);
        let active = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<crate::DiffTab>())
                .map(|tab| (tab.read(cx).path().to_path_buf(), tab.read(cx).mode()))
        });
        assert_eq!(
            active,
            Some((
                PathBuf::from("/project/src/app.rs"),
                crate::diff_tab::DiffTabMode::Edit
            )),
            "Open File shows the file's tab in its edit view"
        );
        assert_eq!(
            open_views(&workspace, cx).1,
            1,
            "the same tab, not a second one"
        );

        run(RowAction::Stage, cx);
        assert_eq!(
            crate::review::tests::calls_to(&calls, "git.stage"),
            [json!({ "workspace": "ws-1", "paths": ["src/app.rs"] })]
        );
        panel.update(cx, |panel, _| {
            if let Some(listing) = &mut panel.listing {
                listing.rows[0].staged = true;
                listing.rows[0].unstaged = false;
            }
        });
        let menu = open_row_menu(&panel, 0, cx);
        menu.update_in(cx, |menu, window, cx| {
            menu.select_last(window, cx);
            menu.confirm(&menu::Confirm, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            crate::review::tests::calls_to(&calls, "git.unstage"),
            [json!({ "workspace": "ws-1", "paths": ["src/app.rs"] })],
            "a fully staged row's last entry is Unstage"
        );
    }

    #[gpui::test]
    async fn discarding_an_untracked_file_says_it_deletes_it(cx: &mut gpui::TestAppContext) {
        let (_workspace, panel, calls, mut cx) =
            git_panel(cx, |_, _| Ok(json!({ "discarded": [] }))).await;
        let cx = &mut cx;
        panel.update(cx, |panel, _| {
            if let Some(listing) = &mut panel.listing {
                listing.rows[1].untracked = true;
            }
        });
        let menu = open_row_menu(&panel, 1, cx);
        menu.update_in(cx, |menu, window, cx| {
            menu.select_last(window, cx);
            menu.confirm(&menu::Confirm, window, cx);
        });
        cx.run_until_parked();
        let (message, detail) = cx.pending_prompt().expect("discarding asks first");
        assert_eq!(message, "Delete untracked file?");
        assert!(detail.contains("deletes it from disk"), "{detail}");
        cx.simulate_prompt_answer("Delete untracked file");
        cx.run_until_parked();
        assert_eq!(
            crate::review::tests::calls_to(&calls, "git.discard"),
            [json!({ "workspace": "ws-1", "paths": ["src/new.rs"] })]
        );
    }

    #[gpui::test]
    async fn stage_all_and_unstage_all_send_every_eligible_listed_file(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_workspace, panel, calls, mut cx) =
            git_panel(cx, |_, _| Ok(json!({ "ok": true }))).await;
        let cx = &mut cx;
        panel.update(cx, |panel, _| {
            if let Some(listing) = &mut panel.listing {
                // Row 0 is unstaged, row 1 untracked and row 2 a staged deletion.
                listing.rows[1].untracked = true;
                listing.rows[2].staged = true;
            }
        });
        panel.update(cx, |panel, cx| panel.stage_all(true, cx));
        cx.run_until_parked();
        assert_eq!(
            crate::review::tests::calls_to(&calls, "git.stage"),
            [json!({ "workspace": "ws-1", "paths": ["src/app.rs", "src/new.rs"] })]
        );
        panel.update(cx, |panel, cx| panel.stage_all(false, cx));
        cx.run_until_parked();
        assert_eq!(
            crate::review::tests::calls_to(&calls, "git.unstage"),
            [json!({ "workspace": "ws-1", "paths": ["README.md"] })]
        );

        // Under a commit nothing is local, so nothing is sent.
        panel.update(cx, |panel, cx| {
            panel.scope = ChangesScope::Commit("c".into());
            panel.stage_all(true, cx);
        });
        cx.run_until_parked();
        assert_eq!(crate::review::tests::calls_to(&calls, "git.stage").len(), 1);
    }

    #[gpui::test]
    async fn expanding_a_commit_lists_its_files_and_opens_one_files_diff(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, panel, calls, mut cx) = git_panel(cx, |method, params| {
            if method != "git.commit_files" {
                return Ok(json!({}));
            }
            if params["sha"] == json!("b".repeat(40)) {
                anyhow::bail!("no commit bbbb");
            }
            Ok(json!({ "oid": params["sha"], "parent": null, "subject": "Fix login", "files": [
                { "path": "src/app.rs", "status": "modified", "old_path": null, "added": 1, "removed": 1, "binary": false },
                { "path": "src/new.rs", "status": "renamed", "old_path": "src/old.rs", "added": 0, "removed": 0, "binary": false },
            ]}))
        })
        .await;
        let cx = &mut cx;
        let (first, second) = ("a".repeat(40), "b".repeat(40));
        panel.update(cx, |panel, cx| {
            panel.commits = serde_json::from_value(json!([
                { "oid": first, "short": "aaaaaaa", "subject": "Fix login", "author": "Ada", "time": 1 },
                { "oid": second, "short": "bbbbbbb", "subject": "Start", "author": "Bo", "time": 1 },
            ]))
            .expect("commits");
            panel.commits_collapsed = false;
            cx.notify();
        });

        panel.update(cx, |panel, cx| panel.toggle_commit(0, cx));
        cx.run_until_parked();
        assert_eq!(
            crate::review::tests::calls_to(&calls, "git.commit_files"),
            [json!({ "workspace": "ws-1", "sha": first })]
        );
        let paths = panel.read_with(cx, |panel, _| match panel.commit_files(&first) {
            Some(CommitFiles::Loaded(files)) => files
                .iter()
                .map(|file| file.path.clone())
                .collect::<Vec<_>>(),
            other => panic!("not loaded: {other:?}"),
        });
        assert_eq!(paths, ["src/app.rs", "src/new.rs"]);
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.scope().clone()),
            ChangesScope::Base,
            "expanding doesn't change what the Changes list shows"
        );

        panel.update_in(cx, |panel, window, cx| {
            panel.open_commit_file(first.clone(), "src/app.rs".into(), window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            open_views(&workspace, cx),
            (1, 0),
            "the file's diff in the commit opens in the commit view"
        );

        panel.update(cx, |panel, cx| panel.toggle_commit(1, cx));
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.commit_files(&second).cloned()),
            Some(CommitFiles::Failed("no commit bbbb".into()))
        );
        panel.update(cx, |panel, cx| panel.toggle_commit(0, cx));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.commit_files(&first).cloned()),
            None,
            "a second click collapses it"
        );
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn sections_hide_move_and_save_to_the_user_settings(cx: &mut gpui::TestAppContext) {
        let (_workspace, panel, calls, mut cx) = git_panel(cx, |_, _| Ok(json!({}))).await;
        let cx = &mut cx;
        let saved = |calls: &std::sync::Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>| {
            crate::review::tests::calls_to(calls, "config.set")
                .last()
                .cloned()
                .expect("saved")
        };
        cx.dispatch_action(ToggleSourceControlSection {
            section: "agent-turns".into(),
        });
        cx.run_until_parked();
        assert_eq!(
            saved(&calls),
            json!({ "key": "sourceControl.sections", "value": { "order": [], "hidden": ["agent-turns"] } })
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.settings().sections.visible()),
            [Section::Review, Section::Changes, Section::Commits]
        );

        panel.update(cx, |panel, cx| {
            panel.move_section_by_one(Section::Commits, true, cx);
            panel.toggle_section(Section::AgentTurns, cx);
        });
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.settings().sections.visible()),
            [
                Section::Review,
                Section::Commits,
                Section::Changes,
                Section::AgentTurns
            ],
            "moving up skips the hidden section, and showing it again keeps its place"
        );
        assert_eq!(
            saved(&calls)["value"],
            json!({ "order": ["pending-review", "commits", "changes", "agent-turns"], "hidden": [] })
        );

        panel.update(cx, |panel, cx| {
            panel.settings.sections = SectionLayout::default();
            panel.toggle_collapsed(Section::Changes, cx);
        });
        assert!(panel.read_with(cx, |panel, _| panel.collapsed(Section::Changes)));
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn commits_list_the_branch_and_open_in_the_commit_view(cx: &mut gpui::TestAppContext) {
        let (workspace, panel, _calls, mut cx) = git_panel(cx, |_, _| Ok(json!({}))).await;
        let cx = &mut cx;
        let commits: BranchCommits = serde_json::from_value(json!({
            "compare": { "status": "ready", "base": "origin/main" },
            "commits": [
                { "oid": "a".repeat(40), "short": "aaaaaaa", "subject": "Fix login",
                  "author": "Ada", "author_email": "ada@example.com", "time": 1_000 },
                { "oid": "b".repeat(40), "short": "bbbbbbb", "subject": "Start", "author": "Bo", "time": 100 },
            ],
            "truncated": false,
        }))
        .expect("server commits");
        assert_eq!(
            commits.commits[0].detail(1_000_000 + 2 * 60_000),
            "aaaaaaa · Ada · 2m ago"
        );
        panel.update(cx, |panel, cx| {
            assert!(panel.commits_collapsed, "collapsed by default, as in Orca");
            panel.commits = commits.commits;
            cx.notify();
        });

        panel.update_in(cx, |panel, window, cx| panel.open_commit(1, window, cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.scope(), &ChangesScope::Commit("b".repeat(40)));
            assert_eq!(
                scope_badge(panel.scope(), None, None).map(|(text, _)| text),
                Some("Commit bbbbbbb".into())
            );
        });
        assert_eq!(
            open_views(&workspace, cx),
            (1, 0),
            "the whole commit opens in the commit view"
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

        let second = panel.read_with(cx, |panel, _| panel.root.join(&panel.rows()[1].path));
        let preview = |cx: &mut gpui::VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                let pane = workspace.active_pane().read(cx);
                pane.preview_item()
                    .and_then(|item| item.downcast::<crate::DiffTab>())
                    .map(|tab| tab.read(cx).path().to_path_buf())
            })
        };
        assert_eq!(preview(cx), Some(PathBuf::from("/project/src/app.rs")));

        cx.dispatch_action(NextChangedFile);
        cx.run_until_parked();
        assert_eq!(
            opened(cx),
            std::slice::from_ref(&second),
            "the step replaced the preview tab"
        );
        assert_eq!(preview(cx), Some(second));
        cx.dispatch_action(PreviousChangedFile);
        cx.run_until_parked();
        assert_eq!(opened(cx), [PathBuf::from("/project/src/app.rs")]);
        let active = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<crate::DiffTab>())
                .map(|tab| tab.read(cx).path().to_path_buf())
        });
        assert_eq!(active, Some(PathBuf::from("/project/src/app.rs")));
    }

    #[gpui::test]
    async fn a_stepped_preview_is_kept_once_edited_and_keeps_its_view(
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
        let tabs = |cx: &mut gpui::VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .items_of_type::<crate::DiffTab>(cx)
                    .map(|tab| (tab.read(cx).path().to_path_buf(), tab.read(cx).mode()))
                    .collect::<Vec<_>>()
            })
        };

        cx.dispatch_action(NextChangedFile);
        cx.run_until_parked();
        let first = workspace
            .read_with(cx, |workspace, cx| {
                workspace.items_of_type::<crate::DiffTab>(cx).next()
            })
            .expect("a diff tab");
        first.update_in(cx, |tab, window, cx| {
            tab.set_mode(DiffTabMode::Edit, cx);
            tab.editor()
                .update(cx, |editor, cx| editor.insert("// kept\n", window, cx));
        });
        cx.run_until_parked();
        let still_preview = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .is_active_preview_item(first.entity_id())
        });
        assert!(!still_preview, "an edit keeps the tab");

        cx.executor().advance_clock(Duration::from_secs(1));
        cx.dispatch_action(NextChangedFile);
        cx.run_until_parked();
        let second = panel.read_with(cx, |panel, _| panel.root.join(&panel.rows()[1].path));
        assert_eq!(
            tabs(cx),
            [
                (PathBuf::from("/project/src/app.rs"), DiffTabMode::Edit),
                (second, DiffTabMode::Edit),
            ],
            "the kept tab stays, and the next file opens in the same view"
        );
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

    #[test]
    fn the_tree_nests_files_under_counted_folders_and_folds() {
        let paths = ["src/ui/app.rs", "src/new.rs", "README.md", "src/ui/view.rs"];
        let open = tree_entries(&paths, &Default::default());
        assert_eq!(
            open,
            [
                TreeEntry::Folder {
                    path: "src".into(),
                    name: "src".into(),
                    depth: 0,
                    files: 3,
                    collapsed: false
                },
                TreeEntry::Folder {
                    path: "src/ui".into(),
                    name: "ui".into(),
                    depth: 1,
                    files: 2,
                    collapsed: false
                },
                TreeEntry::File { index: 0, depth: 2 },
                TreeEntry::File { index: 3, depth: 2 },
                TreeEntry::File { index: 1, depth: 1 },
                TreeEntry::File { index: 2, depth: 0 },
            ]
        );
        let folded = tree_entries(&paths, &["src/ui".to_owned()].into_iter().collect());
        assert_eq!(folded.len(), 4, "a folded folder hides its files");
    }

    #[gpui::test]
    async fn the_tree_view_is_remembered_and_selecting_opens_folders(
        cx: &mut gpui::TestAppContext,
    ) {
        let window = cx.add_empty_window();
        let changes = window.update(|_, cx| {
            cx.new(|cx| SourceControlPanel::preview(sample_listing(), ChangesScope::Base, None, cx))
        });
        changes.update(window, |changes, cx| {
            assert!(!changes.tree_view(cx));
            changes.toggle_tree_view(cx);
            assert!(changes.tree_view(cx));
            assert_eq!(
                crate::client_settings::get(Some(crate::client_settings::CHANGES_VIEW), cx),
                serde_json::json!("tree")
            );
            changes.toggle_folder("src", cx);
            assert_eq!(
                changes.tree().len(),
                2,
                "src folded: the folder and README.md"
            );
            changes.select(0, cx);
            assert_eq!(changes.selected, Some(0));
            assert_eq!(
                changes.tree().len(),
                4,
                "selecting a file in a folded folder opens it"
            );
        });
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
