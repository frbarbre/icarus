//! The pane's "+" menu, after Orca's: a search over open tabs, past agents, files and URLs, then
//! new terminal, browser and Markdown tabs, one row per agent harness, and the agent settings.

use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use agent_servers::harness::{HARNESSES, Harness};
use collections::HashMap;
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, DismissEvent, Entity, EntityId, Task,
    WeakEntity, Window, actions,
};
use picker::{Picker, PickerDelegate};
use project::ProjectPath;
use serde_json::json;
use terminal_view::TerminalView;
use ui::{
    Icon, IconButton, IconName, KeyBinding, ListItem, ListItemSpacing, PopoverMenu,
    PopoverMenuHandle, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{EmptyPaneContent, ItemHandle, NewCenterTerminal, NewItemMenu, Pane, Workspace};

use crate::history::{HistoryEntry, HistorySource};

actions!(
    icarus,
    [
        /// Opens an untitled Markdown document in the active pane.
        NewMarkdown,
    ]
);

pub const PLACEHOLDER: &str = "Search open tabs, history, files, URLs, agents…";
/// Rows each kind of search result may take, as in Orca.
const RESULT_LIMIT: usize = 4;
/// Files the search looks through; past this a project is searched only in part.
const FILE_LIMIT: usize = 20_000;
const SEARCH_URL: &str = "https://www.google.com/search?q=";
/// Typing pauses this long before past agents are searched on the server.
const HISTORY_DELAY: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, PartialEq)]
pub enum MenuAction {
    NewTerminal,
    NewBrowser,
    NewMarkdown,
    Agent(&'static str),
    AgentSettings,
    /// The open tab at this index of the menu's tabs.
    OpenTab(usize),
    /// The past agent at this index of the menu's history results.
    ResumeAgent(usize),
    OpenUrl(String),
    /// The project file at this index of the menu's files.
    OpenFile(usize),
    WebSearch(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MenuRow {
    pub action: MenuAction,
    pub label: SharedString,
    pub icon: IconName,
    pub separator_after: bool,
}

impl MenuRow {
    fn new(action: MenuAction, label: impl Into<SharedString>, icon: IconName) -> Self {
        Self {
            action,
            label: label.into(),
            icon,
            separator_after: false,
        }
    }
}

struct CreateOption {
    action: MenuAction,
    label: &'static str,
    keywords: &'static [&'static str],
    icon: IconName,
}

const CREATE_OPTIONS: [CreateOption; 3] = [
    CreateOption {
        action: MenuAction::NewTerminal,
        label: "New Terminal",
        keywords: &["terminal", "shell", "new terminal", "new shell"],
        icon: IconName::Terminal,
    },
    CreateOption {
        action: MenuAction::NewBrowser,
        label: "New Browser Tab",
        keywords: &["browser", "new browser", "browser tab", "web"],
        icon: IconName::ToolWeb,
    },
    CreateOption {
        action: MenuAction::NewMarkdown,
        label: "New Markdown",
        keywords: &["markdown", "md", "new markdown", "new file", "mark"],
        icon: IconName::FileMarkdown,
    },
];

/// What the search looks through besides the menu's own rows.
#[derive(Default)]
pub struct SearchSources<'a> {
    pub tabs: &'a [SharedString],
    /// Past agents as (title, harness id).
    pub history: &'a [(SharedString, String)],
    /// Project files by their path in the project.
    pub files: &'a [String],
}

/// The menu's rows for `query`: without one, its fixed rows; with one, what matches it, as Orca
/// orders them (open tabs, menu rows, agents, past agents, then a URL, files and a web search).
pub fn menu_rows(
    query: &str,
    harnesses: &[&'static Harness],
    sources: SearchSources,
) -> Vec<MenuRow> {
    let query = normalize(query);
    if query.is_empty() {
        let mut rows: Vec<MenuRow> = CREATE_OPTIONS
            .iter()
            .map(|option| MenuRow::new(option.action.clone(), option.label, option.icon))
            .collect();
        if let Some(last) = rows.last_mut() {
            last.separator_after = true;
        }
        rows.extend(harnesses.iter().map(|harness| {
            MenuRow::new(MenuAction::Agent(harness.id), harness.name, harness.icon)
        }));
        if let Some(last) = rows.last_mut() {
            last.separator_after = true;
        }
        rows.push(MenuRow::new(
            MenuAction::AgentSettings,
            "Agent settings…",
            IconName::Settings,
        ));
        return rows;
    }

    let mut rows = Vec::new();
    rows.extend(
        ranked(sources.tabs.iter().enumerate(), |(_, title)| {
            score_text(&query, title)
        })
        .take(RESULT_LIMIT)
        .map(|(index, title)| {
            MenuRow::new(MenuAction::OpenTab(index), title.clone(), IconName::File)
        }),
    );
    rows.extend(
        ranked(CREATE_OPTIONS.iter(), |option| {
            score_create_option(&query, option)
        })
        .map(|option| MenuRow::new(option.action.clone(), option.label, option.icon)),
    );
    rows.extend(
        ranked(harnesses.iter(), |harness| score_harness(&query, harness))
            .map(|harness| MenuRow::new(MenuAction::Agent(harness.id), harness.name, harness.icon)),
    );
    rows.extend(sources.history.iter().enumerate().take(RESULT_LIMIT).map(
        |(index, (title, harness))| {
            let icon = agent_servers::harness::harness(harness)
                .map_or(IconName::ZedAgent, |harness| harness.icon);
            MenuRow::new(MenuAction::ResumeAgent(index), title.clone(), icon)
        },
    ));
    let raw_query = query.as_str();
    if let Some(url) = classify_url(raw_query) {
        rows.push(MenuRow::new(
            MenuAction::OpenUrl(url.clone()),
            url,
            IconName::ToolWeb,
        ));
    }
    rows.extend(
        ranked(sources.files.iter().enumerate(), |(_, path)| {
            score_file(&query, path)
        })
        .take(RESULT_LIMIT)
        .map(|(index, path)| {
            MenuRow::new(MenuAction::OpenFile(index), path.clone(), IconName::File)
        }),
    );
    rows.push(MenuRow::new(
        MenuAction::WebSearch(raw_query.to_string()),
        format!("Search the web for “{raw_query}”"),
        IconName::MagnifyingGlass,
    ));
    rows
}

/// Candidates with a positive score, best first; ties keep their order.
fn ranked<T>(
    candidates: impl Iterator<Item = T>,
    score: impl Fn(&T) -> u32,
) -> impl Iterator<Item = T> {
    let mut scored: Vec<(u32, usize, T)> = candidates
        .enumerate()
        .filter_map(|(index, candidate)| {
            let score = score(&candidate);
            (score > 0).then_some((score, index, candidate))
        })
        .collect();
    scored.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    scored.into_iter().map(|(_, _, candidate)| candidate)
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

/// Orca's token match: every query token must be in some candidate token (3 exact, 2 prefix,
/// 1 inside); `None` when one isn't.
fn score_tokens(query: &str, values: &[&str]) -> Option<u32> {
    let candidates: Vec<String> = values.iter().flat_map(|value| tokens(value)).collect();
    let query_tokens = tokens(query);
    if candidates.is_empty() || query_tokens.is_empty() {
        return None;
    }
    query_tokens.iter().try_fold(0, |score, query_token| {
        let best = candidates
            .iter()
            .map(|candidate| {
                if candidate == query_token {
                    3
                } else if candidate.starts_with(query_token.as_str()) {
                    2
                } else if candidate.contains(query_token.as_str()) {
                    1
                } else {
                    0
                }
            })
            .max()
            .unwrap_or(0);
        (best > 0).then_some(score + best)
    })
}

fn score_create_option(query: &str, option: &CreateOption) -> u32 {
    let label = normalize(option.label);
    if query == label {
        return 100;
    }
    let mut values = vec![option.label];
    values.extend(option.keywords);
    let Some(score) = score_tokens(query, &values) else {
        return 0;
    };
    if label.contains(query) || query.contains(&label) {
        80 + score
    } else {
        score
    }
}

/// Exact names or token prefixes of two or more letters only, so one keystroke never picks an agent.
fn score_harness(query: &str, harness: &Harness) -> u32 {
    let compact = |text: &str| text.to_lowercase().replace([' ', '-', '_'], "");
    let mut aliases = vec![harness.id.to_string(), harness.name.to_lowercase()];
    aliases.extend(harness.aliases.iter().map(|alias| alias.to_lowercase()));
    if aliases
        .iter()
        .any(|alias| *alias == query || compact(alias) == compact(query))
    {
        return 1000;
    }
    let candidates: Vec<String> = aliases.iter().flat_map(|alias| tokens(alias)).collect();
    let query_tokens = tokens(query);
    if query_tokens.is_empty() {
        return 0;
    }
    let mut score = 0;
    for query_token in &query_tokens {
        let best = candidates
            .iter()
            .map(|candidate| {
                if candidate == query_token {
                    3
                } else if query_token.chars().count() >= 2
                    && candidate.starts_with(query_token.as_str())
                {
                    2
                } else {
                    0
                }
            })
            .max()
            .unwrap_or(0);
        if best == 0 {
            return 0;
        }
        score += best;
    }
    score
}

fn score_text(query: &str, text: &str) -> u32 {
    let text = normalize(text);
    if text == query {
        return 200;
    }
    if text.starts_with(query) {
        return 150;
    }
    if text.contains(query) {
        return 100;
    }
    score_tokens(query, &[&text]).unwrap_or(0)
}

fn score_file(query: &str, path: &str) -> u32 {
    let path = path.to_lowercase();
    let name = path.rsplit('/').next().unwrap_or(&path);
    if path == query {
        400
    } else if name == query {
        300
    } else if name.starts_with(query) {
        200
    } else if name.contains(query) {
        150
    } else if path.contains(query) {
        100
    } else {
        score_tokens(query, &[&path]).unwrap_or(0)
    }
}

const FILE_EXTENSIONS: &[&str] = &[
    "c", "cpp", "css", "go", "h", "html", "java", "js", "json", "jsx", "lock", "md", "mdx", "py",
    "rb", "rs", "sh", "sql", "swift", "toml", "ts", "tsx", "txt", "yaml", "yml",
];

/// The address a typed query opens in a browser tab, if it is one: an http(s) URL, or a host
/// (a domain, `localhost` or an IPv4 address) with an optional port and path. File names such as
/// `README.md` stay searches.
pub fn classify_url(query: &str) -> Option<String> {
    let query = query.trim();
    if query.is_empty() || query.chars().any(char::is_whitespace) {
        return None;
    }
    let lower = query.to_lowercase();
    if let Some(rest) = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
    {
        let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
        return (!host.is_empty()).then(|| query.to_string());
    }
    let authority = query.split(['/', '?', '#']).next().unwrap_or_default();
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if port.is_some_and(|port| port.is_empty() || !port.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    let host = host.to_lowercase();
    let local = host == "localhost" || is_private_ipv4(&host);
    if !local && !is_ipv4(&host) && !is_domain(&host) {
        return None;
    }
    let scheme = if local { "http" } else { "https" };
    Some(format!("{scheme}://{query}"))
}

fn is_ipv4(host: &str) -> bool {
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.len() <= 3 && part.parse::<u8>().is_ok())
}

fn is_private_ipv4(host: &str) -> bool {
    if !is_ipv4(host) {
        return false;
    }
    let octets: Vec<u8> = host
        .split('.')
        .filter_map(|part| part.parse().ok())
        .collect();
    matches!(
        octets.as_slice(),
        [0 | 10 | 127, ..]
            | [100, 64..=127, ..]
            | [172, 16..=31, ..]
            | [169, 254, ..]
            | [192, 168, ..]
    )
}

fn is_domain(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    let Some(top) = labels.last() else {
        return false;
    };
    labels.len() >= 2
        && top.len() >= 2
        && top.chars().all(|c| c.is_ascii_alphabetic())
        && !FILE_EXTENSIONS.contains(top)
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

type OpenHarnessHandler =
    Rc<dyn Fn(&'static str, &mut Workspace, &mut Window, &mut Context<Workspace>)>;

struct GlobalOpenHarnessHandler(OpenHarnessHandler);

impl gpui::Global for GlobalOpenHarnessHandler {}

/// The app crate owns agent tabs; this sets how the menu opens one running a harness.
pub fn set_open_harness_handler(
    cx: &mut App,
    handler: impl Fn(&'static str, &mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    cx.set_global(GlobalOpenHarnessHandler(Rc::new(handler)));
}

/// Each pane's menu handle, and the menu it last opened.
#[derive(Default)]
struct PaneMenus(HashMap<EntityId, PaneMenu>);

impl gpui::Global for PaneMenus {}

#[derive(Default)]
struct PaneMenu {
    handle: PopoverMenuHandle<Picker<NewTabMenu>>,
    picker: Option<WeakEntity<Picker<NewTabMenu>>>,
}

/// Makes every pane's "+" open this menu.
pub fn register(cx: &mut App) {
    cx.set_global(NewItemMenu {
        render: Rc::new(render_button),
        is_focused: Rc::new(|pane, _window, cx| {
            cx.try_global::<PaneMenus>()
                .and_then(|menus| menus.0.get(&pane))
                .is_some_and(|menu| menu.handle.is_deployed())
        }),
    });
    cx.set_global(EmptyPaneContent(Rc::new(render_empty_pane)));
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &NewMarkdown, window, cx| {
            let pane = workspace.active_pane().clone();
            open_markdown(workspace, pane, window, cx);
        });
    })
    .detach();
}

/// The pane's menu, open, so tests and screenshots can show it.
pub fn show_menu(pane: &Entity<Pane>, window: &mut Window, cx: &mut App) {
    let handle = cx
        .try_global::<PaneMenus>()
        .and_then(|menus| menus.0.get(&pane.entity_id()))
        .map(|menu| menu.handle.clone());
    if let Some(handle) = handle {
        handle.show(window, cx);
    }
}

/// The pane's menu while it is open.
pub fn open_menu(pane: &Entity<Pane>, cx: &App) -> Option<Entity<Picker<NewTabMenu>>> {
    let menu = cx.try_global::<PaneMenus>()?.0.get(&pane.entity_id())?;
    menu.handle
        .is_deployed()
        .then(|| menu.picker.as_ref()?.upgrade())?
}

fn render_button(pane: &mut Pane, _window: &mut Window, cx: &mut Context<Pane>) -> AnyElement {
    let pane_id = cx.entity_id();
    let (handle, first_render) = match cx.default_global::<PaneMenus>().0.entry(pane_id) {
        collections::hash_map::Entry::Occupied(menu) => (menu.get().handle.clone(), false),
        collections::hash_map::Entry::Vacant(slot) => {
            (slot.insert(PaneMenu::default()).handle.clone(), true)
        }
    };
    if first_render {
        cx.on_release(move |_, cx| {
            if cx.has_global::<PaneMenus>() {
                cx.global_mut::<PaneMenus>().0.remove(&pane_id);
            }
        })
        .detach();
    }
    let workspace = pane.workspace();
    let pane = cx.entity().downgrade();
    PopoverMenu::new("icarus-new-tab-menu")
        .trigger_with_tooltip(
            IconButton::new("plus", IconName::Plus).icon_size(IconSize::Small),
            Tooltip::text("New Tab…"),
        )
        .anchor(gpui::Anchor::TopRight)
        .with_handle(handle)
        .menu(move |window, cx| build_menu(workspace.clone(), pane.clone(), window, cx))
        .into_any_element()
}

fn build_menu(
    workspace: WeakEntity<Workspace>,
    pane: WeakEntity<Pane>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Entity<Picker<NewTabMenu>>> {
    let workspace_entity = workspace.upgrade()?;
    let (tabs, files, project) = {
        let workspace = workspace_entity.read(cx);
        let mut tabs = Vec::new();
        for pane in workspace.panes() {
            for item in pane.read(cx).items() {
                tabs.push(OpenTab {
                    title: item.tab_content_text(0, cx),
                    icon: item.tab_icon(window, cx),
                    item: item.boxed_clone(),
                });
            }
        }
        let project = workspace.project().clone();
        let mut files = Vec::new();
        let several_roots = project.read(cx).visible_worktrees(cx).count() > 1;
        'worktrees: for worktree in project.read(cx).visible_worktrees(cx) {
            let worktree = worktree.read(cx);
            let root = worktree.root_name_str().to_string();
            for entry in worktree.files(false, 0) {
                if files.len() >= FILE_LIMIT {
                    break 'worktrees;
                }
                let path = entry.path.as_unix_str();
                let shown = if several_roots {
                    format!("{root}/{path}")
                } else {
                    path.to_string()
                };
                files.push((
                    shown,
                    ProjectPath {
                        worktree_id: worktree.id(),
                        path: entry.path.clone(),
                    },
                ));
            }
        }
        (tabs, files, project)
    };
    let history = crate::enabled(cx).then(HistorySource::server);
    let pane_id = pane.entity_id();
    let delegate = NewTabMenu::new(workspace, pane, tabs, files, history);
    let picker = cx.new(|cx| {
        Picker::uniform_list(delegate, window, cx)
            .popover()
            .initial_width(rems(22.))
    });
    if let Some(menu) = cx.default_global::<PaneMenus>().0.get_mut(&pane_id) {
        menu.picker = Some(picker.downgrade());
    }
    let client = crate::routing::client_for_project(project.read(cx), cx);
    if let Some(default) =
        crate::review::request(client, "config.get", json!({ "key": "agents.default" }), cx)
    {
        let weak_picker = picker.downgrade();
        window
            .spawn(cx, async move |cx| {
                let default = default.await?;
                weak_picker.update_in(cx, |picker, window, cx| {
                    if let Some(default) = default.as_str() {
                        remember_default_harness(default, cx);
                        picker.delegate.set_default_harness(default);
                        picker.refresh(window, cx);
                    }
                })
            })
            .detach_and_log_err(cx);
    }
    Some(picker)
}

struct OpenTab {
    title: SharedString,
    icon: Option<Icon>,
    item: Box<dyn ItemHandle>,
}

pub struct NewTabMenu {
    workspace: WeakEntity<Workspace>,
    pane: WeakEntity<Pane>,
    harnesses: Vec<&'static Harness>,
    default_harness: Option<&'static str>,
    tabs: Vec<OpenTab>,
    tab_titles: Vec<SharedString>,
    files: Vec<(String, ProjectPath)>,
    file_paths: Vec<String>,
    history_source: Option<HistorySource>,
    history: Vec<HistoryEntry>,
    query: String,
    rows: Vec<MenuRow>,
    selected: usize,
}

impl NewTabMenu {
    fn new(
        workspace: WeakEntity<Workspace>,
        pane: WeakEntity<Pane>,
        tabs: Vec<OpenTab>,
        files: Vec<(String, ProjectPath)>,
        history_source: Option<HistorySource>,
    ) -> Self {
        let tab_titles = tabs.iter().map(|tab| tab.title.clone()).collect();
        let file_paths = files.iter().map(|(path, _)| path.clone()).collect();
        let mut menu = Self {
            workspace,
            pane,
            harnesses: HARNESSES.iter().collect(),
            default_harness: None,
            tabs,
            tab_titles,
            files,
            file_paths,
            history_source,
            history: Vec::new(),
            query: String::new(),
            rows: Vec::new(),
            selected: 0,
        };
        menu.update_rows();
        menu
    }

    pub fn rows(&self) -> &[MenuRow] {
        &self.rows
    }

    /// Puts the server's default agent first, where its ⌘⌥T applies.
    fn set_default_harness(&mut self, id: &str) {
        let Some(default) = agent_servers::harness::harness(id) else {
            return;
        };
        self.default_harness = Some(default.id);
        self.harnesses = std::iter::once(default)
            .chain(HARNESSES.iter().filter(|harness| harness.id != default.id))
            .collect();
        self.update_rows();
    }

    fn update_rows(&mut self) {
        let history: Vec<(SharedString, String)> = self
            .history
            .iter()
            .map(|entry| (history_title(entry), entry.provider.clone()))
            .collect();
        self.rows = menu_rows(
            &self.query,
            &self.harnesses,
            SearchSources {
                tabs: &self.tab_titles,
                history: &history,
                files: &self.file_paths,
            },
        );
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    fn shortcut(&self, action: &MenuAction, cx: &App) -> Option<Box<dyn Action>> {
        shortcut_for(action, self.default_harness, cx)
    }

    fn run(&self, action: MenuAction, window: &mut Window, cx: &mut App) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let pane = self
            .pane
            .upgrade()
            .unwrap_or_else(|| workspace.read(cx).active_pane().clone());
        if let MenuAction::ResumeAgent(index) = action {
            if let (Some(source), Some(entry)) = (&self.history_source, self.history.get(index)) {
                (source.resume)(entry.clone(), None, cx);
            }
            return;
        }
        if let MenuAction::OpenTab(index) = action {
            if let Some(tab) = self.tabs.get(index) {
                let item = tab.item.boxed_clone();
                workspace.update(cx, |workspace, cx| {
                    workspace.activate_item(item.as_ref(), true, true, window, cx);
                });
            }
            return;
        }
        let file = match &action {
            MenuAction::OpenFile(index) => self.files.get(*index).map(|(_, path)| path.clone()),
            _ => None,
        };
        run_action(action, file, &workspace, pane, window, cx);
    }

    fn row_icon(&self, row: &MenuRow, cx: &App) -> Icon {
        match row.action {
            MenuAction::OpenTab(index) => self
                .tabs
                .get(index)
                .and_then(|tab| tab.icon.clone())
                .unwrap_or_else(|| Icon::new(row.icon)),
            MenuAction::OpenFile(index) => self
                .file_paths
                .get(index)
                .and_then(|path| file_icons::FileIcons::get_icon(Path::new(path), cx))
                .map_or_else(|| Icon::new(row.icon), Icon::from_path),
            _ => Icon::new(row.icon),
        }
    }
}

fn history_title(entry: &HistoryEntry) -> SharedString {
    entry
        .title
        .as_deref()
        .filter(|title| !title.is_empty() && *title != "Agent")
        .or(entry.first_prompt.as_deref())
        .map(|title| title.lines().next().unwrap_or(title).trim().to_string())
        .unwrap_or_else(|| entry.id.clone())
        .into()
}

fn shortcut_for(
    action: &MenuAction,
    default_harness: Option<&str>,
    cx: &App,
) -> Option<Box<dyn Action>> {
    match action {
        MenuAction::NewTerminal => Some(NewCenterTerminal::default().boxed_clone()),
        MenuAction::NewMarkdown => Some(NewMarkdown.boxed_clone()),
        MenuAction::Agent(id) if Some(*id) == default_harness => {
            cx.build_action("icarus::NewAgent", None).ok()
        }
        _ => None,
    }
}

fn run_action(
    action: MenuAction,
    file: Option<ProjectPath>,
    workspace: &Entity<Workspace>,
    pane: Entity<Pane>,
    window: &mut Window,
    cx: &mut App,
) {
    workspace.update(cx, |workspace, cx| match action {
        MenuAction::NewTerminal => {
            TerminalView::deploy(workspace, &NewCenterTerminal::default(), window, cx)
        }
        MenuAction::NewBrowser => {
            crate::browser::open_browser_tab(String::new(), &pane, true, workspace, window, cx);
        }
        MenuAction::NewMarkdown => open_markdown(workspace, pane, window, cx),
        MenuAction::Agent(id) => match cx
            .try_global::<GlobalOpenHarnessHandler>()
            .map(|handler| handler.0.clone())
        {
            Some(handler) => handler(id, workspace, window, cx),
            None => log::error!("no handler opens {id} agents"),
        },
        MenuAction::AgentSettings => crate::settings_pages::open_agent_settings(window, cx),
        MenuAction::OpenUrl(url) => {
            crate::browser::open_browser_tab(url, &pane, true, workspace, window, cx);
        }
        MenuAction::WebSearch(query) => {
            let url = format!("{SEARCH_URL}{}", urlencoding::encode(&query));
            crate::browser::open_browser_tab(url, &pane, true, workspace, window, cx);
        }
        MenuAction::OpenFile(_) => {
            if let Some(path) = file {
                workspace
                    .open_path(path, Some(pane.downgrade()), true, window, cx)
                    .detach_and_log_err(cx);
            }
        }
        MenuAction::OpenTab(_) | MenuAction::ResumeAgent(_) => {}
    });
}

/// The server's default agent harness, which the menu and empty panes list first with ⌘⌥T.
#[derive(Default)]
struct DefaultHarness {
    id: Option<&'static str>,
    requested: bool,
}

impl gpui::Global for DefaultHarness {}

fn default_harness(cx: &App) -> Option<&'static str> {
    cx.try_global::<DefaultHarness>()
        .and_then(|default| default.id)
}

fn remember_default_harness(id: &str, cx: &mut App) {
    if let Some(harness) = agent_servers::harness::harness(id) {
        cx.default_global::<DefaultHarness>().id = Some(harness.id);
    }
}

/// The rows an empty pane offers: the "+" menu's own rows, with the default agent first.
pub fn empty_pane_rows(default_harness: Option<&str>) -> Vec<MenuRow> {
    let default = default_harness.and_then(agent_servers::harness::harness);
    let harnesses: Vec<&'static Harness> = default
        .into_iter()
        .chain(
            HARNESSES
                .iter()
                .filter(|harness| Some(harness.id) != default.map(|default| default.id)),
        )
        .collect();
    menu_rows("", &harnesses, SearchSources::default())
}

/// What a center pane without tabs shows: the "+" menu's options with their shortcuts, centered.
fn render_empty_pane(
    pane: &mut Pane,
    _window: &mut Window,
    cx: &mut Context<Pane>,
) -> Option<AnyElement> {
    let workspace = pane.workspace().upgrade()?;
    let pane_entity = cx.entity();
    {
        let workspace = workspace.read(cx);
        let is_center = workspace
            .center_pane_group()
            .panes()
            .contains(&&pane_entity);
        let has_worktrees = workspace
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .is_some();
        if !is_center {
            return None;
        }
        if crate::is_workspace_loading(pane.workspace().entity_id(), cx) {
            return Some(
                div()
                    .debug_selector(|| "empty-pane-loading".into())
                    .child(Label::new("Loading…").color(Color::Muted))
                    .into_any_element(),
            );
        }
        if !has_worktrees {
            return None;
        }
    }
    if !cx.default_global::<DefaultHarness>().requested {
        cx.global_mut::<DefaultHarness>().requested = true;
        let client = crate::routing::client_for_project(workspace.read(cx).project().read(cx), cx);
        if let Some(default) =
            crate::review::request(client, "config.get", json!({ "key": "agents.default" }), cx)
        {
            cx.spawn(async move |pane, cx| {
                let default = default.await?;
                pane.update(cx, |_, cx| {
                    if let Some(default) = default.as_str() {
                        remember_default_harness(default, cx);
                    }
                    cx.notify();
                })
            })
            .detach_and_log_err(cx);
        }
    }
    let default = default_harness(cx);
    let rows = empty_pane_rows(default);
    let weak_workspace = workspace.downgrade();
    let weak_pane = pane_entity.downgrade();
    let mut list = v_flex()
        .debug_selector(|| "empty-pane-options".into())
        .w(rems(22.))
        .gap_0p5();
    for (index, row) in rows.into_iter().enumerate() {
        let shortcut = shortcut_for(&row.action, default, cx)
            .map(|action| KeyBinding::for_action(action.as_ref(), cx));
        let muted = row.action == MenuAction::AgentSettings;
        let action = row.action.clone();
        let workspace = weak_workspace.clone();
        let pane = weak_pane.clone();
        let selector = format!("empty-pane-option:{}", row.label);
        list = list.child(
            div().debug_selector(move || selector).child(
                ListItem::new(("empty-pane-option", index))
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .start_slot(
                        Icon::new(row.icon)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(row.label.clone()).truncate().color(if muted {
                        Color::Muted
                    } else {
                        Color::Default
                    }))
                    .end_slot::<KeyBinding>(shortcut)
                    .on_click(move |_, window, cx| {
                        let (Some(workspace), Some(pane)) = (workspace.upgrade(), pane.upgrade())
                        else {
                            return;
                        };
                        run_action(action.clone(), None, &workspace, pane, window, cx);
                    }),
            ),
        );
        if row.separator_after {
            list = list.child(div().py_1().child(ui::Divider::horizontal()));
        }
    }
    Some(
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_6()
            .child(
                gpui::img(ui::ICARUS_MARK)
                    .debug_selector(|| "empty-pane-logo".into())
                    .size(rems(4.))
                    .flex_none(),
            )
            .child(list)
            .into_any_element(),
    )
}

impl PickerDelegate for NewTabMenu {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "icarus-new-tab-menu"
    }

    fn match_count(&self) -> usize {
        self.rows.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn separators_after_indices(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.separator_after)
            .map(|(index, _)| index)
            .collect()
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected = index;
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        PLACEHOLDER.into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query.clone();
        self.history.clear();
        self.selected = 0;
        self.update_rows();
        let Some(source) = self
            .history_source
            .clone()
            .filter(|_| !query.trim().is_empty())
        else {
            return Task::ready(());
        };
        cx.spawn(async move |picker, cx| {
            cx.background_executor().timer(HISTORY_DELAY).await;
            let Ok(search) = picker.update(cx, |_, cx| {
                (source.search)(crate::history::HistoryQuery::text(query.clone()), cx)
            }) else {
                return;
            };
            let Some(found) = search.await.log_err() else {
                return;
            };
            picker
                .update(cx, |picker, cx| {
                    if picker.delegate.query == query {
                        picker.delegate.history = found;
                        picker.delegate.update_rows();
                        cx.notify();
                    }
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let action = row.action.clone();
        cx.emit(DismissEvent);
        self.run(action, window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let row = self.rows.get(index)?;
        let muted = row.action == MenuAction::AgentSettings;
        let shortcut = self
            .shortcut(&row.action, cx)
            .map(|action| KeyBinding::for_action(action.as_ref(), cx));
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(
                    self.row_icon(row, cx)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(Label::new(row.label.clone()).truncate().color(if muted {
                    Color::Muted
                } else {
                    Color::Default
                }))
                .end_slot::<KeyBinding>(shortcut),
        )
    }
}

/// Opens an untitled Markdown buffer in `pane`.
fn open_markdown(
    workspace: &mut Workspace,
    pane: Entity<Pane>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let markdown = project.read(cx).languages().language_for_name("Markdown");
    cx.spawn_in(window, async move |workspace, cx| {
        let language = markdown.await.log_err();
        let buffer = project
            .update(cx, |project, cx| project.create_buffer(language, true, cx))
            .await?;
        workspace.update_in(cx, |workspace, window, cx| {
            let editor =
                cx.new(|cx| editor::Editor::for_buffer(buffer, Some(project.clone()), window, cx));
            workspace.add_item(pane, Box::new(editor), None, true, true, window, cx);
        })
    })
    .detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn harnesses() -> Vec<&'static Harness> {
        HARNESSES.iter().collect()
    }

    fn labels(rows: &[MenuRow]) -> Vec<&str> {
        rows.iter().map(|row| row.label.as_ref()).collect()
    }

    #[test]
    fn without_a_query_the_menu_creates_tabs_starts_agents_and_opens_their_settings() {
        let rows = menu_rows("", &harnesses(), SearchSources::default());
        assert_eq!(
            labels(&rows),
            [
                "New Terminal",
                "New Browser Tab",
                "New Markdown",
                "Claude",
                "Codex",
                "Agent settings…"
            ]
        );
        assert_eq!(rows[3].icon, IconName::AiClaude);
        assert_eq!(rows[4].icon, IconName::AiOpenAi);
        let separators: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.separator_after)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(separators, [2, 4], "agents are a group of their own");
        assert_eq!(rows[5].action, MenuAction::AgentSettings);
    }

    #[test]
    fn a_query_filters_the_rows_and_searches_open_tabs_and_files() {
        let tabs: Vec<SharedString> = vec!["login.rs".into(), "Fix the terminal colors".into()];
        let files = vec!["src/login.rs".to_string(), "src/logout.rs".to_string()];
        let rows = menu_rows(
            "term",
            &harnesses(),
            SearchSources {
                tabs: &tabs,
                files: &files,
                ..Default::default()
            },
        );
        assert_eq!(
            labels(&rows),
            [
                "Fix the terminal colors",
                "New Terminal",
                "Search the web for “term”"
            ]
        );
        assert_eq!(rows[0].action, MenuAction::OpenTab(1));

        let rows = menu_rows(
            "login",
            &harnesses(),
            SearchSources {
                tabs: &tabs,
                files: &files,
                ..Default::default()
            },
        );
        assert_eq!(rows[0].action, MenuAction::OpenTab(0));
        assert!(rows.contains(&MenuRow::new(
            MenuAction::OpenFile(0),
            "src/login.rs",
            IconName::File
        )));
        assert!(
            !rows.iter().any(|row| row.action == MenuAction::OpenFile(1)),
            "logout.rs doesn't match login"
        );
        assert!(
            !rows
                .iter()
                .any(|row| row.action == MenuAction::AgentSettings),
            "the fixed rows hide while searching"
        );
    }

    #[test]
    fn agents_match_by_name_alias_or_a_two_letter_prefix() {
        let find = |query: &str| {
            menu_rows(query, &harnesses(), SearchSources::default())
                .into_iter()
                .filter_map(|row| match row.action {
                    MenuAction::Agent(id) => Some(id),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(find("codex"), ["codex"]);
        assert_eq!(find("Claude Code"), ["claude"]);
        assert_eq!(find("openai"), ["codex"]);
        assert_eq!(find("cl"), ["claude"]);
        assert!(find("c").is_empty(), "one keystroke picks no agent");
    }

    #[test]
    fn past_agents_are_offered_with_their_harness() {
        let history = vec![("Fix the flaky test".into(), "codex".to_string())];
        let rows = menu_rows(
            "flaky",
            &harnesses(),
            SearchSources {
                history: &history,
                ..Default::default()
            },
        );
        assert_eq!(rows[0].action, MenuAction::ResumeAgent(0));
        assert_eq!(rows[0].icon, IconName::AiOpenAi);
    }

    #[gpui::test]
    async fn the_panes_plus_opens_the_menu_with_the_default_agent_first(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, _panel, mut cx) = crate::tests::panel_window(Vec::new(), cx).await;
        let opened: Rc<std::cell::RefCell<Vec<&'static str>>> = Rc::default();
        cx.update(|_, cx| {
            register(cx);
            crate::review::tests::stub_rpc(cx, |method, params| match method {
                "config.get" if params["key"] == "agents.default" => Ok(json!("codex")),
                _ => Ok(serde_json::Value::Null),
            });
            let opened = opened.clone();
            set_open_harness_handler(cx, move |harness, _, _, _| {
                opened.borrow_mut().push(harness)
            });
        });
        let pane = workspace.read_with(&cx, |workspace, _| workspace.active_pane().clone());
        workspace.update_in(&mut cx, |workspace, window, cx| {
            // A pane shows its tab bar, and so its +, once it has a tab.
            let item = cx.new(workspace::item::test::TestItem::new);
            workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
        });
        cx.update(|window, cx| {
            let focus = gpui::Focusable::focus_handle(pane.read(cx), cx);
            window.focus(&focus, cx)
        });
        cx.run_until_parked();
        assert!(
            cx.update(|_, cx| cx
                .try_global::<PaneMenus>()
                .is_some_and(|menus| menus.0.contains_key(&pane.entity_id()))),
            "the focused pane shows its +"
        );
        cx.update(|window, cx| show_menu(&pane, window, cx));
        cx.run_until_parked();
        let picker = cx
            .update(|_, cx| open_menu(&pane, cx))
            .expect("the pane's + opens its menu");
        let rows = |cx: &mut gpui::VisualTestContext| {
            picker.read_with(cx, |picker, _| {
                picker
                    .delegate
                    .rows()
                    .iter()
                    .map(|row| row.label.to_string())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            rows(&mut cx),
            [
                "New Terminal",
                "New Browser Tab",
                "New Markdown",
                "Codex",
                "Claude",
                "Agent settings…"
            ],
            "the server's default agent comes first"
        );

        picker.update_in(&mut cx, |picker, window, cx| {
            picker.set_query("claude", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(rows(&mut cx).first().map(String::as_str), Some("Claude"));
        picker.update_in(&mut cx, |picker, window, cx| {
            picker.delegate.confirm(false, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(*opened.borrow(), ["claude"]);
        assert!(
            cx.update(|_, cx| open_menu(&pane, cx)).is_none(),
            "choosing a row closes the menu"
        );
    }

    #[gpui::test]
    async fn a_pane_without_tabs_offers_the_menus_options_with_their_shortcuts(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, _panel, mut cx) = crate::tests::panel_window(Vec::new(), cx).await;
        let opened: Rc<std::cell::RefCell<Vec<&'static str>>> = Rc::default();
        cx.update(|_, cx| {
            register(cx);
            crate::review::tests::stub_rpc(cx, |method, params| match method {
                "config.get" if params["key"] == "agents.default" => Ok(json!("codex")),
                _ => Ok(serde_json::Value::Null),
            });
            let opened = opened.clone();
            set_open_harness_handler(cx, move |harness, _, _, _| {
                opened.borrow_mut().push(harness)
            });
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("empty-pane-options").is_some(),
            "a new worktree's empty pane offers what its + would"
        );
        assert_eq!(cx.update(|_, cx| default_harness(cx)), Some("codex"));
        let labels: Vec<String> = empty_pane_rows(Some("codex"))
            .iter()
            .map(|row| row.label.to_string())
            .collect();
        assert_eq!(
            labels,
            [
                "New Terminal",
                "New Browser Tab",
                "New Markdown",
                "Codex",
                "Claude",
                "Agent settings…"
            ]
        );
        for selector in [
            "empty-pane-option:New Terminal",
            "empty-pane-option:New Browser Tab",
            "empty-pane-option:New Markdown",
            "empty-pane-option:Codex",
            "empty-pane-option:Claude",
            "empty-pane-option:Agent settings…",
        ] {
            assert!(cx.debug_bounds(selector).is_some(), "{selector} is shown");
        }
        cx.update(|_, cx| {
            assert!(shortcut_for(&MenuAction::NewTerminal, Some("codex"), cx).is_some());
            assert!(shortcut_for(&MenuAction::NewMarkdown, Some("codex"), cx).is_some());
            assert!(
                shortcut_for(&MenuAction::Agent("claude"), Some("codex"), cx).is_none(),
                "only the default agent has a shortcut, as in the menu"
            );
        });

        let claude = cx
            .debug_bounds("empty-pane-option:Claude")
            .expect("claude row");
        cx.simulate_click(claude.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(*opened.borrow(), ["claude"]);

        workspace.update_in(&mut cx, |workspace, window, cx| {
            let item = cx.new(workspace::item::test::TestItem::new);
            workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("empty-pane-options").is_none(),
            "a pane with a tab shows the tab"
        );
    }

    #[test]
    fn urls_and_hosts_open_in_a_browser_but_file_names_do_not() {
        assert_eq!(
            classify_url("https://example.com/a?b").as_deref(),
            Some("https://example.com/a?b")
        );
        assert_eq!(
            classify_url("github.com/zed").as_deref(),
            Some("https://github.com/zed")
        );
        assert_eq!(
            classify_url("localhost:3000").as_deref(),
            Some("http://localhost:3000")
        );
        assert_eq!(
            classify_url("192.168.1.2:8080/x").as_deref(),
            Some("http://192.168.1.2:8080/x")
        );
        assert_eq!(classify_url("8.8.8.8").as_deref(), Some("https://8.8.8.8"));
        for not_a_url in [
            "README.md",
            "src/main.rs",
            "docker.io:latest",
            "fix the bug",
            "terminal",
            "https://",
        ] {
            assert_eq!(classify_url(not_a_url), None, "{not_a_url}");
        }
        let rows = menu_rows("example.com", &harnesses(), SearchSources::default());
        assert_eq!(
            rows[0].action,
            MenuAction::OpenUrl("https://example.com".into())
        );
    }
}
