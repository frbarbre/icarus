//! The Agent Session History panel in the right sidebar (Orca's AI Vault): past agent sessions of this
//! workspace, its project or everything, searchable and sortable; picking one resumes it in a new agent tab.

use std::path::PathBuf;
use std::time::Duration;

use collections::{HashMap, HashSet};
use editor::{Editor, EditorEvent};
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, HighlightStyle, Pixels, Render, ScrollHandle, SharedString, StyledText,
    Subscription, Task, WeakEntity, Window, actions, px,
};
use ui::{ButtonLike, ContextMenu, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent, PanelSizeState};

use crate::history::{HistoryEntry, HistoryQuery, HistorySource, one_line, relative_time};

actions!(
    icarus,
    [
        /// Shows and focuses the Agent Session History panel; hides it when it already has focus.
        ToggleSessionHistory,
    ]
);

/// The panel's persistent name, which the sidebar switcher also looks it up by.
pub const SESSION_HISTORY_PANEL: &str = "IcarusSessionHistoryPanel";
const LIMIT: usize = 250;
/// Typing pauses this long before the server is asked, so each keystroke doesn't search.
const SEARCH_DELAY: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryScope {
    Workspace,
    Project,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionSort {
    LastUpdated,
    Created,
    Messages,
}

impl SessionSort {
    const ALL: [Self; 3] = [Self::LastUpdated, Self::Created, Self::Messages];

    fn label(self) -> &'static str {
        match self {
            Self::LastUpdated => "Last updated",
            Self::Created => "Created",
            Self::Messages => "Messages",
        }
    }
}

pub struct SessionHistoryPanel {
    project: WeakEntity<project::Project>,
    /// The worktree whose Icarus workspace the Workspace and Project scopes mean; else the project's.
    root: Option<PathBuf>,
    source: HistorySource,
    scope: HistoryScope,
    sort: SessionSort,
    search: Entity<Editor>,
    query: String,
    total: usize,
    entries: Vec<HistoryEntry>,
    /// Workspace id → (name, path), for group names and for opening the right window.
    workspaces: HashMap<String, (String, PathBuf)>,
    collapsed: HashSet<SharedString>,
    loading: bool,
    error: Option<SharedString>,
    fetch: Task<()>,
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
    position: DockPosition,
    active: bool,
    _search_subscription: Subscription,
}

impl SessionHistoryPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                cx.new(|cx| Self::new(HistorySource::server(), workspace, window, cx))
            })
        })
    }

    pub fn new(
        source: HistorySource,
        workspace: &Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(source, workspace.project().downgrade(), None, window, cx)
    }

    /// A panel for `root`'s workspace, shown at once; for tests and visual tests.
    pub fn preview(
        source: HistorySource,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self::build(source, WeakEntity::new_invalid(), Some(root), window, cx);
        panel.active = true;
        panel.refresh(false, cx);
        panel
    }

    fn build(
        source: HistorySource,
        project: WeakEntity<project::Project>,
        root: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search sessions", window, cx);
            editor
        });
        let search_subscription =
            cx.subscribe(&search, |panel, editor, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    let query = editor.read(cx).text(cx).trim().to_string();
                    if query != panel.query {
                        panel.query = query;
                        panel.refresh(true, cx);
                    }
                }
            });
        Self {
            project,
            root,
            source,
            scope: HistoryScope::Workspace,
            sort: SessionSort::LastUpdated,
            search,
            query: String::new(),
            total: 0,
            entries: Vec::new(),
            workspaces: HashMap::default(),
            collapsed: HashSet::default(),
            loading: false,
            error: None,
            fetch: Task::ready(()),
            focus_handle: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            position: DockPosition::Right,
            active: false,
            _search_subscription: search_subscription,
        }
    }

    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn set_scope(&mut self, scope: HistoryScope, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            self.refresh(false, cx);
        }
    }

    pub fn set_sort(&mut self, sort: SessionSort, cx: &mut Context<Self>) {
        self.sort = sort;
        sort_entries(&mut self.entries, sort);
        cx.notify();
    }

    fn root(&self, cx: &App) -> Option<PathBuf> {
        if let Some(root) = &self.root {
            return Some(root.clone());
        }
        let project = self.project.upgrade()?;
        let worktree = project.read(cx).visible_worktrees(cx).next()?;
        Some(worktree.read(cx).abs_path().to_path_buf())
    }

    pub fn refresh(&mut self, delay: bool, cx: &mut Context<Self>) {
        let source = self.source.clone();
        let scope = self.scope;
        let text = (!self.query.is_empty()).then(|| self.query.clone());
        let root = self.root(cx);
        self.loading = true;
        cx.notify();
        self.fetch = cx.spawn(async move |this, cx| {
            if delay {
                cx.background_executor().timer(SEARCH_DELAY).await;
            }
            let Ok(names) = this.update(cx, |_, cx| (source.workspaces)(cx)) else {
                return;
            };
            let names = names.await.log_err().unwrap_or_default();
            let workspace = root.as_ref().and_then(|root| {
                names
                    .iter()
                    .find(|(_, (_, path))| path == root)
                    .map(|(id, _)| id.clone())
            });
            let base = match scope {
                HistoryScope::All => Some(HistoryQuery::default()),
                HistoryScope::Workspace => workspace.map(|id| HistoryQuery {
                    workspace: Some(id),
                    ..HistoryQuery::default()
                }),
                HistoryScope::Project => workspace.map(|id| HistoryQuery {
                    project: Some(id),
                    ..HistoryQuery::default()
                }),
            }
            .map(|query| HistoryQuery {
                limit: LIMIT,
                ..query
            });
            let Some(base) = base else {
                this.update(cx, |this, cx| {
                    this.workspaces = names;
                    this.show(
                        Err(anyhow::anyhow!("This folder isn't an Icarus workspace")),
                        None,
                        cx,
                    );
                })
                .log_err();
                return;
            };
            let Ok((all, found)) = this.update(cx, |_, cx| {
                let all = (source.search)(base.clone(), cx);
                let found = text.map(|text| {
                    (source.search)(
                        HistoryQuery {
                            query: Some(text),
                            ..base
                        },
                        cx,
                    )
                });
                (all, found)
            }) else {
                return;
            };
            let all = all.await;
            let found = match found {
                Some(found) => Some(found.await),
                None => None,
            };
            this.update(cx, |this, cx| {
                this.workspaces = names;
                this.show(all, found, cx);
            })
            .log_err();
        });
    }

    fn show(
        &mut self,
        all: anyhow::Result<Vec<HistoryEntry>>,
        found: Option<anyhow::Result<Vec<HistoryEntry>>>,
        cx: &mut Context<Self>,
    ) {
        self.loading = false;
        let result = all.and_then(|all| {
            let total = all.len();
            Ok((total, found.unwrap_or(Ok(all))?))
        });
        match result {
            Ok((total, mut entries)) => {
                sort_entries(&mut entries, self.sort);
                self.total = total;
                self.entries = entries;
                self.error = None;
            }
            Err(error) => {
                self.total = 0;
                self.entries.clear();
                self.error = Some(format!("{error:#}").into());
            }
        }
        cx.notify();
    }

    pub fn resume(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        let path = entry
            .workspace
            .as_ref()
            .and_then(|id| self.workspaces.get(id))
            .map(|(_, path)| path.clone());
        (self.source.resume)(entry, path, cx);
    }

    fn group_name(&self, entry: &HistoryEntry) -> SharedString {
        entry
            .project
            .clone()
            .or_else(|| {
                entry
                    .workspace
                    .as_ref()
                    .and_then(|id| self.workspaces.get(id))
                    .map(|(name, _)| name.clone())
            })
            .unwrap_or_else(|| "Other".into())
            .into()
    }

    /// Entries by group, groups in the order of their first entry.
    fn groups(&self) -> Vec<(SharedString, Vec<usize>)> {
        let mut groups: Vec<(SharedString, Vec<usize>)> = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            let name = self.group_name(entry);
            match groups.iter_mut().find(|(group, _)| *group == name) {
                Some((_, members)) => members.push(index),
                None => groups.push((name, vec![index])),
            }
        }
        groups
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_3()
            .pt_2()
            .justify_between()
            .child(
                v_flex()
                    .child(Label::new("Agent Session History").weight(FontWeight::SEMIBOLD))
                    .child(
                        Label::new("Indexed history")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                IconButton::new("session-history-refresh", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .disabled(self.loading)
                    .tooltip(Tooltip::text("Refresh"))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(false, cx))),
            )
    }

    fn render_scope(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let current = self.scope;
        let segment = |scope: HistoryScope, label: &'static str, cx: &mut Context<Self>| {
            let selected = current == scope;
            h_flex()
                .id(label)
                .flex_1()
                .justify_center()
                .py_1()
                .rounded_sm()
                .border_1()
                .border_color(if selected {
                    colors.border
                } else {
                    gpui::transparent_black()
                })
                .when(selected, |this| this.bg(colors.element_selected))
                .when(!selected, |this| {
                    this.cursor_pointer()
                        .hover(|style| style.bg(colors.ghost_element_hover))
                })
                .child(Label::new(label).size(LabelSize::Small).color(if selected {
                    Color::Default
                } else {
                    Color::Muted
                }))
                .on_click(cx.listener(move |this, _, _, cx| this.set_scope(scope, cx)))
        };
        div().px_3().child(
            h_flex()
                .p_0p5()
                .gap_0p5()
                .rounded_md()
                .border_1()
                .border_color(colors.border_variant)
                .child(segment(HistoryScope::Workspace, "Workspace", cx))
                .child(segment(HistoryScope::Project, "Project", cx))
                .child(segment(HistoryScope::All, "All", cx)),
        )
    }

    fn render_search(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().px_3().child(
            h_flex()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().editor_background)
                .child(
                    Icon::new(IconName::MagnifyingGlass)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(div().flex_1().child(self.search.clone())),
        )
    }

    fn render_count(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.entity().downgrade();
        let sort = self.sort;
        h_flex()
            .px_3()
            .py_1()
            .justify_between()
            .border_y_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Label::new(format!("{} of {} sessions", self.entries.len(), self.total))
                    .size(LabelSize::Small)
                    .weight(FontWeight::MEDIUM),
            )
            .child(
                PopoverMenu::new("session-history-sort")
                    .anchor(gpui::Anchor::TopRight)
                    .trigger(
                        ButtonLike::new("session-history-sort-trigger")
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Label::new(sort.label()).size(LabelSize::Small))
                                    .child(
                                        Icon::new(IconName::ChevronDown)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            )
                            .tooltip(Tooltip::text("Sort sessions")),
                    )
                    .menu(move |window, cx| {
                        let weak = weak.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for option in SessionSort::ALL {
                                let weak = weak.clone();
                                menu = menu.toggleable_entry(
                                    option.label(),
                                    option == sort,
                                    IconPosition::End,
                                    None,
                                    move |_, cx| {
                                        weak.update(cx, |panel, cx| panel.set_sort(option, cx))
                                            .log_err();
                                    },
                                );
                            }
                            menu
                        }))
                    }),
            )
    }

    fn render_group_header(
        &self,
        name: SharedString,
        count: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .id(SharedString::from(format!("session-history-group-{name}")))
            .px_3()
            .py_1()
            .gap_2()
            .cursor_pointer()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::Small)
                .color(Color::Muted),
            )
            .child(
                Label::new(name.clone())
                    .size(LabelSize::Small)
                    .weight(FontWeight::MEDIUM)
                    .truncate(),
            )
            .child(div().flex_1())
            .child(
                div()
                    .px_1p5()
                    .rounded_full()
                    .bg(cx.theme().colors().element_background)
                    .child(Label::new(count.to_string()).size(LabelSize::XSmall)),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed.remove(&name) {
                    this.collapsed.insert(name.clone());
                }
                cx.notify();
            }))
    }

    fn render_entry(
        &self,
        index: usize,
        entry: &HistoryEntry,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let EntryText {
            title,
            preview,
            icon,
            harness,
            details,
        } = EntryText::new(entry);
        let mut preview_style = window.text_style();
        preview_style.color = colors.text_muted;
        let preview = preview.map(|text| {
            StyledText::new(text).with_default_highlights(
                &preview_style,
                [(
                    0.."Agent:".len(),
                    HighlightStyle {
                        color: Some(colors.text),
                        font_weight: Some(FontWeight::MEDIUM),
                        ..HighlightStyle::default()
                    },
                )],
            )
        });
        let resumable = entry.resumable;
        let menu_entry = cx.entity().downgrade();
        v_flex()
            .id(("session-history-entry", index))
            .debug_selector(move || format!("session-history-entry-{index}"))
            .px_3()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(colors.border_variant)
            .when(resumable, |this| {
                this.cursor_pointer()
                    .hover(|style| style.bg(colors.ghost_element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.resume(index, cx)))
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(title).weight(FontWeight::SEMIBOLD).truncate()),
                    )
                    .when(!resumable, |this| {
                        this.child(
                            Label::new("can't resume")
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                        )
                    })
                    .child(
                        PopoverMenu::new(("session-history-entry-menu", index))
                            .anchor(gpui::Anchor::TopRight)
                            .trigger(
                                IconButton::new(
                                    ("session-history-more", index),
                                    IconName::Ellipsis,
                                )
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("More")),
                            )
                            .menu(move |window, cx| {
                                let weak = menu_entry.clone();
                                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                                    menu.when(resumable, |menu| {
                                        menu.entry("Resume in new tab", None, move |_, cx| {
                                            weak.update(cx, |panel, cx| panel.resume(index, cx))
                                                .log_err();
                                        })
                                    })
                                    .when(!resumable, |menu| {
                                        menu.header("This session never started")
                                    })
                                }))
                            }),
                    ),
            )
            .when_some(preview, |this, preview| {
                this.child(
                    div()
                        .text_ui_sm(cx)
                        .text_color(colors.text_muted)
                        .line_clamp(2)
                        .text_ellipsis()
                        .child(preview),
                )
            })
            .child(
                h_flex()
                    .gap_1p5()
                    .child(Icon::new(icon).size(IconSize::Small))
                    .child(Label::new(harness).size(LabelSize::Small))
                    .child(
                        Label::new(details)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
    }

    fn render_list(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex()
            .id("session-history-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll);
        if let Some(error) = &self.error {
            return list.child(
                div().p_3().child(
                    Label::new(error.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            );
        }
        if self.entries.is_empty() {
            let message = if self.loading {
                "Loading sessions…"
            } else if self.query.is_empty() {
                "No past sessions"
            } else {
                "No sessions match"
            };
            return list.child(
                div().p_3().child(
                    Label::new(message)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            );
        }
        for (name, members) in self.groups() {
            let collapsed = self.collapsed.contains(&name);
            list = list.child(self.render_group_header(name, members.len(), collapsed, cx));
            if collapsed {
                continue;
            }
            for index in members {
                if let Some(entry) = self.entries.get(index) {
                    list = list.child(self.render_entry(index, entry, window, cx));
                }
            }
        }
        list
    }
}

struct EntryText {
    title: String,
    preview: Option<String>,
    icon: IconName,
    harness: SharedString,
    details: String,
}

impl EntryText {
    fn new(entry: &HistoryEntry) -> Self {
        let title = entry
            .title
            .clone()
            .or_else(|| {
                entry
                    .first_prompt
                    .as_deref()
                    .map(|prompt| one_line(prompt, 120))
            })
            .unwrap_or_else(|| entry.id.clone());
        let harness = agent_servers::harness::harness(&entry.provider);
        let plural = |count: u64| if count == 1 { "" } else { "s" };
        let mut details = vec![format!("{} msg{}", entry.messages, plural(entry.messages))];
        if entry.subagents > 0 {
            details.push(format!(
                "{} subagent{}",
                entry.subagents,
                plural(entry.subagents)
            ));
        }
        details.push(relative_time(entry.last_activity_ms));
        details.extend(entry.model.clone());
        Self {
            title,
            preview: entry
                .last_message
                .as_deref()
                .map(|message| format!("Agent: {}", one_line(message, 400))),
            icon: harness.map_or(IconName::ZedAgent, |harness| harness.icon),
            harness: harness
                .map(|harness| harness.name.into())
                .unwrap_or_else(|| entry.provider.clone().into()),
            details: details.join(" · "),
        }
    }
}

fn sort_entries(entries: &mut [HistoryEntry], sort: SessionSort) {
    entries.sort_by(|a, b| {
        let order = match sort {
            SessionSort::LastUpdated => b.last_activity_ms.cmp(&a.last_activity_ms),
            SessionSort::Created => b.created_at_ms.cmp(&a.created_at_ms),
            SessionSort::Messages => b.messages.cmp(&a.messages),
        };
        order
            .then(b.last_activity_ms.cmp(&a.last_activity_ms))
            .then(a.id.cmp(&b.id))
    });
}

impl EventEmitter<PanelEvent> for SessionHistoryPanel {}

impl Focusable for SessionHistoryPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for SessionHistoryPanel {
    fn persistent_name() -> &'static str {
        SESSION_HISTORY_PANEL
    }

    fn panel_key() -> &'static str {
        SESSION_HISTORY_PANEL
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
        Some(IconName::AgentSessionHistory)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Agent Session History")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        ToggleSessionHistory.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        9
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        let became_active = active && !self.active;
        self.active = active;
        if became_active {
            self.refresh(false, cx);
        }
    }
}

impl Render for SessionHistoryPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("IcarusSessionHistory")
            .track_focus(&self.focus_handle)
            .size_full()
            .gap_2()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .child(self.render_scope(cx))
            .child(self.render_search(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_count(cx))
                    .child(self.render_list(window, cx)),
            )
    }
}

pub(crate) fn register(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleSessionHistory, window, cx| {
            crate::github_panel::toggle_panel::<SessionHistoryPanel>(workspace, window, cx);
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn entry(id: &str, workspace: &str, title: &str, updated: u64) -> HistoryEntry {
        HistoryEntry {
            id: id.into(),
            title: Some(title.into()),
            provider: "claude".into(),
            workspace: Some(workspace.into()),
            project: Some("flowbase".into()),
            last_activity_ms: updated,
            created_at_ms: updated,
            messages: 2,
            state: "sleeping".into(),
            resumable: true,
            ..HistoryEntry::default()
        }
    }

    struct Stub {
        queries: Rc<RefCell<Vec<HistoryQuery>>>,
        resumed: Rc<RefCell<Vec<(String, Option<PathBuf>)>>>,
        source: HistorySource,
    }

    fn stub(entries: Vec<HistoryEntry>) -> Stub {
        let queries: Rc<RefCell<Vec<HistoryQuery>>> = Rc::default();
        let resumed: Rc<RefCell<Vec<(String, Option<PathBuf>)>>> = Rc::default();
        let source = HistorySource {
            search: Rc::new({
                let queries = queries.clone();
                move |query: HistoryQuery, _| {
                    queries.borrow_mut().push(query.clone());
                    let found = entries
                        .iter()
                        .filter(|entry| {
                            query
                                .workspace
                                .as_ref()
                                .is_none_or(|id| entry.workspace.as_ref() == Some(id))
                                && query.query.as_deref().is_none_or(|text| {
                                    entry.title.as_deref().unwrap_or("").contains(text)
                                })
                        })
                        .cloned()
                        .collect();
                    Task::ready(Ok(found))
                }
            }),
            workspaces: Rc::new(|_| {
                let mut names = HashMap::default();
                names.insert("ws-1".into(), ("login".into(), PathBuf::from("/project")));
                names.insert("ws-2".into(), ("docs".into(), PathBuf::from("/w/docs")));
                Task::ready(Ok(names))
            }),
            resume: Rc::new({
                let resumed = resumed.clone();
                move |entry, path, _| resumed.borrow_mut().push((entry.id, path))
            }),
        };
        Stub {
            queries,
            resumed,
            source,
        }
    }

    fn open(
        source: HistorySource,
        cx: &mut TestAppContext,
    ) -> (Entity<SessionHistoryPanel>, &mut VisualTestContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        let (panel, cx) = cx.add_window_view(|window, cx| {
            SessionHistoryPanel::preview(source, PathBuf::from("/project"), window, cx)
        });
        cx.run_until_parked();
        (panel, cx)
    }

    fn ids(panel: &Entity<SessionHistoryPanel>, cx: &mut VisualTestContext) -> Vec<String> {
        panel.read_with(cx, |panel, _| {
            panel
                .entries()
                .iter()
                .map(|entry| entry.id.clone())
                .collect()
        })
    }

    #[gpui::test]
    async fn scopes_ask_for_the_workspace_its_project_or_everything(cx: &mut TestAppContext) {
        let stub = stub(vec![
            entry("agent-a", "ws-1", "Fix login", 10),
            entry("agent-b", "ws-2", "Write docs", 20),
        ]);
        let (panel, cx) = open(stub.source.clone(), cx);
        assert_eq!(ids(&panel, cx), ["agent-a"]);
        assert_eq!(
            stub.queries.borrow().last().cloned(),
            Some(HistoryQuery {
                workspace: Some("ws-1".into()),
                limit: LIMIT,
                ..HistoryQuery::default()
            }),
            "the workspace whose worktree the window shows"
        );

        panel.update(cx, |panel, cx| panel.set_scope(HistoryScope::Project, cx));
        cx.run_until_parked();
        assert_eq!(
            stub.queries.borrow().last().cloned(),
            Some(HistoryQuery {
                project: Some("ws-1".into()),
                limit: LIMIT,
                ..HistoryQuery::default()
            })
        );

        panel.update(cx, |panel, cx| panel.set_scope(HistoryScope::All, cx));
        cx.run_until_parked();
        assert_eq!(
            stub.queries.borrow().last().cloned(),
            Some(HistoryQuery {
                limit: LIMIT,
                ..HistoryQuery::default()
            })
        );
        assert_eq!(ids(&panel, cx), ["agent-b", "agent-a"]);
        assert_eq!(panel.read_with(cx, |panel, _| panel.total()), 2);
    }

    #[gpui::test]
    async fn search_narrows_the_list_and_counts_against_the_scope(cx: &mut TestAppContext) {
        let stub = stub(vec![
            entry("agent-a", "ws-1", "Fix login", 10),
            entry("agent-b", "ws-1", "Write docs", 20),
        ]);
        let (panel, cx) = open(stub.source.clone(), cx);
        assert_eq!(ids(&panel, cx), ["agent-b", "agent-a"]);
        let search = panel.read_with(cx, |panel, _| panel.search.clone());
        search.update_in(cx, |editor, window, cx| {
            editor.set_text("login", window, cx)
        });
        cx.executor().advance_clock(SEARCH_DELAY * 2);
        cx.run_until_parked();
        assert_eq!(ids(&panel, cx), ["agent-a"]);
        assert_eq!(panel.read_with(cx, |panel, _| panel.total()), 2);
        assert!(
            stub.queries
                .borrow()
                .iter()
                .any(|query| query.query.as_deref() == Some("login")
                    && query.workspace.as_deref() == Some("ws-1")),
            "the server searches within the scope"
        );
    }

    #[gpui::test]
    async fn sort_orders_by_update_creation_or_messages(cx: &mut TestAppContext) {
        let mut old_busy = entry("agent-a", "ws-1", "Old", 10);
        old_busy.messages = 40;
        old_busy.created_at_ms = 30;
        let mut fresh = entry("agent-b", "ws-1", "Fresh", 20);
        fresh.created_at_ms = 5;
        let (panel, cx) = open(stub(vec![old_busy, fresh]).source, cx);
        assert_eq!(ids(&panel, cx), ["agent-b", "agent-a"]);
        panel.update(cx, |panel, cx| panel.set_sort(SessionSort::Created, cx));
        assert_eq!(ids(&panel, cx), ["agent-a", "agent-b"]);
        panel.update(cx, |panel, cx| panel.set_sort(SessionSort::Messages, cx));
        assert_eq!(ids(&panel, cx), ["agent-a", "agent-b"]);
        panel.update(cx, |panel, cx| panel.set_sort(SessionSort::LastUpdated, cx));
        assert_eq!(ids(&panel, cx), ["agent-b", "agent-a"]);
    }

    #[gpui::test]
    async fn clicking_an_entry_resumes_it_in_its_workspace(cx: &mut TestAppContext) {
        let mut session = entry("agent-a", "ws-1", "Fix login", 10);
        session.last_message = Some("Patched the redirect".into());
        session.subagents = 2;
        session.messages = 33;
        session.model = Some("claude-opus-5-5".into());
        let stub = stub(vec![session]);
        let (_panel, cx) = open(stub.source.clone(), cx);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let row = cx
            .debug_bounds("session-history-entry-0")
            .expect("the entry is drawn");
        cx.simulate_click(row.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            stub.resumed.borrow().as_slice(),
            [("agent-a".to_owned(), Some(PathBuf::from("/project")))]
        );
    }

    #[gpui::test]
    async fn entries_show_title_preview_counts_and_model(cx: &mut TestAppContext) {
        let mut session = entry("agent-a", "ws-1", "Fix login", 10);
        session.last_message = Some("Patched the redirect".into());
        session.subagents = 2;
        session.messages = 33;
        session.model = Some("claude-opus-5-5".into());
        session.last_activity_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or_default()
            .saturating_sub(2 * 3_600_000);
        let (panel, cx) = open(stub(vec![session]).source, cx);
        let text = panel.read_with(cx, |panel, _| panel.entries().first().map(EntryText::new));
        let text = text.expect("one entry");
        assert_eq!(text.title, "Fix login");
        assert_eq!(text.preview.as_deref(), Some("Agent: Patched the redirect"));
        assert_eq!(text.harness.as_ref(), "Claude");
        assert_eq!(
            text.details,
            "33 msgs · 2 subagents · 2h ago · claude-opus-5-5"
        );

        let mut plain = entry("agent-b", "ws-1", "Plain", 10);
        plain.title = None;
        plain.first_prompt = Some("Bump\n serde".into());
        plain.messages = 1;
        let text = EntryText::new(&plain);
        assert_eq!(text.title, "Bump serde");
        assert!(text.preview.is_none());
        assert!(
            text.details.starts_with("1 msg · ") && !text.details.contains("subagent"),
            "{}",
            text.details
        );
    }
}
