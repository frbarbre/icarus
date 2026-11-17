use std::path::{Path, PathBuf};

use anyhow::Result;
use editor::Editor;
use gpui::{
    App, AsyncWindowContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    PathPromptOptions, PromptLevel, Render, Task, WeakEntity, Window, actions,
};
use serde::Deserialize;
use serde_json::{Value, json};
use ui::{
    Checkbox, CommonAnimationExt as _, ContextMenu, ContextMenuEntry, PopoverMenu, prelude::*,
};
use util::ResultExt as _;
use workspace::notifications::NotificationId;
use workspace::{ModalView, Toast, Workspace};

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
    let server = routing::server_for_project(workspace.project().read(cx), cx);
    workspace.toggle_modal(window, cx, move |window, cx| {
        AddProjectModal::new(path, server, weak, window, cx)
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
    /// The server the project is added to; it reads the folder on its own machine.
    server: ServerRef,
    picked: PathBuf,
    /// The folder, editable so a saved server's own paths can be typed.
    folder: Entity<Editor>,
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
        server: ServerRef,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = single_line("Project name", window, cx);
        let base = single_line("Base ref, e.g. origin/main", window, cx);
        let folder = single_line("Folder on the server", window, cx);
        folder.update(cx, |editor, cx| {
            editor.set_text(picked.to_string_lossy(), window, cx)
        });
        window.focus(&name.focus_handle(cx), cx);
        let mut modal = Self {
            workspace,
            server,
            picked,
            folder,
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

    pub fn server(&self) -> &ServerRef {
        &self.server
    }

    /// Reads the folder again on another server.
    pub fn set_server(&mut self, server: ServerRef, window: &mut Window, cx: &mut Context<Self>) {
        if self.server == server {
            return;
        }
        self.server = server;
        self.detected = None;
        self.inspect(window, cx);
    }

    fn inspect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = editor_text(&self.folder, cx);
        if !typed.is_empty() {
            self.picked = PathBuf::from(typed);
        }
        self.busy = Some("Reading the repository…");
        self.error = None;
        let inspect = request(
            &self.server.clone(),
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
        let typed = editor_text(&self.folder, cx);
        let Some(detected) = self
            .detected
            .clone()
            .filter(|_| typed.is_empty() || Path::new(&typed) == self.picked)
        else {
            self.detected = None;
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
            &self.server.clone(),
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
        let several_servers = server_choices(cx).len() > 1;
        let modal = cx.entity().downgrade();
        let server_picker = render_server_picker(
            "icarus-add-project-server",
            &self.server,
            move |server, window, cx| {
                modal
                    .update(cx, |modal, cx| modal.set_server(server, window, cx))
                    .log_err();
            },
            cx,
        );
        modal_frame("IcarusAddProject", "Add project", cx)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .map(|modal| {
                if several_servers {
                    modal
                        .child(row("Server", server_picker))
                        .child(render_field("Folder", &self.folder, cx))
                } else {
                    modal.child(render_fact("Folder", self.picked.display().to_string()))
                }
            })
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
    PullRequest,
}

/// `project.scripts`: what the project's committed files run, and whether the user trusts it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ProjectScripts {
    #[serde(default)]
    pub setup: Option<ProjectScript>,
    #[serde(default)]
    pub default_tabs: Vec<DefaultTabScript>,
    #[serde(default = "trusted_by_default")]
    pub default_tabs_trusted: bool,
    #[serde(default)]
    pub run_policy: String,
    /// Runs for a workspace started from a pull request (Orca's `issueCommand`).
    #[serde(default)]
    pub issue_command: Option<ProjectScript>,
}

fn trusted_by_default() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProjectScript {
    pub command: String,
    pub trusted: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DefaultTabScript {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
}

impl ProjectScripts {
    fn asks_for_setup(&self) -> bool {
        self.setup.is_some() && self.run_policy == "ask"
    }

    fn setup_runs(&self, run_setup: bool) -> bool {
        self.setup.is_some()
            && match self.run_policy.as_str() {
                "ask" => run_setup,
                "skip-by-default" => false,
                _ => true,
            }
    }

    fn untrusted_tab_commands(&self) -> Vec<&str> {
        if self.default_tabs_trusted {
            return Vec::new();
        }
        self.default_tabs
            .iter()
            .filter_map(|tab| tab.command.as_deref())
            .collect()
    }
}

fn next_create_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "app-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
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
    let server = routing::server_for_project(workspace.project().read(cx), cx);
    workspace.toggle_modal(window, cx, move |window, cx| {
        NewWorkspaceModal::new(weak, server, project, window, cx)
    });
}

/// The servers a dialog can create on: the app's own, then each saved server it doesn't already reach.
pub fn server_choices(cx: &App) -> Vec<(ServerRef, String)> {
    let primary = routing::primary(cx);
    let mut choices = vec![(
        ServerRef::Primary,
        primary.clone().unwrap_or_else(|| "This machine".to_owned()),
    )];
    if let Some(store) = crate::servers::existing_store(cx) {
        for server in store.read(cx).servers() {
            if Some(&server.name) != primary.as_ref() {
                choices.push((ServerRef::Saved(server.name.clone()), server.name.clone()));
            }
        }
    }
    choices
}

fn server_label(server: &ServerRef, cx: &App) -> String {
    server_choices(cx)
        .into_iter()
        .find(|(choice, _)| choice == server)
        .map(|(_, label)| label)
        .unwrap_or_else(|| server.saved_name().unwrap_or("This machine").to_owned())
}

/// A server picker button for the dialogs; `pick` runs with the chosen server.
fn render_server_picker(
    id: &'static str,
    server: &ServerRef,
    pick: impl Fn(ServerRef, &mut Window, &mut App) + Clone + 'static,
    cx: &App,
) -> gpui::AnyElement {
    let choices = server_choices(cx);
    let current = server.clone();
    PopoverMenu::new(id)
        .trigger(
            Button::new(
                SharedString::from(format!("{id}-button")),
                server_label(server, cx),
            )
            .start_icon(Icon::new(IconName::Server).size(IconSize::XSmall))
            .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
            .disabled(choices.len() < 2),
        )
        .menu(move |window, cx| {
            let choices = choices.clone();
            let current = current.clone();
            let pick = pick.clone();
            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                for (choice, label) in &choices {
                    let pick = pick.clone();
                    let choice = choice.clone();
                    menu = menu.toggleable_entry(
                        label.clone(),
                        choice == current,
                        IconPosition::Start,
                        None,
                        move |window, cx| pick(choice.clone(), window, cx),
                    );
                }
                menu
            }))
        })
        .into_any_element()
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
    /// The server the workspace is created on; its projects fill the picker.
    server: ServerRef,
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
    linear_unconfigured: bool,
    loading: bool,
    creating: bool,
    error: Option<String>,
    notice: Option<&'static str>,
    scripts: Option<ProjectScripts>,
    /// The choice for a project whose setup policy is `ask`.
    run_setup: bool,
    trust_prompt: bool,
    /// Orca's "Always trust" in the trust prompt.
    always_trust: bool,
    create_id: Option<String>,
    cancelling: bool,
    load_task: Task<()>,
    layouts_task: Task<()>,
    scripts_task: Task<()>,
    cancel_task: Task<()>,
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
        server: ServerRef,
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
            server,
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
            linear_unconfigured: false,
            loading: true,
            creating: false,
            error: None,
            notice: None,
            scripts: None,
            run_setup: true,
            trust_prompt: false,
            always_trust: false,
            create_id: None,
            cancelling: false,
            load_task: Task::ready(()),
            layouts_task: Task::ready(()),
            scripts_task: Task::ready(()),
            cancel_task: Task::ready(()),
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
            let Ok(search) = this.update(cx, |this, cx| {
                request(
                    &this.server.clone(),
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
                    Ok(found) => {
                        this.issue_results = found;
                        this.linear_unconfigured = false;
                    }
                    Err(error) => {
                        this.issue_results.clear();
                        this.linear_unconfigured =
                            error_code(&error).as_deref() == Some("not_configured");
                        if !this.linear_unconfigured {
                            this.search_error = Some(error_text(&error));
                        }
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

    fn connect_linear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    crate::settings_view::open_linear_settings(workspace, window, cx)
                })
                .log_err();
        });
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
        let server = self.server.clone();
        let projects = request(&server, "project.list", json!({}), cx);
        let provider = request(
            &server,
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
                        this.load_scripts(window, cx);
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
            &self.server.clone(),
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

    pub fn server(&self) -> &ServerRef {
        &self.server
    }

    /// Switches to another server: its projects, layouts and scripts load, and the linked issue is dropped.
    pub fn set_server(&mut self, server: ServerRef, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating || self.server == server {
            return;
        }
        self.server = server;
        self.projects.clear();
        self.project = None;
        self.layouts.clear();
        self.layout = None;
        self.scripts = None;
        self.trust_prompt = false;
        self.linked_issue = None;
        self.issue_results.clear();
        self.error = None;
        self.loading = true;
        self.load(window, cx);
        cx.notify();
    }

    pub fn set_project(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.project = Some(id);
        self.trust_prompt = false;
        self.load_layouts(window, cx);
        self.load_scripts(window, cx);
        cx.notify();
    }

    fn load_scripts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.scripts = None;
        let Some(project) = self.project.clone() else {
            return;
        };
        let scripts = request(
            &self.server.clone(),
            "project.scripts",
            json!({ "id": project }),
            cx,
        );
        self.scripts_task = cx.spawn_in(window, async move |this, cx| {
            let scripts = scripts
                .await
                .and_then(|value| Ok(serde_json::from_value::<ProjectScripts>(value)?))
                .log_err();
            this.update(cx, |this, cx| {
                if this.project.as_deref() == Some(project.as_str()) {
                    this.scripts = scripts;
                    cx.notify();
                }
            })
            .log_err();
        });
    }

    pub fn scripts(&self) -> Option<&ProjectScripts> {
        self.scripts.as_ref()
    }

    pub fn is_trust_prompt_open(&self) -> bool {
        self.trust_prompt
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice
    }

    pub fn set_run_setup(&mut self, run_setup: bool, cx: &mut Context<Self>) {
        self.run_setup = run_setup;
        cx.notify();
    }

    /// The committed scripts this create would run that the user hasn't trusted yet.
    fn untrusted_scripts(&self) -> Vec<String> {
        let Some(scripts) = &self.scripts else {
            return Vec::new();
        };
        let mut untrusted = Vec::new();
        if let Some(setup) = &scripts.setup
            && !setup.trusted
            && scripts.setup_runs(self.run_setup)
        {
            untrusted.push(setup.command.clone());
        }
        untrusted.extend(
            scripts
                .untrusted_tab_commands()
                .into_iter()
                .map(str::to_owned),
        );
        if let Some(issue_command) = &scripts.issue_command
            && !issue_command.trusted
            && self.start == StartPoint::PullRequest
        {
            untrusted.push(issue_command.command.clone());
        }
        untrusted
    }

    pub fn set_start(&mut self, start: StartPoint, window: &mut Window, cx: &mut Context<Self>) {
        self.start = start;
        let placeholder = match start {
            StartPoint::Commit => "Commit SHA",
            StartPoint::PullRequest => "Pull request number or URL",
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
        let start_ref = editor_text(&self.start_ref, cx);
        if name.is_empty() && issue.is_none() && self.start != StartPoint::PullRequest {
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
        match self.start {
            StartPoint::Base => {}
            StartPoint::Branch if start_ref.is_empty() => {
                return Err("Enter the branch to start from.");
            }
            StartPoint::Commit if start_ref.is_empty() => {
                return Err("Enter the commit to start from.");
            }
            StartPoint::PullRequest if start_ref.is_empty() => {
                return Err("Enter the pull request's number or URL.");
            }
            StartPoint::Branch => {
                params["base"] = json!(start_ref);
                params["fetch"] = json!(true);
            }
            StartPoint::Commit => params["base"] = json!(start_ref),
            StartPoint::PullRequest => params["pr"] = json!(start_ref),
        }
        if let Some(layout) = &self.layout {
            params["layout"] = json!(layout);
        }
        if self
            .scripts
            .as_ref()
            .is_some_and(ProjectScripts::asks_for_setup)
        {
            params["setup"] = json!(if self.run_setup { "run" } else { "skip" });
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
        // A trust prompt is answered with its buttons, never by a stray Enter.
        if self.creating || self.loading || self.trust_prompt {
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
        if !self.untrusted_scripts().is_empty() {
            self.trust_prompt = true;
            self.always_trust = false;
            self.error = None;
            cx.notify();
            return;
        }
        self.start_create(params, window, cx);
    }

    /// The trust prompt's "Trust and Create".
    pub fn trust_and_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating {
            return;
        }
        let Ok(mut params) = self.create_params(cx) else {
            return;
        };
        params["trust"] = if self.always_trust {
            json!("always")
        } else {
            json!(true)
        };
        self.trust_prompt = false;
        self.start_create(params, window, cx);
    }

    pub fn set_always_trust(&mut self, always: bool, cx: &mut Context<Self>) {
        self.always_trust = always;
        cx.notify();
    }

    /// The trust prompt's "Create without running them": setup is skipped and default tabs open as plain shells.
    pub fn create_without_scripts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating {
            return;
        }
        let Ok(mut params) = self.create_params(cx) else {
            return;
        };
        params["setup"] = json!("skip");
        self.trust_prompt = false;
        self.start_create(params, window, cx);
    }

    pub fn close_trust_prompt(&mut self, cx: &mut Context<Self>) {
        self.trust_prompt = false;
        cx.notify();
    }

    fn start_create(&mut self, mut params: Value, window: &mut Window, cx: &mut Context<Self>) {
        let create_id = next_create_id();
        params["create_id"] = json!(create_id);
        self.create_id = Some(create_id);
        self.creating = true;
        self.cancelling = false;
        self.error = None;
        self.notice = None;
        cx.notify();
        let server = self.server.clone();
        let create = request(&server, "workspace.create", params, cx);
        let workspace = self.workspace.clone();
        self.create_task = cx.spawn_in(window, async move |this, cx| {
            let created = create.await;
            let path = match created {
                Ok(created) => created["path"].as_str().map(PathBuf::from),
                Err(error) => {
                    this.update_in(cx, |this, window, cx| {
                        this.creating = false;
                        this.cancelling = false;
                        this.create_id = None;
                        match error_code(&error).as_deref() {
                            Some("cancelled") => {
                                this.notice = Some("Cancelled; nothing was created.");
                            }
                            // The scripts changed since the dialog read them: ask again.
                            Some("untrusted_scripts") => {
                                this.load_scripts(window, cx);
                                this.error = Some(error_text(&error));
                            }
                            _ => this.error = Some(error_text(&error)),
                        }
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
                open_created_workspace(&server, &workspace, path, cx);
            }
        });
    }

    /// Asks the server to stop the create in flight; it answers the create with `cancelled` once it has cleaned up.
    pub fn cancel_create(&mut self, cx: &mut Context<Self>) {
        let Some(create_id) = self.create_id.clone().filter(|_| self.creating) else {
            return;
        };
        if self.cancelling {
            return;
        }
        self.cancelling = true;
        cx.notify();
        let cancel = request(
            &self.server.clone(),
            "workspace.create_cancel",
            json!({ "create_id": create_id }),
            cx,
        );
        self.cancel_task = cx.spawn(async move |this, cx| {
            if let Err(error) = cancel.await {
                this.update(cx, |this, cx| {
                    this.cancelling = false;
                    this.error = Some(error_text(&error));
                    cx.notify();
                })
                .log_err();
            }
        });
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        if self.creating {
            self.cancel_create(cx);
        } else if self.trust_prompt {
            self.close_trust_prompt(cx);
        } else {
            cx.emit(DismissEvent);
        }
    }

    fn render_trust_prompt(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let project = self
            .project
            .as_ref()
            .and_then(|id| self.projects.iter().find(|project| &project.id == id))
            .map(|project| project.name.clone())
            .unwrap_or_else(|| "this project".into());
        let scripts = self.untrusted_scripts().join("\n");
        let setup_untrusted = self
            .scripts
            .as_ref()
            .and_then(|scripts| scripts.setup.as_ref())
            .is_some_and(|setup| !setup.trusted);
        v_flex()
            .gap_1p5()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().status().warning_border)
            .bg(cx.theme().status().warning_background)
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .color(Color::Warning),
                    )
                    .child(
                        Label::new(format!("Run scripts from {project}?"))
                            .size(LabelSize::Small)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    ),
            )
            .child(
                Label::new(format!(
                    "{project}'s committed orca.yaml (or icarus.json) runs these on your machine when the workspace is created. Only run them if you trust {project}; you're asked again when they change."
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                div()
                    .px_1p5()
                    .py_1()
                    .rounded_sm()
                    .bg(cx.theme().colors().editor_background)
                    .font_buffer(cx)
                    .text_xs()
                    .max_h(rems(10.))
                    .overflow_hidden()
                    .child(scripts),
            )
            .child(
                Checkbox::new(
                    "icarus-trust-always",
                    if self.always_trust {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .label(format!("Always trust scripts from {project}"))
                .label_size(LabelSize::Small)
                .on_click(cx.listener(|this, state: &ToggleState, _, cx| {
                    this.set_always_trust(*state == ToggleState::Selected, cx)
                })),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("icarus-trust-back", "Back").on_click(
                            cx.listener(|this, _, _, cx| this.close_trust_prompt(cx)),
                        ),
                    )
                    .child(
                        Button::new(
                            "icarus-trust-skip",
                            if setup_untrusted {
                                "Create without running them"
                            } else {
                                "Create without the tab commands"
                            },
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.create_without_scripts(window, cx)
                        })),
                    )
                    .child(
                        Button::new("icarus-trust-run", "Trust and Create")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.trust_and_create(window, cx)
                            })),
                    ),
            )
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
            .when(self.linear_unconfigured, |column| {
                column.child(
                    h_flex()
                        .gap_2()
                        .justify_between()
                        .child(
                            Label::new("Connect Linear to search your issues.")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Button::new("icarus-connect-linear", "Connect Linear…")
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.connect_linear(window, cx)
                                })),
                        ),
                )
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

/// Opens a workspace the dialog created: a saved server's opens the way its cards do (over SSH, or the
/// folder when it is on this machine too).
fn open_created_workspace(
    server: &ServerRef,
    workspace: &WeakEntity<Workspace>,
    path: PathBuf,
    cx: &mut AsyncWindowContext,
) {
    let ServerRef::Saved(name) = server else {
        cx.update(|_, cx| crate::show_workspace(path, cx)).log_err();
        return;
    };
    cx.update(|_, cx| {
        let Some(panel) = workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).panel::<WorkspacesPanel>(cx))
        else {
            return;
        };
        let ssh = crate::servers::existing_store(cx).and_then(|store| {
            store
                .read(cx)
                .servers()
                .iter()
                .find(|saved| &saved.name == name)
                .and_then(|saved| saved.ssh_destination())
        });
        crate::open_remote_workspace(name, ssh, path, panel.downgrade(), cx);
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
            (StartPoint::PullRequest, "Pull request".to_owned()),
        ];
        let failed = self.error.is_some() && !self.creating;
        let asks_for_setup = self
            .scripts
            .as_ref()
            .is_some_and(ProjectScripts::asks_for_setup);
        let several_servers = server_choices(cx).len() > 1;
        let modal = cx.entity().downgrade();
        let server_picker = render_server_picker(
            "icarus-new-workspace-server",
            &self.server,
            move |server, window, cx| {
                modal
                    .update(cx, |modal, cx| modal.set_server(server, window, cx))
                    .log_err();
            },
            cx,
        );
        modal_frame("IcarusNewWorkspace", "New workspace", cx)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .child(self.render_issue_search(cx))
            .child(render_field("Name", &self.name, cx))
            .child(render_field("Branch", &self.branch, cx))
            .when(several_servers, |modal| {
                modal.child(row("Server", server_picker))
            })
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
            .when(asks_for_setup, |modal| {
                modal.child(row(
                    "Setup script",
                    h_flex().gap_1().children(
                        [(true, "Run setup"), (false, "Skip")]
                            .into_iter()
                            .map(|(run, label)| {
                                Button::new(
                                    SharedString::from(format!("icarus-setup-{run}")),
                                    label,
                                )
                                .style(ButtonStyle::Subtle)
                                .toggle_state(self.run_setup == run)
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.set_run_setup(run, cx)),
                                )
                            }),
                    ),
                ))
            })
            .when(self.creating, |modal| {
                modal.child(render_busy(if self.cancelling {
                    "Cancelling and cleaning up…"
                } else {
                    "Creating the worktree and starting setup…"
                }))
            })
            .when_some(self.error.as_deref(), |modal, error| {
                modal.child(render_error(error))
            })
            .when_some(self.notice, |modal, notice| {
                modal.child(
                    Label::new(notice)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when(self.trust_prompt, |modal| {
                modal.child(self.render_trust_prompt(cx))
            })
            .when(!self.trust_prompt, |modal| {
                modal.child(
                    h_flex()
                        .justify_end()
                        .gap_1()
                        .child(
                            Button::new(
                                "icarus-new-workspace-cancel",
                                if self.creating {
                                    "Cancel Create"
                                } else {
                                    "Cancel"
                                },
                            )
                            .disabled(self.cancelling)
                            .on_click(cx.listener(
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
                            .on_click(cx.listener(
                                |this, _, window, cx| this.confirm(&menu::Confirm, window, cx),
                            )),
                        ),
                )
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardAction {
    Pin,
    Unpin,
    Rename,
    Sleep,
    Wake,
    RerunSetup,
    Delete,
}

impl CardAction {
    pub fn label(self) -> &'static str {
        match self {
            CardAction::Pin => "Pin",
            CardAction::Unpin => "Unpin",
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
        if summary.pinned {
            CardAction::Unpin
        } else {
            CardAction::Pin
        },
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
    origin: Option<String>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let setup_running = summary
        .setup
        .as_ref()
        .is_some_and(|setup| setup.state == "running");
    ContextMenu::build(window, cx, move |mut menu, _, _| {
        if let Some(origin) = origin {
            menu = menu.header(origin);
        }
        for action in card_actions(&summary) {
            if matches!(action, CardAction::Rename | CardAction::Delete) {
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
    let mut params = json!({ "id": summary.id });
    let (method, failure) = match action {
        CardAction::Pin | CardAction::Unpin => {
            let pinned = action == CardAction::Pin;
            params["pinned"] = json!(pinned);
            let verb = if pinned { "pin" } else { "unpin" };
            ("workspace.set", format!("Couldn't {verb} {title}"))
        }
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
    let task = request(server, method, params, cx);
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

/// What a right-click on a multi-selection offers: pin or unpin, sleep, wake and delete, each on every
/// selected workspace's own server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkAction {
    Pin,
    Unpin,
    Sleep,
    Wake,
    Delete,
}

/// The bulk actions with their labels and the workspaces each applies to.
pub fn bulk_actions(
    targets: &[(ServerRef, WorkspaceSummary)],
) -> Vec<(BulkAction, String, Vec<(ServerRef, WorkspaceSummary)>)> {
    let pick = |keep: &dyn Fn(&WorkspaceSummary) -> bool| -> Vec<(ServerRef, WorkspaceSummary)> {
        targets
            .iter()
            .filter(|(_, workspace)| keep(workspace))
            .cloned()
            .collect()
    };
    let unpinned = pick(&|workspace| !workspace.pinned);
    let mut actions = Vec::new();
    if unpinned.is_empty() {
        actions.push((
            BulkAction::Unpin,
            crate::cards::count_label("Unpin", targets.len()),
            targets.to_vec(),
        ));
    } else {
        actions.push((
            BulkAction::Pin,
            crate::cards::count_label("Pin", unpinned.len()),
            unpinned,
        ));
    }
    let awake = pick(&|workspace| !workspace.sleeping);
    if !awake.is_empty() {
        actions.push((
            BulkAction::Sleep,
            crate::cards::count_label("Sleep", awake.len()),
            awake,
        ));
    }
    let asleep = pick(&|workspace| workspace.sleeping);
    if !asleep.is_empty() {
        actions.push((
            BulkAction::Wake,
            crate::cards::count_label("Wake", asleep.len()),
            asleep,
        ));
    }
    actions.push((
        BulkAction::Delete,
        format!("{}…", crate::cards::count_label("Delete", targets.len())),
        targets.to_vec(),
    ));
    actions
}

pub fn bulk_card_menu(
    targets: Vec<(ServerRef, WorkspaceSummary)>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |mut menu, _, _| {
        menu = menu.header(format!("{} workspaces selected", targets.len()));
        for (action, label, applies_to) in bulk_actions(&targets) {
            if action == BulkAction::Delete {
                menu = menu.separator();
            }
            let workspace = workspace.clone();
            menu = menu.entry(label, None, move |window, cx| {
                run_bulk_action(action, applies_to.clone(), workspace.clone(), window, cx)
            });
        }
        menu
    })
}

pub fn run_bulk_action(
    action: BulkAction,
    targets: Vec<(ServerRef, WorkspaceSummary)>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let (method, verb) = match action {
        BulkAction::Pin => ("workspace.set", "pin"),
        BulkAction::Unpin => ("workspace.set", "unpin"),
        BulkAction::Sleep => ("workspace.sleep", "put to sleep"),
        BulkAction::Wake => ("workspace.wake", "wake"),
        BulkAction::Delete => {
            delete_workspaces(targets, workspace, window, cx);
            return;
        }
    };
    let tasks: Vec<(String, Task<Result<Value>>)> = targets
        .iter()
        .map(|(server, summary)| {
            let mut params = json!({ "id": summary.id });
            if matches!(action, BulkAction::Pin | BulkAction::Unpin) {
                params["pinned"] = json!(action == BulkAction::Pin);
            }
            (summary.display_title(), request(server, method, params, cx))
        })
        .collect();
    window
        .spawn(cx, async move |cx| {
            let mut failures = Vec::new();
            for (title, task) in tasks {
                if let Err(error) = task.await {
                    failures.push(format!("{title}: {}", error_text(&error)));
                }
            }
            cx.update(|_, cx| {
                refresh_panel(&workspace, cx);
                if !failures.is_empty() {
                    show_failure(
                        &workspace,
                        format!("Couldn't {verb} {}", failures.join("; ")),
                        cx,
                    );
                }
            })
            .log_err();
        })
        .detach();
}

/// Deletes several workspaces after one confirmation (Orca always confirms a bulk delete). Those refused
/// for local changes, unsent review comments or a failed archive script are offered one Force Delete.
fn delete_workspaces(
    targets: Vec<(ServerRef, WorkspaceSummary)>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let count = targets.len();
    let answer = window.prompt(
        PromptLevel::Warning,
        &format!("Delete {count} workspaces?"),
        Some(
            "Each project's archive script runs, then the worktree folders and their branches are removed. Their agents and terminals end.",
        ),
        &[&crate::cards::count_label("Delete", count), "Cancel"],
        cx,
    );
    window
        .spawn(cx, async move |cx| {
            if answer.await? != 0 {
                return anyhow::Ok(());
            }
            let mut forceable = Vec::new();
            let mut failures = Vec::new();
            for (server, summary) in targets {
                let removed = cx
                    .update(|_, cx| {
                        request(&server, "workspace.remove", json!({ "id": summary.id }), cx)
                    })?
                    .await;
                let Err(error) = removed else {
                    continue;
                };
                let line = format!("{}: {}", summary.display_title(), error_text(&error));
                if error_code(&error)
                    .as_deref()
                    .is_some_and(|code| FORCEABLE.contains(&code))
                {
                    forceable.push((server, summary, line));
                } else {
                    failures.push(line);
                }
            }
            cx.update(|_, cx| refresh_panel(&workspace, cx))?;
            if !forceable.is_empty() {
                let detail = forceable
                    .iter()
                    .map(|(_, _, line)| line.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let answer = cx.update(|window, cx| {
                    window.prompt(
                        PromptLevel::Critical,
                        &if forceable.len() == 1 {
                            "1 workspace wasn't deleted".to_owned()
                        } else {
                            format!("{} workspaces weren't deleted", forceable.len())
                        },
                        Some(&detail),
                        &["Force Delete", "Cancel"],
                        cx,
                    )
                })?;
                if answer.await? == 0 {
                    for (server, summary, _) in forceable {
                        let forced = cx
                            .update(|_, cx| {
                                request(
                                    &server,
                                    "workspace.remove",
                                    json!({ "id": summary.id, "force": true }),
                                    cx,
                                )
                            })?
                            .await;
                        if let Err(error) = forced {
                            failures.push(format!(
                                "{}: {}",
                                summary.display_title(),
                                error_text(&error)
                            ));
                        }
                    }
                }
            }
            cx.update(|_, cx| {
                refresh_panel(&workspace, cx);
                if !failures.is_empty() {
                    show_failure(
                        &workspace,
                        format!("Couldn't delete {}", failures.join("; ")),
                        cx,
                    );
                }
            })
        })
        .detach_and_log_err(cx);
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
        let creates = |calls: &Calls| {
            calls_to(calls, "workspace.create")
                .into_iter()
                .map(|mut params| {
                    assert!(
                        params["create_id"].is_string(),
                        "each create can be cancelled"
                    );
                    if let Some(object) = params.as_object_mut() {
                        object.remove("create_id");
                    }
                    params
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(creates(&calls), std::slice::from_ref(&expected));
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
        assert_eq!(creates(&calls), [expected.clone(), expected]);
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
            modal.read_with(cx, |modal, _| (
                modal.search_error.clone(),
                modal.linear_unconfigured
            )),
            (None, true),
            "a missing key is a hint to connect Linear, not an error"
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

    fn scripts_stub(cx: &mut VisualTestContext, scripts: Value) -> Calls {
        cx.update(|_, cx| {
            stub_rpc(cx, move |method, params| match method {
                "project.list" => {
                    Ok(json!([{ "id": "proj-a", "name": "api", "base_ref": "origin/main" }]))
                }
                "config.get" => Ok(json!("claude")),
                "layout.list" => Ok(json!([{ "name": "default" }])),
                "project.scripts" => Ok(scripts.clone()),
                "workspace.create_cancel" => Ok(json!({ "cancelled": true })),
                "workspace.create" if params["name"] == "slow" => {
                    Err(rpc_error("cancelled", "the workspace create was cancelled"))
                }
                "workspace.create" => Ok(json!({ "id": "ws-new", "path": "/project" })),
                other => Err(anyhow::anyhow!("unexpected {other}")),
            })
        })
    }

    async fn open_dialog(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<NewWorkspaceModal> {
        workspace.update_in(cx, |workspace, window, cx| {
            open_new_workspace(None, workspace, window, cx)
        });
        cx.run_until_parked();
        active::<NewWorkspaceModal>(workspace, cx).expect("dialog")
    }

    #[gpui::test]
    async fn a_pull_request_start_point_needs_no_name(cx: &mut gpui::TestAppContext) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        let cx = &mut cx;
        scripts_stub(cx, json!({ "run_policy": "run-by-default" }));
        let modal = open_dialog(&workspace, cx).await;
        let start_ref = modal.read_with(cx, |modal, _| modal.start_ref.clone());
        modal.update_in(cx, |modal, window, cx| {
            modal.set_start(StartPoint::PullRequest, window, cx)
        });
        assert_eq!(
            modal.read_with(cx, |modal, cx| modal.create_params(cx)),
            Err("Enter the pull request's number or URL.")
        );
        type_into(&start_ref, "https://github.com/acme/api/pull/12", cx);
        assert_eq!(
            modal.read_with(cx, |modal, cx| modal.create_params(cx)),
            Ok(json!({
                "project": "proj-a", "provider": "claude",
                "pr": "https://github.com/acme/api/pull/12",
            })),
            "the server names it after the pull request's branch"
        );
    }

    #[gpui::test]
    async fn untrusted_scripts_prompt_before_creating(cx: &mut gpui::TestAppContext) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        let cx = &mut cx;
        let calls = scripts_stub(
            cx,
            json!({
                "run_policy": "ask",
                "setup": { "command": "pnpm install", "source": "shared", "trusted": false },
                "default_tabs": [{ "title": "Dev", "command": "pnpm dev" }],
                "default_tabs_trusted": false,
            }),
        );
        let modal = open_dialog(&workspace, cx).await;
        assert_eq!(
            calls_to(&calls, "project.scripts"),
            [json!({ "id": "proj-a" })]
        );
        let name = modal.read_with(cx, |modal, _| modal.name.clone());
        type_into(&name, "feature", cx);
        assert_eq!(
            modal
                .read_with(cx, |modal, cx| modal.create_params(cx))
                .map(|params| params["setup"].clone()),
            Ok(json!("run")),
            "an `ask` project gets the dialog's choice"
        );

        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert!(modal.read_with(cx, |modal, _| modal.is_trust_prompt_open()));
        assert!(
            calls_to(&calls, "workspace.create").is_empty(),
            "nothing runs before an answer"
        );
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.untrusted_scripts()),
            ["pnpm install", "pnpm dev"]
        );
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert!(
            calls_to(&calls, "workspace.create").is_empty(),
            "Enter doesn't answer the prompt"
        );
        cx.dispatch_action(menu::Cancel);
        cx.run_until_parked();
        assert!(!modal.read_with(cx, |modal, _| modal.is_trust_prompt_open()));
        assert!(
            active::<NewWorkspaceModal>(&workspace, cx).is_some(),
            "Escape goes back"
        );

        cx.dispatch_action(menu::Confirm);
        modal.update_in(cx, |modal, window, cx| {
            modal.create_without_scripts(window, cx)
        });
        cx.run_until_parked();
        let created = calls_to(&calls, "workspace.create");
        assert_eq!(created.len(), 1);
        assert_eq!(created[0]["setup"], "skip");
        assert!(created[0]["trust"].is_null());

        let modal = open_dialog(&workspace, cx).await;
        let name = modal.read_with(cx, |modal, _| modal.name.clone());
        type_into(&name, "trusted", cx);
        cx.dispatch_action(menu::Confirm);
        modal.update_in(cx, |modal, window, cx| modal.trust_and_create(window, cx));
        cx.run_until_parked();
        let created = calls_to(&calls, "workspace.create");
        assert_eq!(created[1]["trust"], true);
        assert_eq!(created[1]["setup"], "run");

        let modal = open_dialog(&workspace, cx).await;
        let name = modal.read_with(cx, |modal, _| modal.name.clone());
        type_into(&name, "always", cx);
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        modal.update_in(cx, |modal, window, cx| {
            modal.set_always_trust(true, cx);
            modal.trust_and_create(window, cx)
        });
        cx.run_until_parked();
        let created = calls_to(&calls, "workspace.create");
        assert_eq!(
            created[2]["trust"], "always",
            "Always trust covers later changes too"
        );
    }

    #[gpui::test]
    async fn an_untrusted_issue_command_asks_only_for_a_pull_request_start(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        let cx = &mut cx;
        scripts_stub(
            cx,
            json!({
                "run_policy": "run-by-default",
                "issue_command": { "command": "claude {{artifact_url}}", "source": "shared", "trusted": false },
            }),
        );
        let modal = open_dialog(&workspace, cx).await;
        assert!(modal.read_with(cx, |modal, _| modal.untrusted_scripts().is_empty()));
        modal.update_in(cx, |modal, window, cx| {
            modal.set_start(StartPoint::PullRequest, window, cx)
        });
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.untrusted_scripts()),
            ["claude {{artifact_url}}"]
        );
    }

    #[gpui::test]
    async fn a_create_in_flight_can_be_cancelled(cx: &mut gpui::TestAppContext) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        let cx = &mut cx;
        let calls = scripts_stub(cx, json!({ "run_policy": "run-by-default" }));
        let modal = open_dialog(&workspace, cx).await;
        let name = modal.read_with(cx, |modal, _| modal.name.clone());
        type_into(&name, "slow", cx);
        modal.update_in(cx, |modal, window, cx| {
            modal.confirm(&menu::Confirm, window, cx);
            assert!(modal.is_creating());
            modal.cancel(&menu::Cancel, window, cx);
            assert!(modal.cancelling, "Escape while creating cancels the create");
        });
        cx.run_until_parked();
        let create_id = calls_to(&calls, "workspace.create")[0]["create_id"].clone();
        assert_eq!(
            calls_to(&calls, "workspace.create_cancel"),
            [json!({ "create_id": create_id })]
        );
        modal.read_with(cx, |modal, _| {
            assert!(!modal.is_creating());
            assert_eq!(modal.error(), None);
            assert_eq!(modal.notice(), Some("Cancelled; nothing was created."));
        });
        assert!(
            active::<NewWorkspaceModal>(&workspace, cx).is_some(),
            "the dialog stays open to try again"
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
                CardAction::Pin,
                CardAction::Rename,
                CardAction::Sleep,
                CardAction::RerunSetup,
                CardAction::Delete
            ]
        );
        assert_eq!(card_actions(&asleep)[2], CardAction::Wake);

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
    async fn pin_and_unpin_set_the_flag_on_the_owning_server(cx: &mut gpui::TestAppContext) {
        let card = summary("fix-login", "/w/fix-login", None);
        let mut pinned = summary("docs", "/w/docs", None);
        pinned.pinned = true;
        assert_eq!(card_actions(&pinned)[0], CardAction::Unpin);
        let (workspace, _, mut cx) = panel_window(vec![card.clone(), pinned.clone()], cx).await;
        let cx = &mut cx;
        let calls = cx.update(|_, cx| stub_rpc(cx, |_, _| Ok(json!({}))));
        let weak = workspace.downgrade();
        cx.update(|window, cx| {
            run_card_action(
                CardAction::Pin,
                &ServerRef::Primary,
                &card,
                &weak,
                window,
                cx,
            );
            run_card_action(
                CardAction::Unpin,
                &ServerRef::Saved("desk".into()),
                &pinned,
                &weak,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "workspace.set"),
            [
                json!({ "id": "ws-fix-login", "pinned": true }),
                json!({ "id": "ws-docs", "pinned": false })
            ]
        );
        assert!(!has_failure_toast(&workspace, cx));
    }

    #[gpui::test]
    async fn bulk_actions_run_on_every_selected_workspace_with_one_delete_confirmation(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut pinned = summary("pinned", "/w/pinned", None);
        pinned.pinned = true;
        let mut asleep = summary("asleep", "/w/asleep", None);
        asleep.sleeping = true;
        let dirty = summary("dirty", "/w/dirty", None);
        let targets = vec![
            (ServerRef::Primary, pinned.clone()),
            (ServerRef::Saved("desk".into()), asleep.clone()),
            (ServerRef::Primary, dirty.clone()),
        ];
        let labels: Vec<(BulkAction, String, usize)> = bulk_actions(&targets)
            .into_iter()
            .map(|(action, label, applies)| (action, label, applies.len()))
            .collect();
        assert_eq!(
            labels,
            [
                (BulkAction::Pin, "Pin 2 Workspaces".to_owned(), 2),
                (BulkAction::Sleep, "Sleep 2 Workspaces".to_owned(), 2),
                (BulkAction::Wake, "Wake 1 Workspace".to_owned(), 1),
                (BulkAction::Delete, "Delete 3 Workspaces…".to_owned(), 3),
            ]
        );
        assert_eq!(
            bulk_actions(&targets[..1])[0].1,
            "Unpin 1 Workspace",
            "all pinned: unpin"
        );

        let (workspace, _, mut cx) =
            panel_window(vec![pinned.clone(), asleep.clone(), dirty.clone()], cx).await;
        let cx = &mut cx;
        let calls = cx.update(|_, cx| {
            stub_rpc(cx, |method, params| match method {
                "workspace.remove"
                    if params["id"] == "ws-dirty" && params["force"] != json!(true) =>
                {
                    Err(rpc_error("dirty", "worktree has uncommitted changes"))
                }
                _ => Ok(json!({})),
            })
        });
        let weak = workspace.downgrade();
        cx.update(|window, cx| {
            run_bulk_action(
                BulkAction::Pin,
                targets[1..].to_vec(),
                weak.clone(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "workspace.set"),
            [
                json!({ "id": "ws-asleep", "pinned": true }),
                json!({ "id": "ws-dirty", "pinned": true })
            ]
        );

        cx.update(|window, cx| {
            run_bulk_action(
                BulkAction::Delete,
                targets.clone(),
                weak.clone(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("Delete 3 Workspaces");
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "workspace.remove").len(),
            3,
            "one confirmation for all"
        );
        assert!(cx.has_pending_prompt(), "the dirty one is offered a force");
        cx.simulate_prompt_answer("Force Delete");
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "workspace.remove").last(),
            Some(&json!({ "id": "ws-dirty", "force": true }))
        );
        assert!(!cx.has_pending_prompt());
        assert!(!has_failure_toast(&workspace, cx));
    }

    #[gpui::test]
    async fn the_dialogs_create_on_the_picked_server(cx: &mut gpui::TestAppContext) {
        let (workspace, panel, mut cx) = panel_window(Vec::new(), cx).await;
        let cx = &mut cx;
        let calls = new_workspace_stub(cx, false);
        let store = panel.read_with(cx, |panel, _| panel.remote_servers().clone());
        store.update(cx, |store, cx| {
            store.set_saved(
                &[crate::servers::tests::saved("desk", "100.64.0.2:4000")],
                cx,
            )
        });
        let choices: Vec<String> = cx.update(|_, cx| {
            server_choices(cx)
                .into_iter()
                .map(|(_, label)| label)
                .collect()
        });
        assert_eq!(choices, ["This machine", "desk"]);

        workspace.update_in(cx, |workspace, window, cx| {
            open_new_workspace(None, workspace, window, cx)
        });
        cx.run_until_parked();
        let modal = active::<NewWorkspaceModal>(&workspace, cx).expect("the dialog");
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.server().clone()),
            ServerRef::Primary,
            "the window's own server by default"
        );
        modal.update_in(cx, |modal, window, cx| {
            modal.set_project("proj-w".into(), window, cx);
            modal.set_server(ServerRef::Saved("desk".into()), window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "project.list").len(),
            2,
            "the saved server's projects load"
        );
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.server(), &ServerRef::Saved("desk".into()));
            assert_eq!(
                modal.selected_project(),
                Some("proj-a"),
                "the old pick isn't kept across servers"
            );
        });

        cx.dispatch_action(menu::Cancel);
        workspace.update_in(cx, |workspace, window, cx| {
            open_add_project(PathBuf::from("/srv/repo"), workspace, window, cx)
        });
        cx.run_until_parked();
        let add = active::<AddProjectModal>(&workspace, cx).expect("the Add project dialog");
        add.update_in(cx, |modal, window, cx| {
            modal.set_server(ServerRef::Saved("desk".into()), window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            calls_to(&calls, "project.inspect"),
            [
                json!({ "path": "/srv/repo" }),
                json!({ "path": "/srv/repo" })
            ],
            "the folder is read again on the picked server"
        );
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
