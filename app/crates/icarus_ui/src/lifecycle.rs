use std::path::PathBuf;

use anyhow::Result;
use editor::Editor;
use gpui::{
    App, AsyncWindowContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    PathPromptOptions, PromptLevel, Render, Task, WeakEntity, Window, actions,
};
use serde::Deserialize;
use serde_json::{Value, json};
use ui::{CommonAnimationExt as _, ContextMenu, ContextMenuEntry, PopoverMenu, prelude::*};
use util::ResultExt as _;
use workspace::notifications::NotificationId;
use workspace::{ModalView, OpenOptions, Toast, Workspace};

use crate::routing::{self, ServerRef};
use crate::{WorkspaceSummary, WorkspacesPanel};

actions!(
    icarus,
    [
        /// Adds a git repository as an Icarus project, picked in the folder dialog.
        AddProject,
        /// Opens the New workspace dialog: name, project, start point, agent, layout and Linear issue.
        NewWorkspace,
    ]
);

pub const DEFAULT_PROVIDER: &str = "claude";
pub const PROVIDERS: [(&str, &str); 2] = [("claude", "Claude"), ("codex", "Codex")];

fn request(
    server: &ServerRef,
    method: &'static str,
    params: Value,
    cx: &mut App,
) -> Task<Result<Value>> {
    let client = routing::client_for(server, cx);
    crate::review::request(client, method, params, cx).unwrap_or_else(|| {
        Task::ready(Err(anyhow::anyhow!(
            "Not connected to the Icarus server. Start it with `icarus server start`."
        )))
    })
}

fn error_text(error: &anyhow::Error) -> String {
    crate::changes_tab::git_error_text(error)
}

fn error_code(error: &anyhow::Error) -> Option<String> {
    match error.downcast_ref::<icarus_client::ClientError>() {
        Some(icarus_client::ClientError::Rpc(rpc)) => Some(rpc.code.clone()),
        None => None,
    }
}

fn refresh_panel(workspace: &WeakEntity<Workspace>, cx: &mut App) {
    let Some(panel) = workspace
        .upgrade()
        .and_then(|workspace| workspace.read(cx).panel::<WorkspacesPanel>(cx))
    else {
        return;
    };
    panel.update(cx, |panel, cx| panel.refresh(cx));
}

pub(crate) struct LifecycleToast;

fn show_failure(workspace: &WeakEntity<Workspace>, message: String, cx: &mut App) {
    workspace
        .update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(NotificationId::unique::<LifecycleToast>(), message),
                cx,
            )
        })
        .log_err();
}

fn editor_text(editor: &Entity<Editor>, cx: &App) -> String {
    editor.read(cx).text(cx).trim().to_string()
}

fn single_line(placeholder: &str, window: &mut Window, cx: &mut App) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor
    })
}

fn render_field(label: &'static str, editor: &Entity<Editor>, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_0p5()
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
        .child(
            div()
                .px_1p5()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().editor_background)
                .child(editor.clone()),
        )
}

fn render_busy(text: &'static str) -> impl IntoElement {
    h_flex()
        .gap_1p5()
        .child(
            Icon::new(IconName::ArrowCircle)
                .size(IconSize::Small)
                .color(Color::Accent)
                .with_rotate_animation(2),
        )
        .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
}

fn render_error(error: &str) -> impl IntoElement {
    h_flex()
        .gap_1p5()
        .items_start()
        .child(
            Icon::new(IconName::Warning)
                .size(IconSize::Small)
                .color(Color::Error),
        )
        .child(
            div().flex_1().min_w_0().child(
                Label::new(error.to_owned())
                    .size(LabelSize::Small)
                    .color(Color::Error),
            ),
        )
}

fn render_fact(label: &'static str, value: String) -> impl IntoElement {
    h_flex()
        .gap_2()
        .child(
            div()
                .w(rems(7.))
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(Label::new(value).size(LabelSize::Small).truncate()),
        )
}

fn modal_frame(key_context: &'static str, title: &'static str, cx: &App) -> Div {
    v_flex()
        .key_context(key_context)
        .w(rems(34.))
        .elevation_3(cx)
        .p_3()
        .gap_2()
        .child(Label::new(title).weight(gpui::FontWeight::SEMIBOLD))
}

pub fn add_project(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let paths = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Add Project".into()),
    });
    cx.spawn_in(window, async move |workspace, cx| {
        let Some(path) = paths.await??.and_then(|paths| paths.into_iter().next()) else {
            return Ok(());
        };
        workspace.update_in(cx, |workspace, window, cx| {
            open_add_project(path, workspace, window, cx)
        })
    })
    .detach_and_log_err(cx);
}

pub fn open_add_project(
    path: PathBuf,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let weak = workspace.weak_handle();
    workspace.toggle_modal(window, cx, move |window, cx| {
        AddProjectModal::new(path, weak, window, cx)
    });
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DetectedProject {
    pub root: PathBuf,
    pub name: String,
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub base_ref: Option<String>,
    #[serde(default)]
    pub existing: Option<String>,
}

pub struct AddProjectModal {
    workspace: WeakEntity<Workspace>,
    picked: PathBuf,
    detected: Option<DetectedProject>,
    name: Entity<Editor>,
    base: Entity<Editor>,
    busy: Option<&'static str>,
    error: Option<String>,
    task: Task<()>,
}

impl AddProjectModal {
    pub fn new(
        picked: PathBuf,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = single_line("Project name", window, cx);
        let base = single_line("Base ref, e.g. origin/main", window, cx);
        window.focus(&name.focus_handle(cx), cx);
        let mut modal = Self {
            workspace,
            picked,
            detected: None,
            name,
            base,
            busy: None,
            error: None,
            task: Task::ready(()),
        };
        modal.inspect(window, cx);
        modal
    }

    pub fn detected(&self) -> Option<&DetectedProject> {
        self.detected.as_ref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn inspect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.busy = Some("Reading the repository…");
        self.error = None;
        let inspect = request(
            &ServerRef::Primary,
            "project.inspect",
            json!({ "path": self.picked }),
            cx,
        );
        self.task = cx.spawn_in(window, async move |this, cx| {
            let detected = inspect
                .await
                .and_then(|value| Ok(serde_json::from_value::<DetectedProject>(value)?));
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match detected {
                    Ok(detected) => {
                        let base = detected.base_ref.clone().unwrap_or_default();
                        this.name.update(cx, |editor, cx| {
                            editor.set_text(detected.name.clone(), window, cx)
                        });
                        this.base
                            .update(cx, |editor, cx| editor.set_text(base, window, cx));
                        this.detected = Some(detected);
                    }
                    Err(error) => this.error = Some(error_text(&error)),
                }
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(detected) = self.detected.clone() else {
            self.inspect(window, cx);
            return;
        };
        if detected.existing.is_some() {
            return;
        }
        self.busy = Some("Adding the project…");
        self.error = None;
        cx.notify();
        let add = request(
            &ServerRef::Primary,
            "project.add",
            json!({
                "path": detected.root,
                "name": editor_text(&self.name, cx),
                "base_ref": editor_text(&self.base, cx),
            }),
            cx,
        );
        let workspace = self.workspace.clone();
        self.task = cx.spawn_in(window, async move |this, cx| {
            let added = add.await;
            this.update(cx, |this, cx| match added {
                Ok(_) => {
                    refresh_panel(&workspace, cx);
                    cx.emit(DismissEvent);
                }
                Err(error) => {
                    this.busy = None;
                    this.error = Some(error_text(&error));
                    cx.notify();
                }
            })
            .log_err();
        });
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl ModalView for AddProjectModal {}

impl EventEmitter<DismissEvent> for AddProjectModal {}

impl Focusable for AddProjectModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for AddProjectModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let existing = self
            .detected
            .as_ref()
            .is_some_and(|detected| detected.existing.is_some());
        let confirm_label = match (&self.detected, &self.error) {
            (None, Some(_)) => "Try Again",
            _ => "Add Project",
        };
        modal_frame("IcarusAddProject", "Add project", cx)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .child(render_fact("Folder", self.picked.display().to_string()))
            .when_some(self.detected.as_ref(), |modal, detected| {
                modal
                    .child(render_fact(
                        "Repository",
                        detected.root.display().to_string(),
                    ))
                    .child(render_fact(
                        "Remote",
                        detected.remote.clone().unwrap_or_else(|| "none".into()),
                    ))
                    .child(render_fact(
                        "Default base",
                        detected
                            .base_ref
                            .clone()
                            .unwrap_or_else(|| "not found; set one below".into()),
                    ))
            })
            .child(render_field("Name", &self.name, cx))
            .child(render_field("Base ref", &self.base, cx))
            .when(existing, |modal| {
                modal.child(
                    Label::new("This repository is already a project.")
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .when_some(self.busy, |modal, busy| modal.child(render_busy(busy)))
            .when_some(self.error.as_deref(), |modal, error| {
                modal.child(render_error(error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(Button::new("icarus-add-project-cancel", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                    ))
                    .child(
                        Button::new("icarus-add-project-confirm", confirm_label)
                            .style(ButtonStyle::Filled)
                            .disabled(self.busy.is_some() || existing)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx)
                            })),
                    ),
            )
    }
}

pub fn remove_project(
    server: ServerRef,
    project_id: String,
    project_name: String,
    workspace_count: usize,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    // Why: removing a project cascades to its workspace records, orphaning their worktrees and agents.
    if workspace_count > 0 {
        let detail = format!(
            "It still has {workspace_count} workspace(s). Delete them first, so their worktrees and agents are cleaned up."
        );
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("Can't remove {project_name} yet"),
            Some(&detail),
            &["OK"],
            cx,
        );
        window
            .spawn(cx, async move |_| answer.await.log_err())
            .detach();
        return;
    }
    let answer = window.prompt(
        PromptLevel::Warning,
        &format!("Remove the project {project_name}?"),
        Some("Icarus forgets the project. The repository on disk is not touched."),
        &["Remove", "Cancel"],
        cx,
    );
    window
        .spawn(cx, async move |cx| {
            if answer.await? != 0 {
                return anyhow::Ok(());
            }
            let removed = cx
                .update(|_, cx| {
                    request(&server, "project.remove", json!({ "id": project_id }), cx)
                })?
                .await;
            cx.update(|_, cx| match removed {
                Ok(_) => refresh_panel(&workspace, cx),
                Err(error) => show_failure(
                    &workspace,
                    format!("Couldn't remove {project_name}: {}", error_text(&error)),
                    cx,
                ),
            })
        })
        .detach_and_log_err(cx);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPoint {
    Base,
    Branch,
    Commit,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProjectChoice {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub base_ref: Option<String>,
}

/// Opens the New workspace dialog, preselecting `project` (else the window's own project).
pub fn open_new_workspace(
    project: Option<String>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let weak = workspace.weak_handle();
    workspace.toggle_modal(window, cx, move |window, cx| {
        NewWorkspaceModal::new(weak, project, window, cx)
    });
}

pub fn window_project(workspace: &Workspace, cx: &App) -> Option<String> {
    let roots: Vec<PathBuf> = workspace
        .root_paths(cx)
        .iter()
        .map(|root| root.to_path_buf())
        .collect();
    workspace
        .panel::<WorkspacesPanel>(cx)?
        .read(cx)
        .workspace_for_roots(&roots)
        .map(|summary| summary.project_id.clone())
}

pub struct NewWorkspaceModal {
    workspace: WeakEntity<Workspace>,
    projects: Vec<ProjectChoice>,
    project: Option<String>,
    name: Entity<Editor>,
    start: StartPoint,
    start_ref: Entity<Editor>,
    provider: String,
    layouts: Vec<String>,
    layout: Option<String>,
    branch: Entity<Editor>,
    issue: Entity<Editor>,
    issue_results: Vec<IssueHit>,
    linked_issue: Option<IssueHit>,
    searching: bool,
    search_error: Option<String>,
    loading: bool,
    creating: bool,
    error: Option<String>,
    load_task: Task<()>,
    layouts_task: Task<()>,
    search_task: Task<()>,
    create_task: Task<()>,
    _issue_edits: gpui::Subscription,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct IssueHit {
    pub identifier: String,
    pub title: String,
    #[serde(default)]
    pub state: Option<IssueHitState>,
    #[serde(default)]
    pub team_key: Option<String>,
    #[serde(default)]
    pub branch_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct IssueHitState {
    pub name: String,
}

const ISSUE_SEARCH_DELAY: std::time::Duration = std::time::Duration::from_millis(250);
const ISSUE_SEARCH_LIMIT: usize = 8;

fn looks_like_identifier(text: &str) -> bool {
    let Some((team, number)) = text.split_once('-') else {
        return false;
    };
    team.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && team.chars().all(|c| c.is_ascii_alphanumeric())
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
}

impl NewWorkspaceModal {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        project: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = single_line("Name, e.g. fix-login", window, cx);
        let start_ref = single_line("origin/feature or a local branch", window, cx);
        let branch = single_line("New branch (defaults to the name)", window, cx);
        let issue = single_line("Search Linear issues by id or title", window, cx);
        let issue_edits = cx.subscribe(&issue, |this, _, event: &editor::EditorEvent, cx| {
            if matches!(event, editor::EditorEvent::BufferEdited) {
                this.search_issues(cx);
            }
        });
        window.focus(&name.focus_handle(cx), cx);
        let mut modal = Self {
            workspace,
            projects: Vec::new(),
            project,
            name,
            start: StartPoint::Base,
            start_ref,
            provider: DEFAULT_PROVIDER.into(),
            layouts: Vec::new(),
            layout: None,
            branch,
            issue,
            issue_results: Vec::new(),
            linked_issue: None,
            searching: false,
            search_error: None,
            loading: true,
            creating: false,
            error: None,
            load_task: Task::ready(()),
            layouts_task: Task::ready(()),
            search_task: Task::ready(()),
            create_task: Task::ready(()),
            _issue_edits: issue_edits,
        };
        modal.load(window, cx);
        modal
    }

    fn search_issues(&mut self, cx: &mut Context<Self>) {
        let query = editor_text(&self.issue, cx);
        self.search_error = None;
        if query.is_empty() {
            self.searching = false;
            self.issue_results.clear();
            self.search_task = Task::ready(());
            cx.notify();
            return;
        }
        self.searching = true;
        cx.notify();
        self.search_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(ISSUE_SEARCH_DELAY).await;
            let Ok(search) = this.update(cx, |_, cx| {
                request(
                    &ServerRef::Primary,
                    "issue.search",
                    json!({ "query": query, "limit": ISSUE_SEARCH_LIMIT }),
                    cx,
                )
            }) else {
                return;
            };
            let found = search
                .await
                .and_then(|value| Ok(serde_json::from_value::<Vec<IssueHit>>(value)?));
            this.update(cx, |this, cx| {
                this.searching = false;
                match found {
                    Ok(found) => this.issue_results = found,
                    Err(error) => {
                        this.issue_results.clear();
                        this.search_error = Some(error_text(&error));
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    pub fn issue_results(&self) -> &[IssueHit] {
        &self.issue_results
    }

    pub fn linked_issue(&self) -> Option<&IssueHit> {
        self.linked_issue.as_ref()
    }

    pub fn pick_issue(&mut self, issue: IssueHit, window: &mut Window, cx: &mut Context<Self>) {
        // Why: Linear's long branch names as folder names break `<slug>.localhost` URLs (63-char DNS labels).
        let name = issue.identifier.to_lowercase();
        let branch = issue.branch_name.clone().unwrap_or_default();
        self.name
            .update(cx, |editor, cx| editor.set_text(name, window, cx));
        self.branch
            .update(cx, |editor, cx| editor.set_text(branch, window, cx));
        self.linked_issue = Some(issue);
        self.issue
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.issue_results.clear();
        self.search_task = Task::ready(());
        self.searching = false;
        cx.notify();
    }

    pub fn unlink_issue(&mut self, cx: &mut Context<Self>) {
        self.linked_issue = None;
        cx.notify();
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let projects = request(&ServerRef::Primary, "project.list", json!({}), cx);
        let provider = request(
            &ServerRef::Primary,
            "config.get",
            json!({ "key": "agents.default" }),
            cx,
        );
        self.load_task = cx.spawn_in(window, async move |this, cx| {
            let projects = projects
                .await
                .and_then(|value| Ok(serde_json::from_value::<Vec<ProjectChoice>>(value)?));
            let provider = provider.await.ok();
            this.update_in(cx, |this, window, cx| {
                this.loading = false;
                if let Some(provider) = provider
                    .as_ref()
                    .and_then(Value::as_str)
                    .filter(|provider| PROVIDERS.iter().any(|(id, _)| id == provider))
                {
                    this.provider = provider.to_owned();
                }
                match projects {
                    Ok(projects) => {
                        let keep = this
                            .project
                            .as_ref()
                            .is_some_and(|id| projects.iter().any(|project| &project.id == id));
                        if !keep {
                            this.project = projects.first().map(|project| project.id.clone());
                        }
                        this.projects = projects;
                        this.load_layouts(window, cx);
                    }
                    Err(error) => this.error = Some(error_text(&error)),
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn load_layouts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
            return;
        };
        let layouts = request(
            &ServerRef::Primary,
            "layout.list",
            json!({ "project": project }),
            cx,
        );
        self.layouts_task = cx.spawn_in(window, async move |this, cx| {
            let names: Vec<String> = layouts
                .await
                .log_err()
                .and_then(|value| value.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|layout| layout["name"].as_str().map(str::to_owned))
                .collect();
            this.update(cx, |this, cx| {
                if this
                    .layout
                    .as_ref()
                    .is_some_and(|layout| !names.contains(layout))
                {
                    this.layout = None;
                }
                this.layouts = names;
                cx.notify();
            })
            .log_err();
        });
    }

    pub fn projects(&self) -> &[ProjectChoice] {
        &self.projects
    }

    pub fn selected_project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn is_creating(&self) -> bool {
        self.creating
    }

    pub fn layouts(&self) -> &[String] {
        &self.layouts
    }

    pub fn set_project(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.project = Some(id);
        self.load_layouts(window, cx);
        cx.notify();
    }

    pub fn set_start(&mut self, start: StartPoint, window: &mut Window, cx: &mut Context<Self>) {
        self.start = start;
        let placeholder = match start {
            StartPoint::Commit => "Commit SHA",
            StartPoint::Base | StartPoint::Branch => "origin/feature or a local branch",
        };
        self.start_ref.update(cx, |editor, cx| {
            editor.set_placeholder_text(placeholder, window, cx)
        });
        if start != StartPoint::Base {
            window.focus(&self.start_ref.focus_handle(cx), cx);
        }
        cx.notify();
    }

    pub fn set_provider(&mut self, provider: &str, cx: &mut Context<Self>) {
        self.provider = provider.to_owned();
        cx.notify();
    }

    pub fn set_layout(&mut self, layout: Option<String>, cx: &mut Context<Self>) {
        self.layout = layout;
        cx.notify();
    }

    pub fn create_params(&self, cx: &App) -> Result<Value, &'static str> {
        let project = self.project.clone().ok_or("Pick a project.")?;
        let name = editor_text(&self.name, cx);
        let typed_issue = editor_text(&self.issue, cx);
        let issue = match &self.linked_issue {
            Some(linked) => Some(json!({
                "provider": "linear", "id": linked.identifier, "title": linked.title,
            })),
            None if looks_like_identifier(&typed_issue) => {
                Some(json!({ "provider": "linear", "id": typed_issue }))
            }
            None => None,
        };
        if name.is_empty() && issue.is_none() {
            return Err("Name the workspace, or link a Linear issue.");
        }
        let mut params = json!({ "project": project, "provider": self.provider });
        if !name.is_empty() {
            params["name"] = json!(name);
        }
        let branch = editor_text(&self.branch, cx);
        if !branch.is_empty() {
            params["branch"] = json!(branch);
        }
        let start_ref = editor_text(&self.start_ref, cx);
        match self.start {
            StartPoint::Base => {}
            StartPoint::Branch if start_ref.is_empty() => {
                return Err("Enter the branch to start from.");
            }
            StartPoint::Commit if start_ref.is_empty() => {
                return Err("Enter the commit to start from.");
            }
            StartPoint::Branch => {
                params["base"] = json!(start_ref);
                params["fetch"] = json!(true);
            }
            StartPoint::Commit => params["base"] = json!(start_ref),
        }
        if let Some(layout) = &self.layout {
            params["layout"] = json!(layout);
        }
        if let Some(issue) = issue {
            params["issue"] = issue;
        }
        Ok(params)
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.issue.focus_handle(cx).is_focused(window)
            && let Some(first) = self.issue_results.first().cloned()
        {
            self.pick_issue(first, window, cx);
            window.focus(&self.name.focus_handle(cx), cx);
            return;
        }
        if self.creating || self.loading {
            return;
        }
        let params = match self.create_params(cx) {
            Ok(params) => params,
            Err(missing) => {
                self.error = Some(missing.to_owned());
                cx.notify();
                return;
            }
        };
        self.creating = true;
        self.error = None;
        cx.notify();
        let create = request(&ServerRef::Primary, "workspace.create", params, cx);
        let workspace = self.workspace.clone();
        self.create_task = cx.spawn_in(window, async move |this, cx| {
            let created = create.await;
            let path = match created {
                Ok(created) => created["path"].as_str().map(PathBuf::from),
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.creating = false;
                        this.error = Some(error_text(&error));
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
            };
            this.update(cx, |_, cx| {
                refresh_panel(&workspace, cx);
                cx.emit(DismissEvent);
            })
            .log_err();
            if let Some(path) = path {
                open_workspace_path(&workspace, path, cx);
            }
        });
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_issue_search(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let linked = self.linked_issue.as_ref().map(|issue| {
            h_flex()
                .gap_1p5()
                .px_1p5()
                .py_0p5()
                .rounded_md()
                .bg(cx.theme().colors().element_background)
                .child(
                    Icon::new(IconName::Link)
                        .size(IconSize::XSmall)
                        .color(Color::Accent),
                )
                .child(
                    Label::new(issue.identifier.clone())
                        .size(LabelSize::Small)
                        .weight(gpui::FontWeight::SEMIBOLD),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(issue.title.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    ),
                )
                .child(
                    IconButton::new("icarus-unlink-issue", IconName::Close)
                        .icon_size(IconSize::XSmall)
                        .tooltip(ui::Tooltip::text("Unlink the issue"))
                        .on_click(cx.listener(|this, _, _, cx| this.unlink_issue(cx))),
                )
        });
        let results = self.issue_results.iter().enumerate().map(|(index, issue)| {
            let facts = [
                issue.state.as_ref().map(|state| state.name.clone()),
                issue.team_key.clone(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
            let picked = issue.clone();
            h_flex()
                .id(("icarus-issue-hit", index))
                .gap_1p5()
                .px_1p5()
                .py_0p5()
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(cx.theme().colors().element_hover))
                .child(
                    Label::new(issue.identifier.clone())
                        .size(LabelSize::Small)
                        .weight(gpui::FontWeight::SEMIBOLD),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(issue.title.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    ),
                )
                .child(
                    Label::new(facts)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.pick_issue(picked.clone(), window, cx)
                }))
        });
        v_flex()
            .gap_0p5()
            .child(render_field("Linear issue", &self.issue, cx))
            .children(linked)
            .when(self.searching, |column| {
                column.child(render_busy("Searching Linear…"))
            })
            .when_some(self.search_error.as_deref(), |column, error| {
                column.child(render_error(error))
            })
            .children(results)
    }

    fn render_project_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let label = self
            .project
            .as_ref()
            .and_then(|id| self.projects.iter().find(|project| &project.id == id))
            .map(|project| project.name.clone())
            .unwrap_or_else(|| {
                if self.loading {
                    "Loading…".into()
                } else {
                    "No projects; add one first".into()
                }
            });
        let projects = self.projects.clone();
        let modal = cx.entity().downgrade();
        PopoverMenu::new("icarus-new-workspace-project")
            .trigger(
                Button::new("icarus-new-workspace-project-button", label)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                    .disabled(projects.is_empty()),
            )
            .menu(move |window, cx| {
                let projects = projects.clone();
                let modal = modal.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for project in &projects {
                        let modal = modal.clone();
                        let id = project.id.clone();
                        menu = menu.entry(project.name.clone(), None, move |window, cx| {
                            let id = id.clone();
                            modal
                                .update(cx, |modal, cx| modal.set_project(id, window, cx))
                                .log_err();
                        });
                    }
                    menu
                }))
            })
    }

    fn render_layout_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let label = self.layout.clone().unwrap_or_else(|| "Default".into());
        let layouts = self.layouts.clone();
        let modal = cx.entity().downgrade();
        PopoverMenu::new("icarus-new-workspace-layout")
            .trigger(
                Button::new("icarus-new-workspace-layout-button", label)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
            )
            .menu(move |window, cx| {
                let layouts = layouts.clone();
                let modal = modal.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    let default = modal.clone();
                    menu = menu.entry("Default", None, move |_, cx| {
                        default
                            .update(cx, |modal, cx| modal.set_layout(None, cx))
                            .log_err();
                    });
                    for layout in layouts
                        .iter()
                        .filter(|name| name.as_str() != icarus_protocol::layout::DEFAULT_LAYOUT)
                    {
                        let modal = modal.clone();
                        let name = layout.clone();
                        menu = menu.entry(layout.clone(), None, move |_, cx| {
                            let name = name.clone();
                            modal
                                .update(cx, |modal, cx| modal.set_layout(Some(name), cx))
                                .log_err();
                        });
                    }
                    menu
                }))
            })
    }
}

fn open_workspace_path(
    workspace: &WeakEntity<Workspace>,
    path: PathBuf,
    cx: &mut AsyncWindowContext,
) {
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace
                .open_paths(vec![path], OpenOptions::default(), None, window, cx)
                .detach();
        })
        .log_err();
}

impl ModalView for NewWorkspaceModal {}

impl EventEmitter<DismissEvent> for NewWorkspaceModal {}

impl Focusable for NewWorkspaceModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

fn row(label: &'static str, control: impl IntoElement) -> impl IntoElement {
    h_flex()
        .gap_2()
        .child(
            div()
                .w(rems(7.))
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
        )
        .child(control)
}

impl Render for NewWorkspaceModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let base_label = self
            .project
            .as_ref()
            .and_then(|id| self.projects.iter().find(|project| &project.id == id))
            .and_then(|project| project.base_ref.clone())
            .map(|base| format!("Base ({base})"))
            .unwrap_or_else(|| "Base".into());
        let start_buttons = [
            (StartPoint::Base, base_label),
            (StartPoint::Branch, "Branch".to_owned()),
            (StartPoint::Commit, "Commit".to_owned()),
        ];
        let failed = self.error.is_some() && !self.creating;
        modal_frame("IcarusNewWorkspace", "New workspace", cx)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .child(self.render_issue_search(cx))
            .child(render_field("Name", &self.name, cx))
            .child(render_field("Branch", &self.branch, cx))
            .child(row("Project", self.render_project_picker(cx)))
            .child(row(
                "Start from",
                h_flex()
                    .gap_1()
                    .children(start_buttons.into_iter().map(|(start, label)| {
                        Button::new(SharedString::from(format!("icarus-start-{start:?}")), label)
                            .style(ButtonStyle::Subtle)
                            .toggle_state(self.start == start)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_start(start, window, cx)
                            }))
                    })),
            ))
            .when(self.start != StartPoint::Base, |modal| {
                modal.child(
                    div()
                        .px_1p5()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .bg(cx.theme().colors().editor_background)
                        .child(self.start_ref.clone()),
                )
            })
            .child(row(
                "Agent",
                h_flex()
                    .gap_1()
                    .children(PROVIDERS.iter().map(|(id, label)| {
                        Button::new(SharedString::from(format!("icarus-agent-{id}")), *label)
                            .style(ButtonStyle::Subtle)
                            .toggle_state(self.provider == *id)
                            .on_click(cx.listener(move |this, _, _, cx| this.set_provider(id, cx)))
                    })),
            ))
            .child(row("Layout", self.render_layout_picker(cx)))
            .when(self.creating, |modal| {
                modal.child(render_busy("Creating the worktree and starting setup…"))
            })
            .when_some(self.error.as_deref(), |modal, error| {
                modal.child(render_error(error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("icarus-new-workspace-cancel", "Cancel").on_click(cx.listener(
                            |this, _, window, cx| this.cancel(&menu::Cancel, window, cx),
                        )),
                    )
                    .child(
                        Button::new(
                            "icarus-new-workspace-confirm",
                            if failed { "Retry" } else { "Create" },
                        )
                        .style(ButtonStyle::Filled)
                        .disabled(self.creating || self.loading || self.projects.is_empty())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.confirm(&menu::Confirm, window, cx)
                        })),
                    ),
            )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardAction {
    Rename,
    Sleep,
    Wake,
    RerunSetup,
    Delete,
}

impl CardAction {
    pub fn label(self) -> &'static str {
        match self {
            CardAction::Rename => "Rename…",
            CardAction::Sleep => "Sleep",
            CardAction::Wake => "Wake",
            CardAction::RerunSetup => "Re-run Setup",
            CardAction::Delete => "Delete…",
        }
    }
}

pub fn card_actions(summary: &WorkspaceSummary) -> Vec<CardAction> {
    vec![
        CardAction::Rename,
        if summary.sleeping {
            CardAction::Wake
        } else {
            CardAction::Sleep
        },
        CardAction::RerunSetup,
        CardAction::Delete,
    ]
}

pub fn card_menu(
    server: ServerRef,
    summary: WorkspaceSummary,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let setup_running = summary
        .setup
        .as_ref()
        .is_some_and(|setup| setup.state == "running");
    ContextMenu::build(window, cx, move |mut menu, _, _| {
        for action in card_actions(&summary) {
            if action == CardAction::Delete {
                menu = menu.separator();
            }
            let server = server.clone();
            let summary = summary.clone();
            let workspace = workspace.clone();
            menu = menu.item(
                ContextMenuEntry::new(action.label())
                    .disabled(action == CardAction::RerunSetup && setup_running)
                    .handler(move |window, cx| {
                        run_card_action(action, &server, &summary, &workspace, window, cx)
                    }),
            );
        }
        menu
    })
}

pub fn run_card_action(
    action: CardAction,
    server: &ServerRef,
    summary: &WorkspaceSummary,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let title = summary.display_title();
    let (method, failure) = match action {
        CardAction::Rename => {
            let server = server.clone();
            let summary = summary.clone();
            let weak = workspace.clone();
            // The menu that ran this may still be updating; open the dialog once it is done.
            window.defer(cx, move |window, cx| {
                weak.update(cx, |workspace, cx| {
                    let weak = workspace.weak_handle();
                    workspace.toggle_modal(window, cx, move |window, cx| {
                        RenameWorkspaceModal::new(server, summary, weak, window, cx)
                    });
                })
                .log_err();
            });
            return;
        }
        CardAction::Delete => {
            delete_workspace(server.clone(), summary, workspace.clone(), window, cx);
            return;
        }
        CardAction::Sleep => ("workspace.sleep", format!("Couldn't put {title} to sleep")),
        CardAction::Wake => ("workspace.wake", format!("Couldn't wake {title}")),
        CardAction::RerunSetup => (
            "workspace.setup",
            format!("Couldn't re-run setup for {title}"),
        ),
    };
    let task = request(server, method, json!({ "id": summary.id }), cx);
    let workspace = workspace.clone();
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            cx.update(|_, cx| match result {
                Ok(_) => refresh_panel(&workspace, cx),
                Err(error) => {
                    show_failure(&workspace, format!("{failure}: {}", error_text(&error)), cx)
                }
            })
            .log_err();
        })
        .detach();
}

const FORCEABLE: [&str; 3] = ["dirty", "unsent_review", "archive_failed"];

fn delete_workspace(
    server: ServerRef,
    summary: &WorkspaceSummary,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let title = summary.display_title();
    let id = summary.id.clone();
    let answer = window.prompt(
        PromptLevel::Warning,
        &format!("Delete {title}?"),
        Some(
            "The project's archive script runs, then the worktree folder and its branch are removed. Its agents and terminals end.",
        ),
        &["Delete", "Cancel"],
        cx,
    );
    window
        .spawn(cx, async move |cx| {
            if answer.await? != 0 {
                return anyhow::Ok(());
            }
            let removed = cx
                .update(|_, cx| request(&server, "workspace.remove", json!({ "id": id }), cx))?
                .await;
            let error = match removed {
                Ok(_) => return cx.update(|_, cx| refresh_panel(&workspace, cx)),
                Err(error) => error,
            };
            let code = error_code(&error);
            if !code
                .as_deref()
                .is_some_and(|code| FORCEABLE.contains(&code))
            {
                return cx.update(|_, cx| {
                    show_failure(
                        &workspace,
                        format!("Couldn't delete {title}: {}", error_text(&error)),
                        cx,
                    )
                });
            }
            let detail = if code.as_deref() == Some("dirty") {
                "It has uncommitted changes. Force Delete throws them away.".to_owned()
            } else {
                error_text(&error)
            };
            let answer = cx.update(|window, cx| {
                window.prompt(
                    PromptLevel::Critical,
                    &format!("{title} wasn't deleted"),
                    Some(&detail),
                    &["Force Delete", "Cancel"],
                    cx,
                )
            })?;
            if answer.await? != 0 {
                return Ok(());
            }
            let forced = cx
                .update(|_, cx| {
                    request(
                        &server,
                        "workspace.remove",
                        json!({ "id": id, "force": true }),
                        cx,
                    )
                })?
                .await;
            cx.update(|_, cx| match forced {
                Ok(_) => refresh_panel(&workspace, cx),
                Err(error) => show_failure(
                    &workspace,
                    format!("Couldn't delete {title}: {}", error_text(&error)),
                    cx,
                ),
            })
        })
        .detach_and_log_err(cx);
}

// Why: only the title changes; the folder and branch names back paths, URLs and git refs.
pub struct RenameWorkspaceModal {
    server: ServerRef,
    workspace_id: String,
    workspace: WeakEntity<Workspace>,
    title: Entity<Editor>,
    saving: bool,
    error: Option<String>,
    task: Task<()>,
}

impl RenameWorkspaceModal {
    pub fn new(
        server: ServerRef,
        summary: WorkspaceSummary,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let current = summary
            .title
            .clone()
            .unwrap_or_else(|| summary.name.clone());
        let title = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(current, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor
        });
        window.focus(&title.focus_handle(cx), cx);
        Self {
            server,
            workspace_id: summary.id,
            workspace,
            title,
            saving: false,
            error: None,
            task: Task::ready(()),
        }
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let title = editor_text(&self.title, cx);
        if self.saving || title.is_empty() {
            return;
        }
        self.saving = true;
        self.error = None;
        cx.notify();
        let set = request(
            &self.server,
            "workspace.set",
            json!({ "id": self.workspace_id, "title": title }),
            cx,
        );
        let workspace = self.workspace.clone();
        self.task = cx.spawn_in(window, async move |this, cx| {
            let result = set.await;
            this.update(cx, |this, cx| match result {
                Ok(_) => {
                    refresh_panel(&workspace, cx);
                    cx.emit(DismissEvent);
                }
                Err(error) => {
                    this.saving = false;
                    this.error = Some(error_text(&error));
                    cx.notify();
                }
            })
            .log_err();
        });
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl ModalView for RenameWorkspaceModal {}

impl EventEmitter<DismissEvent> for RenameWorkspaceModal {}

impl Focusable for RenameWorkspaceModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.title.focus_handle(cx)
    }
}

impl Render for RenameWorkspaceModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        modal_frame("IcarusRenameWorkspace", "Rename workspace", cx)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .child(render_field("Title", &self.title, cx))
            .when(self.saving, |modal| modal.child(render_busy("Saving…")))
            .when_some(self.error.as_deref(), |modal, error| {
                modal.child(render_error(error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(Button::new("icarus-rename-cancel", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                    ))
                    .child(
                        Button::new("icarus-rename-confirm", "Rename")
                            .style(ButtonStyle::Filled)
                            .disabled(self.saving)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx)
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::tests::{calls_to, stub_rpc};
    use crate::tests::{panel_window, summary};
    use gpui::VisualTestContext;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    type Calls = Arc<Mutex<Vec<(String, Value)>>>;

    fn rpc_error(code: &str, message: &str) -> anyhow::Error {
        anyhow::anyhow!(icarus_client::ClientError::Rpc(
            icarus_protocol::RpcError::new(code, message)
        ))
    }

    fn active<V: ModalView>(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Option<Entity<V>> {
        workspace.read_with(cx, |workspace, cx| workspace.active_modal::<V>(cx))
    }

    fn type_into(editor: &Entity<Editor>, text: &str, cx: &mut VisualTestContext) {
        editor.update_in(cx, |editor, window, cx| editor.set_text(text, window, cx));
    }

    fn text_of(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
        editor.read_with(cx, |editor, cx| editor.text(cx))
    }

    fn has_failure_toast(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
        workspace.read_with(cx, |workspace, _| {
            workspace.has_notification(&NotificationId::unique::<LifecycleToast>())
        })
    }

    #[gpui::test]
    async fn add_project_shows_what_was_detected_then_adds_it(cx: &mut gpui::TestAppContext) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        let cx = &mut cx;
        let calls = cx.update(|_, cx| {
            stub_rpc(cx, |method, _| match method {
                "project.inspect" => Ok(json!({
                    "root": "/code/widgets", "name": "widgets",
                    "remote": "github.com/acme/widgets", "base_ref": "origin/main", "existing": null,
                })),
                "project.add" => Ok(json!({ "id": "proj-1" })),
                other => Err(anyhow::anyhow!("unexpected {other}")),
            })
        });

        cx.dispatch_action(AddProject);
        cx.run_until_parked();
        assert!(cx.did_prompt_for_paths(), "the system folder picker opens");
        cx.simulate_path_prompt_response(|options| {
            assert!(options.directories && !options.files && !options.multiple);
            Some(vec![PathBuf::from("/code/widgets/src")])
        });
        cx.run_until_parked();

        let modal = active::<AddProjectModal>(&workspace, cx).expect("the Add project dialog");
        assert_eq!(
            calls_to(&calls, "project.inspect"),
            [json!({ "path": "/code/widgets/src" })]
        );
        let (name, base) = modal.read_with(cx, |modal, _| (modal.name.clone(), modal.base.clone()));
        assert_eq!(text_of(&name, cx), "widgets");
        assert_eq!(text_of(&base, cx), "origin/main");
        assert_eq!(
            modal.read_with(cx, |modal, _| modal
                .detected()
                .and_then(|detected| detected.remote.clone())),
            Some("github.com/acme/widgets".into())
        );

        type_into(&name, "Widgets", cx);
        type_into(&base, "origin/develop", cx);
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "project.add"),
            [json!({ "path": "/code/widgets", "name": "Widgets", "base_ref": "origin/develop" })],
            "adds the repository root, not the picked subfolder"
        );
        assert!(
            active::<AddProjectModal>(&workspace, cx).is_none(),
            "adding closes the dialog"
        );
    }

    #[gpui::test]
    async fn add_project_shows_errors_inline_and_tries_again(cx: &mut gpui::TestAppContext) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        let cx = &mut cx;
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = attempts.clone();
        let calls = cx.update(|_, cx| {
            stub_rpc(cx, move |method, _| match method {
                "project.inspect" if counted.fetch_add(1, Ordering::SeqCst) == 0 => {
                    Err(rpc_error("not_a_repo", "not a git repository: /tmp/notes"))
                }
                "project.inspect" => Ok(json!({
                    "root": "/tmp/notes", "name": "notes", "remote": null,
                    "base_ref": null, "existing": "proj-7",
                })),
                other => Err(anyhow::anyhow!("unexpected {other}")),
            })
        });
        workspace.update_in(cx, |workspace, window, cx| {
            open_add_project(PathBuf::from("/tmp/notes"), workspace, window, cx)
        });
        cx.run_until_parked();
        let modal = active::<AddProjectModal>(&workspace, cx).expect("dialog");
        let error = |cx: &mut VisualTestContext| {
            modal.read_with(cx, |modal, _| modal.error().map(str::to_owned))
        };
        assert_eq!(
            error(cx),
            Some("not a git repository: /tmp/notes".into()),
            "the server's words, without the error code"
        );

        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "confirm tries again");
        assert_eq!(error(cx), None);
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert!(
            calls_to(&calls, "project.add").is_empty(),
            "an already added repository isn't added twice"
        );
        assert!(active::<AddProjectModal>(&workspace, cx).is_some());
    }

    fn issue_node(identifier: &str, title: &str) -> Value {
        json!({
            "identifier": identifier, "title": title, "team_key": "ABC",
            "state": { "name": "Todo" },
            "branch_name": format!("frederik/{}-{}", identifier.to_lowercase(), "fix-login-redirect"),
        })
    }

    fn new_workspace_stub(cx: &mut VisualTestContext, fail_first_create: bool) -> Calls {
        let creates = AtomicUsize::new(0);
        cx.update(|_, cx| {
            stub_rpc(cx, move |method, params| match method {
                "project.list" => Ok(json!([
                    { "id": "proj-a", "name": "api", "base_ref": "origin/main" },
                    { "id": "proj-w", "name": "web", "base_ref": "origin/trunk" },
                ])),
                "config.get" => Ok(json!("codex")),
                "layout.list" => Ok(json!([{ "name": "default" }, { "name": "review" }])),
                "issue.search" if params["query"] == "nothing" => Ok(json!([])),
                "issue.search" if params["query"] == "offline" => {
                    Err(rpc_error("not_configured", "Linear is not configured"))
                }
                "issue.search" => Ok(json!([
                    issue_node("ABC-12", "Fix login redirect"),
                    issue_node("ABC-14", "Login copy"),
                ])),
                "workspace.create"
                    if fail_first_create && creates.fetch_add(1, Ordering::SeqCst) == 0 =>
                {
                    Err(rpc_error(
                        "git_failed",
                        "fatal: invalid reference: origin/nope",
                    ))
                }
                "workspace.create" => Ok(json!({ "id": "ws-new", "path": "/project" })),
                other => Err(anyhow::anyhow!("unexpected {other}")),
            })
        })
    }

    #[gpui::test]
    async fn new_workspace_creates_with_the_chosen_options_and_retries_errors(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut current = summary("current", "/project", None);
        current.project_id = "proj-w".into();
        let (workspace, _, mut cx) = panel_window(vec![current], cx).await;
        let cx = &mut cx;
        let calls = new_workspace_stub(cx, true);

        cx.dispatch_action(NewWorkspace);
        cx.run_until_parked();
        let modal = active::<NewWorkspaceModal>(&workspace, cx).expect("the New workspace dialog");
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.projects().len(), 2);
            assert_eq!(
                modal.selected_project(),
                Some("proj-w"),
                "the window's own project is preselected"
            );
            assert_eq!(modal.provider, "codex", "the agents.default setting");
            assert_eq!(modal.layouts(), ["default", "review"]);
        });
        assert_eq!(
            calls_to(&calls, "layout.list"),
            [json!({ "project": "proj-w" })]
        );

        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.error().map(str::to_owned)),
            Some("Name the workspace, or link a Linear issue.".into())
        );
        assert!(calls_to(&calls, "workspace.create").is_empty());

        let (name, start_ref) =
            modal.read_with(cx, |modal, _| (modal.name.clone(), modal.start_ref.clone()));
        type_into(&name, "Fix login", cx);
        modal.update_in(cx, |modal, window, cx| {
            modal.set_project("proj-a".into(), window, cx);
            modal.set_start(StartPoint::Branch, window, cx);
            modal.set_provider("claude", cx);
            modal.set_layout(Some("review".into()), cx);
        });
        type_into(&start_ref, "origin/nope", cx);
        modal.update_in(cx, |modal, window, cx| {
            modal.confirm(&menu::Confirm, window, cx);
            assert!(
                modal.is_creating(),
                "shows progress while the worktree is created"
            );
        });
        cx.run_until_parked();
        let expected = json!({
            "project": "proj-a", "name": "Fix login", "provider": "claude",
            "base": "origin/nope", "fetch": true, "layout": "review",
        });
        assert_eq!(
            calls_to(&calls, "workspace.create"),
            std::slice::from_ref(&expected)
        );
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.error(), Some("fatal: invalid reference: origin/nope"));
            assert!(!modal.is_creating());
        });
        assert!(
            active::<NewWorkspaceModal>(&workspace, cx).is_some(),
            "stays open to retry"
        );

        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "workspace.create"),
            [expected.clone(), expected]
        );
        assert!(
            active::<NewWorkspaceModal>(&workspace, cx).is_none(),
            "success closes the dialog"
        );
    }

    #[gpui::test]
    async fn picking_a_linear_issue_names_the_workspace_and_branch_after_it(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        let cx = &mut cx;
        let calls = new_workspace_stub(cx, false);
        workspace.update_in(cx, |workspace, window, cx| {
            open_new_workspace(None, workspace, window, cx)
        });
        cx.run_until_parked();
        let modal = active::<NewWorkspaceModal>(&workspace, cx).expect("dialog");
        let (name, branch, issue) = modal.read_with(cx, |modal, _| {
            (
                modal.name.clone(),
                modal.branch.clone(),
                modal.issue.clone(),
            )
        });

        type_into(&issue, "log", cx);
        type_into(&issue, "login", cx);
        assert!(modal.read_with(cx, |modal, _| modal.searching));
        cx.executor().advance_clock(ISSUE_SEARCH_DELAY);
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "issue.search"),
            [json!({ "query": "login", "limit": ISSUE_SEARCH_LIMIT })],
            "typing is debounced into one search"
        );
        let hits = modal.read_with(cx, |modal, _| {
            modal
                .issue_results()
                .iter()
                .map(|hit| hit.identifier.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(hits, ["ABC-12", "ABC-14"]);

        let picked = modal.read_with(cx, |modal, _| modal.issue_results()[0].clone());
        modal.update_in(cx, |modal, window, cx| modal.pick_issue(picked, window, cx));
        assert_eq!(text_of(&name, cx), "abc-12");
        assert_eq!(text_of(&branch, cx), "frederik/abc-12-fix-login-redirect");
        assert_eq!(text_of(&issue, cx), "");
        assert!(modal.read_with(cx, |modal, _| modal.issue_results().is_empty()));

        type_into(&branch, "abc-12-login", cx);
        let params = modal.read_with(cx, |modal, cx| modal.create_params(cx));
        assert_eq!(
            params,
            Ok(json!({
                "project": "proj-a", "name": "abc-12", "branch": "abc-12-login", "provider": "codex",
                "issue": { "provider": "linear", "id": "ABC-12", "title": "Fix login redirect" },
            })),
            "both stay editable"
        );

        modal.update(cx, |modal, cx| modal.unlink_issue(cx));
        type_into(&issue, "offline", cx);
        cx.executor().advance_clock(ISSUE_SEARCH_DELAY);
        cx.run_until_parked();
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.search_error.clone()),
            Some("Linear is not configured".into())
        );
        type_into(&issue, "XYZ-3", cx);
        assert_eq!(
            modal
                .read_with(cx, |modal, cx| modal.create_params(cx))
                .map(|params| params["issue"].clone()),
            Ok(json!({ "provider": "linear", "id": "XYZ-3" })),
            "a typed identifier links without picking"
        );
    }

    #[gpui::test]
    async fn enter_in_the_issue_search_picks_the_first_result(cx: &mut gpui::TestAppContext) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        let cx = &mut cx;
        let calls = new_workspace_stub(cx, false);
        cx.dispatch_action(NewWorkspace);
        cx.run_until_parked();
        let modal = active::<NewWorkspaceModal>(&workspace, cx).expect("dialog");
        let issue = modal.read_with(cx, |modal, _| modal.issue.clone());
        cx.focus(&issue);
        type_into(&issue, "login", cx);
        cx.executor().advance_clock(ISSUE_SEARCH_DELAY);
        cx.run_until_parked();
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(
            modal.read_with(cx, |modal, _| modal
                .linked_issue()
                .map(|issue| issue.identifier.clone())),
            Some("ABC-12".into())
        );
        assert!(
            calls_to(&calls, "workspace.create").is_empty(),
            "Enter picks; it doesn't create yet"
        );
    }

    #[gpui::test]
    async fn new_workspace_start_points_map_to_create_params(cx: &mut gpui::TestAppContext) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        let cx = &mut cx;
        new_workspace_stub(cx, false);
        workspace.update_in(cx, |workspace, window, cx| {
            open_new_workspace(None, workspace, window, cx)
        });
        cx.run_until_parked();
        let modal = active::<NewWorkspaceModal>(&workspace, cx).expect("dialog");
        let (name, start_ref) =
            modal.read_with(cx, |modal, _| (modal.name.clone(), modal.start_ref.clone()));
        type_into(&name, "spike", cx);
        let params =
            |cx: &mut VisualTestContext| modal.read_with(cx, |modal, cx| modal.create_params(cx));
        assert_eq!(
            params(cx),
            Ok(json!({ "project": "proj-a", "name": "spike", "provider": "codex" })),
            "the first project and its base by default"
        );
        modal.update_in(cx, |modal, window, cx| {
            modal.set_start(StartPoint::Commit, window, cx)
        });
        assert_eq!(params(cx), Err("Enter the commit to start from."));
        type_into(&start_ref, "1a2b3c4", cx);
        assert_eq!(
            params(cx),
            Ok(
                json!({ "project": "proj-a", "name": "spike", "provider": "codex", "base": "1a2b3c4" })
            ),
            "a commit is not fetched"
        );
    }

    #[gpui::test]
    async fn the_card_menu_runs_sleep_wake_and_setup_on_the_owning_server(
        cx: &mut gpui::TestAppContext,
    ) {
        let awake = summary("awake", "/w/awake", None);
        let mut asleep = summary("asleep", "/w/asleep", None);
        asleep.sleeping = true;
        assert_eq!(
            card_actions(&awake),
            [
                CardAction::Rename,
                CardAction::Sleep,
                CardAction::RerunSetup,
                CardAction::Delete
            ]
        );
        assert_eq!(card_actions(&asleep)[1], CardAction::Wake);

        let (workspace, _, mut cx) = panel_window(vec![awake.clone(), asleep.clone()], cx).await;
        let cx = &mut cx;
        let calls = cx.update(|_, cx| {
            stub_rpc(cx, |method, _| match method {
                "workspace.setup" => Err(rpc_error(
                    "no_setup_script",
                    "this project has no setup script",
                )),
                _ => Ok(json!({})),
            })
        });
        let weak = workspace.downgrade();
        let saved = ServerRef::Saved("desk".into());
        cx.update(|window, cx| {
            run_card_action(
                CardAction::Sleep,
                &ServerRef::Primary,
                &awake,
                &weak,
                window,
                cx,
            );
            run_card_action(CardAction::Wake, &saved, &asleep, &weak, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "workspace.sleep"),
            [json!({ "id": "ws-awake" })]
        );
        assert_eq!(
            calls_to(&calls, "workspace.wake"),
            [json!({ "id": "ws-asleep" })]
        );
        assert!(!has_failure_toast(&workspace, cx));

        cx.update(|window, cx| {
            run_card_action(
                CardAction::RerunSetup,
                &ServerRef::Primary,
                &awake,
                &weak,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "workspace.setup"),
            [json!({ "id": "ws-awake" })]
        );
        assert!(has_failure_toast(&workspace, cx), "a failure is shown");
    }

    #[gpui::test]
    async fn rename_sets_the_workspace_title(cx: &mut gpui::TestAppContext) {
        let mut card = summary("fix-login", "/w/fix-login", None);
        card.title = Some("Fix login".into());
        let (workspace, _, mut cx) = panel_window(vec![card.clone()], cx).await;
        let cx = &mut cx;
        let calls = cx.update(|_, cx| stub_rpc(cx, |_, _| Ok(json!({}))));
        let weak = workspace.downgrade();
        cx.update(|window, cx| {
            run_card_action(
                CardAction::Rename,
                &ServerRef::Primary,
                &card,
                &weak,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let modal = active::<RenameWorkspaceModal>(&workspace, cx).expect("the rename dialog");
        let title = modal.read_with(cx, |modal, _| modal.title.clone());
        assert_eq!(text_of(&title, cx), "Fix login");
        type_into(&title, "Fix the login page", cx);
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "workspace.set"),
            [json!({ "id": "ws-fix-login", "title": "Fix the login page" })]
        );
        assert!(active::<RenameWorkspaceModal>(&workspace, cx).is_none());
    }

    #[gpui::test]
    async fn delete_confirms_and_offers_force_for_a_dirty_checkout(cx: &mut gpui::TestAppContext) {
        let card = summary("dirty", "/w/dirty", None);
        let (workspace, _, mut cx) = panel_window(vec![card.clone()], cx).await;
        let cx = &mut cx;
        let calls = cx.update(|_, cx| {
            stub_rpc(cx, |method, params| match method {
                "workspace.remove" if params["force"] != json!(true) => {
                    Err(rpc_error("dirty", "worktree has uncommitted changes"))
                }
                _ => Ok(json!({ "removed": true })),
            })
        });
        let weak = workspace.downgrade();
        let delete = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                run_card_action(
                    CardAction::Delete,
                    &ServerRef::Primary,
                    &card,
                    &weak,
                    window,
                    cx,
                )
            });
            cx.run_until_parked();
        };

        delete(cx);
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(
            calls_to(&calls, "workspace.remove").is_empty(),
            "cancel does nothing"
        );

        delete(cx);
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "workspace.remove"),
            [json!({ "id": "ws-dirty" })]
        );
        assert!(cx.has_pending_prompt(), "the dirty refusal asks again");
        cx.simulate_prompt_answer("Force Delete");
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "workspace.remove"),
            [
                json!({ "id": "ws-dirty" }),
                json!({ "id": "ws-dirty", "force": true })
            ]
        );
        assert!(!cx.has_pending_prompt());
        assert!(!has_failure_toast(&workspace, cx));
    }

    #[gpui::test]
    async fn projects_group_with_their_workspaces_and_can_be_removed(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut api = summary("api-fix", "/w/api-fix", None);
        api.project_id = "proj-a".into();
        let (workspace, panel, mut cx) = panel_window(vec![api], cx).await;
        let cx = &mut cx;
        panel.update(cx, |panel, cx| {
            panel.set_project_names(
                [
                    ("proj-w".to_string(), "web".to_string()),
                    ("proj-a".into(), "api".into()),
                ]
                .into_iter()
                .collect(),
                cx,
            );
            panel.set_group_by(crate::cards::GroupBy::Project, cx);
        });
        let groups = panel.read_with(cx, |panel, _| {
            panel
                .project_groups()
                .into_iter()
                .map(|(id, name, workspaces)| (id, name, workspaces.len()))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            groups,
            [
                ("proj-a".to_string(), "api".to_string(), 1),
                ("proj-w".into(), "web".into(), 0)
            ],
            "an empty project still shows, so it can be removed"
        );

        let calls = cx.update(|_, cx| stub_rpc(cx, |_, _| Ok(json!({ "removed": true }))));
        let weak = workspace.downgrade();
        cx.update(|window, cx| {
            remove_project(
                ServerRef::Primary,
                "proj-a".into(),
                "api".into(),
                1,
                weak.clone(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("OK");
        cx.run_until_parked();
        assert!(
            calls_to(&calls, "project.remove").is_empty(),
            "a project with workspaces isn't removed"
        );

        cx.update(|window, cx| {
            remove_project(
                ServerRef::Primary,
                "proj-w".into(),
                "web".into(),
                0,
                weak.clone(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("Remove");
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "project.remove"),
            [json!({ "id": "proj-w" })]
        );
    }
}
