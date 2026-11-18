//! Session history: a palette over past agent sessions (the server's full-text `agent.history`),
//! where Enter resumes one and opens it as an agent tab in its workspace.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use collections::HashMap;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, Focusable, IntoElement,
    Render, Task, WeakEntity, Window, actions,
};
use gpui_tokio::Tokio;
use picker::{Picker, PickerDelegate};
use serde::{Deserialize, Serialize};
use serde_json::json;
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::{app_client, enabled, with_workspace_window};

actions!(
    icarus,
    [
        /// Searches past agent sessions and resumes the chosen one in its workspace.
        OpenAgentHistory,
    ]
);

const LIMIT: usize = 50;
/// Typing pauses this long before the server is asked, so each keystroke doesn't search.
const SEARCH_DELAY: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct HistoryEntry {
    pub id: String,
    pub title: Option<String>,
    pub provider: String,
    pub workspace: Option<String>,
    /// The workspace's project, by name.
    pub project: Option<String>,
    pub first_prompt: Option<String>,
    pub last_message: Option<String>,
    pub created_at_ms: u64,
    pub last_activity_ms: u64,
    pub turns: u64,
    pub messages: u64,
    pub subagents: u64,
    pub model: Option<String>,
    pub state: String,
    pub resumable: bool,
    pub snippet: Option<String>,
}

/// An `agent.history` request: words to find, narrowed to a workspace or a project (id, name, or any
/// of its workspaces).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HistoryQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub limit: usize,
}

impl HistoryQuery {
    pub fn text(query: String) -> Self {
        Self {
            query: Some(query),
            limit: LIMIT,
            ..Self::default()
        }
    }
}

/// Where the palette gets sessions and how it resumes one; tests swap in a stub.
#[derive(Clone)]
pub struct HistorySource {
    pub search: Rc<dyn Fn(HistoryQuery, &mut App) -> Task<Result<Vec<HistoryEntry>>>>,
    /// Workspace id → (name, path), for row labels and for opening the right window.
    pub workspaces: Rc<dyn Fn(&mut App) -> Task<Result<HashMap<String, (String, PathBuf)>>>>,
    pub resume: Rc<dyn Fn(HistoryEntry, Option<PathBuf>, &mut App)>,
}

impl HistorySource {
    pub fn server() -> Self {
        Self {
            search: Rc::new(|query, cx| {
                let Some(client) = app_client(cx) else {
                    return Task::ready(Err(anyhow::anyhow!("not connected to the Icarus server")));
                };
                Tokio::spawn_result(cx, async move {
                    let value = client
                        .request("agent.history", serde_json::to_value(query)?)
                        .await?;
                    Ok(serde_json::from_value(value)?)
                })
            }),
            workspaces: Rc::new(|cx| {
                let Some(client) = app_client(cx) else {
                    return Task::ready(Ok(HashMap::default()));
                };
                Tokio::spawn_result(cx, async move {
                    let value = client.request("workspace.list", json!({})).await?;
                    let mut out = HashMap::default();
                    for workspace in value.as_array().into_iter().flatten() {
                        if let (Some(id), Some(path)) =
                            (workspace["id"].as_str(), workspace["path"].as_str())
                        {
                            let name = workspace["title"]
                                .as_str()
                                .or(workspace["name"].as_str())
                                .unwrap_or(id);
                            out.insert(id.to_owned(), (name.to_owned(), PathBuf::from(path)));
                        }
                    }
                    Ok(out)
                })
            }),
            resume: Rc::new(resume_on_server),
        }
    }
}

/// Wakes the session on the server, then shows it as an agent tab in its workspace's window.
fn resume_on_server(entry: HistoryEntry, path: Option<PathBuf>, cx: &mut App) {
    let Some(client) = app_client(cx) else {
        return;
    };
    let id = entry.id;
    cx.spawn(async move |cx| {
        Tokio::spawn_result(cx, {
            let id = id.clone();
            async move { client.request("agent.resume", json!({ "id": id })).await }
        })
        .await?;
        cx.update(|cx| open_resumed(id, path, cx));
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

pub(crate) fn open_resumed(agent_id: String, path: Option<PathBuf>, cx: &mut App) {
    let open =
        move |workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
            crate::open_agent_tab(Some(agent_id), None, workspace, window, cx);
        };
    match path {
        Some(path) => with_workspace_window(path, cx, open),
        None => {
            if let Some((window, workspace)) = crate::front_workspace(cx) {
                window
                    .update(cx, |_, window, cx| {
                        workspace.update(cx, |workspace, cx| open(workspace, window, cx))
                    })
                    .log_err();
            }
        }
    }
}

pub struct HistoryPalette {
    picker: Entity<Picker<HistoryDelegate>>,
}

impl HistoryPalette {
    pub fn new(source: HistorySource, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let delegate = HistoryDelegate {
            palette: cx.entity().downgrade(),
            source: source.clone(),
            entries: Vec::new(),
            workspaces: HashMap::default(),
            selected_index: 0,
            error: None,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        let names = (source.workspaces)(cx);
        let weak_picker = picker.downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let names = names.await.log_err().unwrap_or_default();
            weak_picker
                .update(cx, |picker, cx| {
                    picker.delegate.workspaces = names;
                    cx.notify();
                })
                .log_err();
        })
        .detach();
        Self { picker }
    }

    pub fn picker(&self) -> &Entity<Picker<HistoryDelegate>> {
        &self.picker
    }
}

impl EventEmitter<DismissEvent> for HistoryPalette {}
impl ModalView for HistoryPalette {}

impl Focusable for HistoryPalette {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for HistoryPalette {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("IcarusAgentHistory")
            .w(rems(40.))
            .child(self.picker.clone())
    }
}

pub struct HistoryDelegate {
    palette: WeakEntity<HistoryPalette>,
    source: HistorySource,
    entries: Vec<HistoryEntry>,
    workspaces: HashMap<String, (String, PathBuf)>,
    selected_index: usize,
    error: Option<String>,
}

impl HistoryDelegate {
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }
}

pub(crate) fn relative_time(at_ms: u64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(at_ms);
    let seconds = now_ms.saturating_sub(at_ms) / 1000;
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86_400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

pub(crate) fn one_line(text: &str, max: usize) -> String {
    let line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let cut: String = line.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

impl PickerDelegate for HistoryDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "icarus agent history"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search past agent sessions…".into()
    }

    fn match_count(&self) -> usize {
        self.entries.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = (!query.trim().is_empty()).then(|| query.trim().to_owned());
        let search = self.source.search.clone();
        let delay = query.is_some();
        cx.spawn_in(window, async move |this, cx| {
            if delay {
                cx.background_executor().timer(SEARCH_DELAY).await;
            }
            let query = HistoryQuery {
                limit: LIMIT,
                ..query.map(HistoryQuery::text).unwrap_or_default()
            };
            let Ok(found) = this.update(cx, |_, cx| search(query, cx)) else {
                return;
            };
            let found = found.await;
            this.update(cx, |picker, cx| {
                match found {
                    Ok(entries) => {
                        picker.delegate.entries = entries;
                        picker.delegate.error = None;
                    }
                    Err(error) => {
                        picker.delegate.entries.clear();
                        picker.delegate.error = Some(format!("{error:#}"));
                    }
                }
                picker.delegate.selected_index = 0;
                cx.notify();
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self.entries.get(self.selected_index).cloned() else {
            return;
        };
        let path = entry
            .workspace
            .as_ref()
            .and_then(|id| self.workspaces.get(id))
            .map(|(_, path)| path.clone());
        (self.source.resume)(entry, path, cx);
        self.palette
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.palette
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(match &self.error {
            Some(error) => error.clone().into(),
            None => "No past sessions".into(),
        })
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = self.entries.get(index)?;
        let prompt_line = entry
            .first_prompt
            .as_deref()
            .and_then(|prompt| prompt.lines().find(|line| !line.trim().is_empty()))
            .map(|line| one_line(line, usize::MAX))
            .unwrap_or_default();
        // The server titles untitled sessions after their first prompt's first line; those show the prompt once.
        let session_title = entry.title.clone().filter(|title| {
            let from_prompt = *title == prompt_line
                || title
                    .strip_suffix('…')
                    .is_some_and(|cut| prompt_line.starts_with(cut));
            !title.is_empty() && title != "Agent" && !from_prompt
        });
        let titled = session_title.is_some();
        let title = session_title
            .or_else(|| {
                entry
                    .first_prompt
                    .as_deref()
                    .map(|prompt| one_line(prompt, 70))
            })
            .unwrap_or_else(|| entry.id.clone());
        let workspace = entry
            .workspace
            .as_ref()
            .map(|id| {
                self.workspaces
                    .get(id)
                    .map(|(name, _)| name.clone())
                    .unwrap_or_else(|| id.clone())
            })
            .unwrap_or_else(|| "no workspace".into());
        let detail = format!(
            "{} · {} · {} · {} turn{}",
            entry.provider,
            workspace,
            relative_time(entry.last_activity_ms),
            entry.turns,
            if entry.turns == 1 { "" } else { "s" }
        );
        // The first prompt is already the title when the session has none of its own.
        let preview = entry
            .snippet
            .as_deref()
            .or(entry.first_prompt.as_deref().filter(|_| titled))
            .map(|text| one_line(text, 110));
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .w_full()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Label::new(title).truncate())
                                .child(div().flex_1())
                                .when(!entry.resumable, |row| {
                                    row.child(
                                        Label::new("can't resume")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Warning),
                                    )
                                }),
                        )
                        .child(
                            Label::new(detail)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        // A uniform list sizes every row like the first, so each row keeps three lines.
                        .child(
                            Label::new(preview.unwrap_or_else(|| "\u{a0}".to_owned()))
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                ),
        )
    }
}

pub fn open_history_palette(
    source: HistorySource,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.toggle_modal(window, cx, |window, cx| {
        HistoryPalette::new(source, window, cx)
    });
}

pub(crate) fn register(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, cx| {
        if !enabled(cx) {
            return;
        }
        workspace.register_action(|workspace, _: &OpenAgentHistory, window, cx| {
            open_history_palette(HistorySource::server(), workspace, window, cx);
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::panel_window;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn entry(id: &str, workspace: &str, prompt: &str) -> HistoryEntry {
        HistoryEntry {
            id: id.into(),
            provider: "claude".into(),
            workspace: Some(workspace.into()),
            first_prompt: Some(prompt.into()),
            turns: 2,
            state: "sleeping".into(),
            resumable: true,
            ..HistoryEntry::default()
        }
    }

    #[gpui::test]
    async fn history_searches_the_server_and_enter_resumes_in_the_right_workspace(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        let all = [
            entry("agent-a", "ws-1", "fix the login page"),
            entry("agent-b", "ws-2", "write the changelog"),
        ];
        let queries: Rc<RefCell<Vec<Option<String>>>> = Rc::default();
        let resumed: Rc<RefCell<Vec<(String, Option<PathBuf>)>>> = Rc::default();
        let source = HistorySource {
            search: Rc::new({
                let queries = queries.clone();
                move |query: HistoryQuery, _| {
                    let query = query.query;
                    queries.borrow_mut().push(query.clone());
                    let found = all
                        .iter()
                        .filter(|entry| {
                            query.as_deref().is_none_or(|query| {
                                entry.first_prompt.as_deref().unwrap_or("").contains(query)
                            })
                        })
                        .cloned()
                        .collect();
                    Task::ready(Ok(found))
                }
            }),
            workspaces: Rc::new(|_| {
                let mut names = HashMap::default();
                names.insert("ws-1".into(), ("login".into(), PathBuf::from("/w/login")));
                names.insert("ws-2".into(), ("docs".into(), PathBuf::from("/w/docs")));
                Task::ready(Ok(names))
            }),
            resume: Rc::new({
                let resumed = resumed.clone();
                move |entry, path, _| resumed.borrow_mut().push((entry.id, path))
            }),
        };
        workspace.update_in(&mut cx, |workspace, window, cx| {
            open_history_palette(source, workspace, window, cx);
        });
        cx.run_until_parked();
        let palette = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.active_modal::<HistoryPalette>(cx)
            })
            .expect("the history palette is open");
        let ids = |cx: &mut gpui::VisualTestContext| {
            palette.read_with(cx, |palette, cx| {
                palette
                    .picker()
                    .read(cx)
                    .delegate
                    .entries()
                    .iter()
                    .map(|entry| entry.id.clone())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(ids(&mut cx), ["agent-a", "agent-b"]);

        cx.simulate_input("changelog");
        cx.executor().advance_clock(SEARCH_DELAY * 2);
        cx.run_until_parked();
        assert_eq!(ids(&mut cx), ["agent-b"], "the server's search decides");
        assert_eq!(
            queries.borrow().last().cloned().flatten().as_deref(),
            Some("changelog")
        );

        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(
            resumed.borrow().as_slice(),
            [("agent-b".to_owned(), Some(PathBuf::from("/w/docs")))],
            "Enter resumes the session in its workspace's folder"
        );
        assert!(workspace.read_with(&cx, |workspace, cx| {
            workspace.active_modal::<HistoryPalette>(cx).is_none()
        }));
    }

    #[test]
    fn long_prompts_fold_to_one_line() {
        assert_eq!(one_line("fix\n  the   login", 40), "fix the login");
        assert_eq!(one_line("abcdefghij", 5), "abcd…");
    }
}
