pub mod accounts;
pub mod browser;
pub mod cards;
pub mod changes_tab;
pub mod client_settings;
pub mod comment_input;
pub mod commit_modal;
pub mod diff_tab;
mod focus;
pub mod github_panel;
pub mod history;
pub mod layout;
pub mod lifecycle;
pub mod links;
mod model;
mod next_action;
pub mod notifications;
pub mod palettes;
pub mod pane_nav;
pub mod quick_commands;
pub mod remote_agent;
pub mod remote_control;
pub mod review;
pub mod routing;
pub mod servers;
pub mod settings_view;
pub mod sidebar;
pub mod source_control_settings;
mod ssh_install;
pub mod status_style;
pub(crate) mod terminals;
mod worktree_trust;
pub mod zed_import;

pub use changes_tab::SourceControlPanel;
pub use diff_tab::DiffTab;
pub use github_panel::GitHubPanel;
pub use links::{PortEntry, PortsStore, PreviewDeployment, WorkspaceLinks};
pub use next_action::{NextAction, NextActionButton};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use collections::HashSet;
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels,
    Render, Subscription, Task, WeakEntity, Window, WindowHandle, actions, px,
};
use gpui_tokio::Tokio;
use icarus_client::Client;
use ui::{
    ButtonLike, CommonAnimationExt as _, ContextMenu, Indicator, PopoverMenu, Tooltip, prelude::*,
    right_click_menu,
};
use util::ResultExt as _;
use workspace::dock::{DockPosition, Panel, PanelEvent, PanelSizeState};
use workspace::{AppState, MultiWorkspace, OpenOptions, Workspace};

use crate::github_panel::{AvatarKind, render_avatar};

pub use model::{
    AgentSummary, IssueLabel, PrAuthor, PrGroup, PrSummary, SetupBadge, SetupSummary, StackGroup,
    StackLane, StackPlace, StatusColumn, StatusGroup, Unread, UnreadKind, WorkspaceSummary,
    group_by_pr, group_by_stack, group_by_status, pinned_first,
};

actions!(
    icarus,
    [
        /// Shows or hides the Icarus workspaces panel.
        ToggleWorkspacesPanel,
        /// Reloads workspaces from the Icarus server.
        RefreshWorkspaces,
        /// Runs this workspace's next step: create PR, commit & push, ready for review, resolve conflicts or merge.
        RunNextAction,
        /// Opens this workspace's running frontend in the browser.
        OpenFrontend,
        /// Opens this workspace's pull request preview deployment in the browser.
        OpenPreview,
        /// Opens this workspace's pull request on GitHub.
        OpenPullRequest,
        /// Copies the URL of this workspace's pull request.
        CopyPullRequestUrl,
        /// Activates the next tab of the active tab's kind (agent, terminal, file, diff…).
        ActivateNextItemOfSameType,
        /// Activates the previous tab of the active tab's kind.
        ActivatePreviousItemOfSameType,
        /// Jumps to another workspace by name, issue, title, branch or status.
        OpenWorkspaceSwitcher,
    ]
);

/// A card's setup state: a spinner while the project's setup script runs, a warning if it failed.
fn render_setup_badge(workspace_id: &str, badge: SetupBadge) -> impl IntoElement {
    let (icon, label, color, tooltip) = match badge {
        SetupBadge::Running => (
            Icon::new(IconName::ArrowCircle)
                .size(IconSize::XSmall)
                .color(Color::Accent)
                .with_rotate_animation(2),
            "Setting up",
            Color::Accent,
            "The project's setup script is running in the Setup terminal.".to_owned(),
        ),
        SetupBadge::Failed(reason) => (
            Icon::new(IconName::Warning)
                .size(IconSize::XSmall)
                .color(Color::Error),
            "Setup failed",
            Color::Error,
            format!("{reason} Rerun it with icarus workspace setup."),
        ),
    };
    h_flex()
        .id(SharedString::from(format!("icarus-setup-{workspace_id}")))
        .gap_0p5()
        .child(icon)
        .child(Label::new(label).size(LabelSize::XSmall).color(color))
        .tooltip(Tooltip::text(tooltip))
}

/// Orca's tab.nextSameType / previousSameType: cycles the active pane's tabs of the active tab's kind.
fn activate_item_of_same_type(
    forward: bool,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let pane = workspace.active_pane().clone();
    let target = {
        let pane = pane.read(cx);
        let items: Vec<_> = pane.items().collect();
        let active = pane.active_item_index();
        let Some(kind) = items
            .get(active)
            .map(|item| item.to_any_view().entity_type())
        else {
            return;
        };
        let count = items.len();
        (1..count)
            .map(|offset| {
                if forward {
                    (active + offset) % count
                } else {
                    (active + count - offset) % count
                }
            })
            .find(|&index| items[index].to_any_view().entity_type() == kind)
    };
    if let Some(index) = target {
        pane.update(cx, |pane, cx| {
            pane.activate_item(index, true, true, window, cx)
        });
    }
}

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// A request from the server (e.g. `icarus file open`) to show a file at a position.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct OpenFileRequest {
    pub path: PathBuf,
    pub line: Option<u32>,
    pub column: Option<u32>,
    #[serde(default)]
    pub focus: bool,
}

type OpenFileHandler = Arc<dyn Fn(OpenFileRequest, &mut App) + 'static>;

struct GlobalOpenFileHandler(OpenFileHandler);

impl gpui::Global for GlobalOpenFileHandler {}

/// The app crate decides how to open files; this crate only receives the requests.
pub fn set_open_file_handler(cx: &mut App, handler: impl Fn(OpenFileRequest, &mut App) + 'static) {
    cx.set_global(GlobalOpenFileHandler(Arc::new(handler)));
}

/// An item (diff note, review comment, failed check, ...) the server wants attached to a composer.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct ComposerAttachment {
    pub id: String,
    pub kind: String,
    pub title: String,
}

/// Payload of the `composer.attach` event.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct ComposerAttachRequest {
    pub workspace_path: PathBuf,
    pub attachment: ComposerAttachment,
}

type ComposerAttachHandler =
    Arc<dyn Fn(ComposerAttachment, &mut Workspace, &mut Window, &mut Context<Workspace>) + 'static>;

struct GlobalComposerAttachHandler(ComposerAttachHandler);

impl gpui::Global for GlobalComposerAttachHandler {}

/// The app crate owns the agent panel; this crate only routes attachments to the right workspace.
pub fn set_composer_attach_handler(
    cx: &mut App,
    handler: impl Fn(ComposerAttachment, &mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    cx.set_global(GlobalComposerAttachHandler(Arc::new(handler)));
}

fn attach_to_composer(request: ComposerAttachRequest, cx: &mut App) {
    let Some(handler) = cx
        .try_global::<GlobalComposerAttachHandler>()
        .map(|handler| handler.0.clone())
    else {
        log::error!("no composer attach handler registered");
        return;
    };
    let Some((window_handle, workspace)) = workspace_for_path(&request.workspace_path, cx) else {
        log::error!(
            "no window to attach {:?} to for {}",
            request.attachment.id,
            request.workspace_path.display()
        );
        return;
    };
    window_handle
        .update(cx, |multi_workspace, window, cx| {
            multi_workspace.activate(workspace.clone(), None, window, cx);
            window.activate_window();
            workspace.update(cx, |workspace, cx| {
                handler(request.attachment, workspace, window, cx);
            });
        })
        .log_err();
}

fn multi_workspace_windows(cx: &App) -> Vec<WindowHandle<MultiWorkspace>> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .collect()
}

/// The window showing the workspace rooted at `path`, if any.
fn window_for_path(
    path: &Path,
    cx: &App,
) -> Option<(WindowHandle<MultiWorkspace>, Entity<Workspace>)> {
    for window in multi_workspace_windows(cx) {
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        let matching = multi_workspace.workspaces().find(|workspace| {
            workspace
                .read(cx)
                .root_paths(cx)
                .iter()
                .any(|root| root.as_ref() == path)
        });
        if let Some(workspace) = matching {
            return Some((window, workspace.clone()));
        }
    }
    None
}

/// The workspace in the active window, else in the first Icarus window.
pub(crate) fn front_workspace(
    cx: &App,
) -> Option<(WindowHandle<MultiWorkspace>, Entity<Workspace>)> {
    let window = cx
        .active_window()
        .and_then(|window| window.downcast::<MultiWorkspace>())
        .or_else(|| multi_workspace_windows(cx).first().copied())?;
    let workspace = window.read(cx).ok()?.workspace().clone();
    Some((window, workspace))
}

/// The window showing the workspace rooted at `path`, or the active window if none is.
fn workspace_for_path(
    path: &Path,
    cx: &App,
) -> Option<(WindowHandle<MultiWorkspace>, Entity<Workspace>)> {
    if let Some(found) = window_for_path(path, cx) {
        return Some(found);
    }
    let window = cx
        .active_window()
        .and_then(|window| window.downcast::<MultiWorkspace>())
        .or_else(|| multi_workspace_windows(cx).first().copied())?;
    let workspace = window.read(cx).ok()?.workspace().clone();
    Some((window, workspace))
}

type LayoutAppliedHandler =
    Arc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static>;

struct GlobalLayoutAppliedHandler(LayoutAppliedHandler);

impl gpui::Global for GlobalLayoutAppliedHandler {}

/// The app crate owns the composer, so it decides what gets focus once a layout is in place.
pub fn set_layout_applied_handler(
    cx: &mut App,
    handler: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    cx.set_global(GlobalLayoutAppliedHandler(Arc::new(handler)));
}

type OpenAgentHandler = Arc<
    dyn Fn(
            Option<String>,
            Option<Entity<workspace::Pane>>,
            &mut Workspace,
            &mut Window,
            &mut Context<Workspace>,
        ) + 'static,
>;

struct GlobalOpenAgentHandler(OpenAgentHandler);

impl gpui::Global for GlobalOpenAgentHandler {}

/// The app crate owns agent tabs; this crate asks it to open one.
pub fn set_open_agent_handler(
    cx: &mut App,
    handler: impl Fn(
        Option<String>,
        Option<Entity<workspace::Pane>>,
        &mut Workspace,
        &mut Window,
        &mut Context<Workspace>,
    ) + 'static,
) {
    cx.set_global(GlobalOpenAgentHandler(Arc::new(handler)));
}

/// Opens an agent tab in `pane` (default: the active pane): the server agent `agent_id`, or a new agent.
pub fn open_agent_tab(
    agent_id: Option<String>,
    pane: Option<Entity<workspace::Pane>>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match cx
        .try_global::<GlobalOpenAgentHandler>()
        .map(|handler| handler.0.clone())
    {
        Some(handler) => handler(agent_id, pane, workspace, window, cx),
        None => log::error!("no open-agent handler registered"),
    }
}

/// Shows the workspace's window (opening it if needed) and arranges its tabs as `pending` says.
fn open_layout(pending: layout::PendingLayout, cx: &mut App) {
    let path = pending.path.clone();
    with_workspace_window(path, cx, move |workspace, window, cx| {
        layout::apply_layout(&pending.layout, &pending.path, workspace, window, cx);
        layout_applied(workspace, window, cx);
    });
}

/// Brings the window showing `path` to the front (opening one if needed), then runs `then` in its workspace.
pub(crate) fn with_workspace_window(
    path: PathBuf,
    cx: &mut App,
    then: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    let window = match window_for_path(&path, cx) {
        Some(found) => Task::ready(Ok(found)),
        None => {
            let Some(app_state) = AppState::try_global(cx) else {
                log::error!("no app state to open {} with", path.display());
                return;
            };
            let open = workspace::open_paths(&[path], app_state, OpenOptions::default(), cx);
            cx.spawn(async move |_| {
                let opened = open.await?;
                anyhow::Ok((opened.window, opened.workspace))
            })
        }
    };
    cx.spawn(async move |cx| {
        let (window, workspace) = window.await?;
        window.update(cx, |multi_workspace, window, cx| {
            multi_workspace.activate(workspace.clone(), None, window, cx);
            window.activate_window();
            workspace.update(cx, |workspace, cx| then(workspace, window, cx));
        })?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// The app's connection to its server, if it is connected.
pub(crate) fn app_client(cx: &App) -> Option<Arc<Client>> {
    cx.try_global::<AppConnection>()
        .and_then(|connection| connection.client.clone())
}

fn layout_applied(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(handler) = cx
        .try_global::<GlobalLayoutAppliedHandler>()
        .map(|handler| handler.0.clone())
    {
        handler(workspace, window, cx);
    }
}

/// Applies layouts the server holds for workspaces that are open in a window right now.
async fn apply_pending_for_open_windows(client: Arc<Client>, cx: &mut gpui::AsyncApp) {
    let Some(pending) = Tokio::spawn_result(cx, layout::pending_layouts(client.clone()))
        .await
        .log_err()
    else {
        return;
    };
    for candidate in pending {
        let open = cx.update(|cx| window_for_path(&candidate.path, cx).is_some());
        if !open {
            continue;
        }
        let taken = Tokio::spawn_result(
            cx,
            layout::take_layout(client.clone(), candidate.workspace_id.clone()),
        )
        .await;
        if let Some(Some(pending)) = taken.log_err() {
            cx.update(|cx| open_layout(pending, cx));
        }
    }
}

#[derive(serde::Deserialize)]
struct CaptureRequest {
    dir: PathBuf,
}

#[derive(serde::Deserialize)]
struct DispatchRequest {
    action: String,
    args: Option<serde_json::Value>,
    result: Option<PathBuf>,
}

/// Runs a named action in the frontmost window, as its key binding would, and reports the outcome to `result`.
fn dispatch_action(request: DispatchRequest, cx: &mut App) {
    let outcome = cx
        .build_action(&request.action, request.args)
        .map_err(|error| anyhow::anyhow!("{error}"))
        .and_then(|action| {
            let window = cx
                .active_window()
                .or_else(|| cx.windows().into_iter().next())
                .ok_or_else(|| anyhow::anyhow!("no app window is open"))?;
            cx.update_window(window, |_, window, cx| window.dispatch_action(action, cx))
        });
    let report = match &outcome {
        Ok(()) => serde_json::json!({ "action": request.action }),
        Err(error) => {
            serde_json::json!({ "action": request.action, "error": format!("{error:#}") })
        }
    };
    if let Some(path) = request.result {
        cx.background_spawn(async move { std::fs::write(path, report.to_string()) })
            .detach_and_log_err(cx);
    }
    outcome.log_err();
}

/// Saves each window's last rendered frame as a PNG, then writes `manifest.json` so the CLI knows it's done.
/// A browser page drawn outside GPUI's frame: where it sits and its snapshot, to paste into the window image.
struct PageCapture {
    window_id: u64,
    scale: f32,
    info: browser::BrowserCapture,
    snapshot: Option<futures::channel::oneshot::Receiver<Option<Vec<u8>>>>,
}

fn capture_windows(request: CaptureRequest, cx: &mut App) {
    let mut frames = Vec::new();
    let mut pages = Vec::new();
    let mut errors = Vec::new();
    for handle in cx.windows() {
        let id = handle.window_id().as_u64();
        // Windows outside a visible GUI session never get a display-link frame, so draw one now.
        let captured = cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
            let image = window.render_to_image();
            window.refresh();
            (image, window.scale_factor())
        });
        let scale = match captured {
            Ok((Ok(image), scale)) => {
                frames.push((id, image));
                scale
            }
            Ok((Err(error), _)) => {
                errors.push(format!("window {id}: {error:#}"));
                continue;
            }
            Err(error) => {
                errors.push(format!("window {id}: {error:#}"));
                continue;
            }
        };
        for tab in browser::browsers_in(handle, cx) {
            let tab = tab.read(cx);
            let info = tab.capture_info();
            let snapshot = info.shown.then(|| tab.snapshot_png()).flatten();
            pages.push(PageCapture {
                window_id: id,
                scale,
                info,
                snapshot,
            });
        }
    }
    cx.spawn(async move |cx| {
        let mut snapshots = Vec::new();
        for page in &mut pages {
            let png = match page.snapshot.take() {
                Some(receiver) => receiver.await.ok().flatten(),
                None => None,
            };
            snapshots.push(png);
        }
        cx.background_spawn(async move {
            let mut files = Vec::new();
            let mut browsers = Vec::new();
            for (index, (page, png)) in pages.iter().zip(snapshots).enumerate() {
                let mut entry = serde_json::to_value(&page.info)?;
                entry["window"] = serde_json::json!(page.window_id);
                if let Some(png) = png {
                    let path = request.dir.join(format!("browser-{index}.png"));
                    std::fs::write(&path, &png)?;
                    entry["snapshot"] = serde_json::json!(path);
                    files.push(path);
                    let decoded = image::load_from_memory(&png).map(|image| image.to_rgba8());
                    match (decoded, page.info.bounds) {
                        (Ok(page_image), Some(bounds)) => {
                            if let Some((_, frame)) =
                                frames.iter_mut().find(|(id, _)| *id == page.window_id)
                            {
                                paste_page(frame, &page_image, bounds, page.scale);
                            }
                        }
                        (Err(error), _) => errors.push(format!("browser {index}: {error}")),
                        _ => {}
                    }
                }
                browsers.push(entry);
            }
            for (id, image) in &frames {
                let path = request.dir.join(format!("window-{id}.png"));
                match image.save_with_format(&path, image::ImageFormat::Png) {
                    Ok(()) => files.push(path),
                    Err(error) => errors.push(format!("{}: {error}", path.display())),
                }
            }
            let manifest =
                serde_json::json!({ "files": files, "errors": errors, "browsers": browsers });
            std::fs::write(
                request.dir.join("manifest.json"),
                serde_json::to_vec_pretty(&manifest)?,
            )?;
            anyhow::Ok(())
        })
        .await
    })
    .detach_and_log_err(cx);
}

/// Draws a web page snapshot over the window image at the page's logical bounds.
fn paste_page(frame: &mut image::RgbaImage, page: &image::RgbaImage, bounds: [f32; 4], scale: f32) {
    let [x, y, width, height] = bounds.map(|value| (value * scale).round().max(0.0) as u32);
    if width == 0 || height == 0 {
        return;
    }
    let page = image::imageops::resize(page, width, height, image::imageops::FilterType::Triangle);
    image::imageops::overlay(frame, &page, i64::from(x), i64::from(y));
}

/// Refreshes the views a server event concerns; every connected server's events come through here.
pub(crate) fn handle_view_event(topic: &str, payload: serde_json::Value, cx: &mut App) {
    match topic {
        // Notes render in the same diff-tab layer, so one reload covers both.
        "review.changed" | "note.changed" => review::review_changed(cx),
        "git.changed" => changes_tab::git_changed(payload, cx),
        "ports.changed" => links::ports_changed(payload, cx),
        "config.changed" => settings_view::config_changed(cx),
        _ => {}
    }
}

/// The app-wide connection to the server and the Icarus workspace paths it last listed.
#[derive(Default)]
pub(crate) struct AppConnection {
    pub(crate) client: Option<Arc<Client>>,
    pub(crate) workspace_paths: Vec<PathBuf>,
    /// The saved server the app's own connection uses instead of this machine's, if the user picked one.
    pub(crate) primary: Option<String>,
    /// Ends the current connection so the next one goes to a newly picked primary server.
    pub(crate) reconnect: Option<futures::channel::oneshot::Sender<()>>,
}

impl gpui::Global for AppConnection {}

/// Fetches the context block an attachment expands to when the prompt is sent.
pub fn fetch_attachment_text(id: String, cx: &mut App) -> Task<Result<String>> {
    #[derive(serde::Deserialize)]
    struct AttachmentText {
        text: String,
    }

    Tokio::spawn_result(cx, async move {
        let home = server_home().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
        let (client, _events) = Client::connect(&home, "icarus-panel").await?;
        let response = client
            .request("attachment.get", serde_json::json!({ "id": id }))
            .await?;
        let attachment: AttachmentText = serde_json::from_value(response)?;
        Ok(attachment.text)
    })
}

/// Keeps one app-wide connection to the server for requests aimed at the editor, reconnecting as needed.
fn start_app_connection(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            let primary = cx.update(|cx| cx.default_global::<AppConnection>().primary.clone());
            let connect = Tokio::spawn_result(cx, async move {
                match primary {
                    Some(name) => {
                        let saved = icarus_client::servers::SavedServers::load()?;
                        let entry = saved.get(&name).cloned().ok_or_else(|| {
                            anyhow::anyhow!(
                                "no saved server named {name}; see `icarus remote list`"
                            )
                        })?;
                        Client::connect_saved(&entry, "icarus-app").await
                    }
                    None => {
                        let home =
                            server_home().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
                        Client::connect(&home, "icarus-app").await
                    }
                }
            });
            let Ok((client, mut events)) = connect.await else {
                cx.background_executor().timer(REFRESH_INTERVAL * 2).await;
                continue;
            };
            let (reconnect, mut switched) = futures::channel::oneshot::channel();
            cx.update(|cx| {
                let connection = cx.default_global::<AppConnection>();
                connection.client = Some(client.clone());
                connection.reconnect = Some(reconnect);
            });
            let listed =
                Tokio::spawn_result(cx, worktree_trust::list_workspace_paths(client.clone())).await;
            if let Some(listed) = listed.log_err() {
                cx.update(|cx| {
                    worktree_trust::set_icarus_workspace_paths(
                        listed.into_iter().map(|(_, path)| path).collect(),
                        cx,
                    )
                });
            }
            apply_pending_for_open_windows(client.clone(), cx).await;
            cx.update(focus::connected);
            notifications::connected(client.clone(), cx).await;
            loop {
                let next = Tokio::spawn(cx, async move {
                    let event = events.recv().await;
                    (events, event)
                });
                let next = match futures::future::select(next, &mut switched).await {
                    futures::future::Either::Left((next, _)) => next,
                    // A newly picked primary server: drop this connection and connect to that one.
                    futures::future::Either::Right(_) => break,
                };
                let Ok((returned, Some(event))) = next else {
                    break;
                };
                events = returned;
                match event.topic.as_str() {
                    "client.open_file" => {
                        let Some(request) =
                            serde_json::from_value::<OpenFileRequest>(event.payload).log_err()
                        else {
                            continue;
                        };
                        cx.update(|cx| {
                            if let Some(handler) = cx
                                .try_global::<GlobalOpenFileHandler>()
                                .map(|h| h.0.clone())
                            {
                                handler(request, cx);
                            }
                        });
                    }
                    "composer.attach" => {
                        let Some(request) =
                            serde_json::from_value::<ComposerAttachRequest>(event.payload)
                                .log_err()
                        else {
                            continue;
                        };
                        cx.update(|cx| attach_to_composer(request, cx));
                    }
                    "client.capture" => {
                        let Some(request) =
                            serde_json::from_value::<CaptureRequest>(event.payload).log_err()
                        else {
                            continue;
                        };
                        cx.update(|cx| capture_windows(request, cx));
                    }
                    "client.config" => {
                        let Some(request) =
                            serde_json::from_value::<client_settings::ConfigRequest>(event.payload)
                                .log_err()
                        else {
                            continue;
                        };
                        cx.update(|cx| client_settings::handle_request(request, cx));
                    }
                    "client.dispatch" => {
                        let Some(request) =
                            serde_json::from_value::<DispatchRequest>(event.payload).log_err()
                        else {
                            continue;
                        };
                        cx.update(|cx| dispatch_action(request, cx));
                    }
                    "review.changed" | "note.changed" | "git.changed" | "ports.changed"
                    | "config.changed" => {
                        cx.update(|cx| handle_view_event(&event.topic, event.payload, cx));
                    }
                    "terminal.keyboard" | "terminal.label" => cx.update(|cx| {
                        let primary = routing::primary(cx);
                        terminals::terminal_changed(
                            primary.as_deref(),
                            &event.topic,
                            &event.payload,
                            cx,
                        )
                    }),
                    "notification" | "notifications.changed" => {
                        cx.update(|cx| {
                            notifications::handle_event(&event.topic, event.payload, cx)
                        });
                    }
                    "open_layout" => {
                        let Some(announced) =
                            serde_json::from_value::<layout::PendingLayout>(event.payload)
                                .log_err()
                        else {
                            continue;
                        };
                        if let Some(listed) = Tokio::spawn_result(
                            cx,
                            worktree_trust::list_workspace_paths(client.clone()),
                        )
                        .await
                        .log_err()
                        {
                            cx.update(|cx| {
                                worktree_trust::set_icarus_workspace_paths(
                                    listed.into_iter().map(|(_, path)| path).collect(),
                                    cx,
                                )
                            });
                        }
                        // Claiming first makes applying idempotent across windows and reconnects.
                        let taken = Tokio::spawn_result(
                            cx,
                            layout::take_layout(client.clone(), announced.workspace_id),
                        )
                        .await;
                        if let Some(Some(pending)) = taken.log_err() {
                            cx.update(|cx| open_layout(pending, cx));
                        }
                    }
                    _ => {}
                }
            }
            cx.update(|cx| cx.default_global::<AppConnection>().client = None);
            drop(client);
            cx.background_executor().timer(REFRESH_INTERVAL).await;
        }
    })
    .detach();
}

struct IcarusEnabled;

impl gpui::Global for IcarusEnabled {}

/// Whether `init` ran; components built without it (e.g. in upstream Zed tests) must not reach for the server.
pub fn enabled(cx: &App) -> bool {
    cx.has_global::<IcarusEnabled>()
}

#[cfg(test)]
pub(crate) fn enable_for_tests(cx: &mut App) {
    cx.set_global(IcarusEnabled);
}

pub fn init(cx: &mut App) {
    cx.set_global(IcarusEnabled);
    routing::load_primary(cx);
    servers::store(cx, true);
    start_app_connection(cx);
    next_action::register(cx);
    links::register(cx);
    worktree_trust::register(cx);
    github_panel::register(cx);
    browser::register(cx);
    changes_tab::register(cx);
    diff_tab::register(cx);
    review::attach_notes_to_file_editors(cx);
    sidebar::register(cx);
    zed_import::register(cx);
    quick_commands::register(cx);
    terminals::register(cx);
    notifications::register(cx);
    history::register(cx);
    accounts::register(cx);
    remote_control::register(cx);
    settings_view::register(cx);
    client_settings::init(cx);
    focus::register(cx);
    pane_nav::register(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        if window.is_some() {
            // A workspace opened after its layout was stored (e.g. the app was closed) applies it now.
            cx.spawn(async move |workspace, cx| {
                for _ in 0..20 {
                    let ready = workspace
                        .read_with(cx, |workspace, cx| !workspace.root_paths(cx).is_empty())
                        .unwrap_or(false);
                    if ready {
                        break;
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(250))
                        .await;
                }
                let client = cx.update(|cx| {
                    cx.try_global::<AppConnection>()
                        .and_then(|connection| connection.client.clone())
                });
                if let Some(client) = client {
                    apply_pending_for_open_windows(client, cx).await;
                }
            })
            .detach();
        }
        workspace.register_action(|workspace, _: &ActivateNextItemOfSameType, window, cx| {
            activate_item_of_same_type(true, workspace, window, cx);
        });
        workspace.register_action(
            |workspace, _: &ActivatePreviousItemOfSameType, window, cx| {
                activate_item_of_same_type(false, workspace, window, cx);
            },
        );
        workspace.register_action(|workspace, _: &ToggleWorkspacesPanel, window, cx| {
            workspace.toggle_panel_focus::<WorkspacesPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &RefreshWorkspaces, _window, cx| {
            if let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) {
                panel.update(cx, |panel, cx| panel.refresh(cx));
            }
        });
        register_panel_actions(workspace);
    })
    .detach();
}

/// `#rrggbb` (Linear's label colours) as a colour.
fn parse_hex_color(hex: &str) -> Option<gpui::Hsla> {
    let digits = hex.strip_prefix('#').unwrap_or(hex);
    if digits.len() != 6 {
        return None;
    }
    u32::from_str_radix(digits, 16)
        .ok()
        .map(|value| gpui::rgb(value).into())
}

fn toast(workspace: &mut Workspace, message: &'static str, cx: &mut Context<Workspace>) {
    struct IcarusPanelToast;
    workspace.show_toast(
        workspace::Toast::new(
            workspace::notifications::NotificationId::unique::<IcarusPanelToast>(),
            message,
        )
        .autohide(),
        cx,
    );
}

/// The workspaces panel's view actions, the workspace switcher and Open Linear issue.
pub fn register_panel_actions(workspace: &mut Workspace) {
    workspace.register_action(|workspace, action: &cards::ToggleCardField, _, cx| {
        if let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) {
            panel.update(cx, |panel, cx| panel.toggle_card_field(&action.field, cx));
        }
    });
    workspace.register_action(|workspace, _: &cards::UseSharedCardFields, _, cx| {
        if let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) {
            panel.update(cx, |panel, cx| panel.set_local_card_fields(None, cx));
        }
    });
    workspace.register_action(|workspace, action: &cards::SetWorkspaceGrouping, _, cx| {
        if let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) {
            let group_by = cards::GroupBy::parse(&action.group_by);
            panel.update(cx, |panel, cx| panel.set_group_by(group_by, cx));
        }
    });
    workspace.register_action(|workspace, action: &cards::SetWorkspaceSort, _, cx| {
        if let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) {
            let sort_by = cards::SortBy::parse(&action.sort_by);
            panel.update(cx, |panel, cx| panel.set_sort_by(sort_by, cx));
        }
    });
    workspace.register_action(|workspace, _: &cards::FocusWorkspaceFilter, window, cx| {
        if workspace.panel::<WorkspacesPanel>(cx).is_none() {
            return;
        }
        workspace.open_panel::<WorkspacesPanel>(window, cx);
        if let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) {
            panel.update(cx, |panel, cx| panel.focus_filter(window, cx));
        }
    });
    workspace.register_action(|workspace, _: &cards::ToggleGroupByStack, _, cx| {
        if let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) {
            panel.update(cx, |panel, cx| {
                let group_by = if panel.view.group_by == cards::GroupBy::Stack {
                    cards::GroupBy::Status
                } else {
                    cards::GroupBy::Stack
                };
                panel.set_group_by(group_by, cx)
            });
        }
    });
    workspace.register_action(|workspace, _: &cards::OpenLinearIssue, _, cx| {
        let roots: Vec<PathBuf> = workspace
            .root_paths(cx)
            .iter()
            .map(|root| root.to_path_buf())
            .collect();
        let found = workspace.panel::<WorkspacesPanel>(cx).and_then(|panel| {
            panel
                .read(cx)
                .workspace_for_roots(&roots)
                .map(|w| (w.issue.is_some(), w.issue_url()))
        });
        match found {
            Some((_, Some(url))) => cx.open_url(&url),
            Some((true, None)) => toast(
                workspace,
                "The linked issue hasn't loaded yet; check the Linear key (`icarus issue status`).",
                cx,
            ),
            Some((false, _)) => toast(workspace, "This workspace has no linked issue.", cx),
            None => toast(workspace, "This window isn't an Icarus workspace.", cx),
        }
    });
    workspace.register_action(|workspace, _: &OpenWorkspaceSwitcher, window, cx| {
        open_workspace_switcher(workspace, window, cx);
    });
    workspace.register_action(|workspace, _: &lifecycle::AddProject, window, cx| {
        lifecycle::add_project(workspace, window, cx);
    });
    workspace.register_action(|workspace, _: &lifecycle::NewWorkspace, window, cx| {
        let project = lifecycle::window_project(workspace, cx);
        lifecycle::open_new_workspace(project, workspace, window, cx);
    });
}

/// The switcher's rows, one per workspace in the panel's order; the filter also matches the
/// name, title, issue title, branch, status and PR number.
pub fn switcher_items(panel: &WorkspacesPanel) -> Vec<palettes::PaletteItem> {
    panel
        .workspaces
        .iter()
        .map(|w| palettes::PaletteItem {
            label: w.display_title(),
            detail: w.status.as_deref().map(|status| panel.status_label(status)),
            keywords: [
                w.name.clone(),
                w.title.clone().unwrap_or_default(),
                w.subtitle().unwrap_or_default(),
                w.branch.clone().unwrap_or_default(),
                w.status.clone().unwrap_or_default(),
                w.pr.as_ref()
                    .map(|pr| format!("#{}", pr.number))
                    .unwrap_or_default(),
            ]
            .join(" "),
        })
        .collect()
}

/// ⌘J: a palette over every workspace; Enter opens it (or focuses its window).
pub fn open_workspace_switcher(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<WorkspacesPanel>(cx) else {
        return;
    };
    let (workspaces, items) = {
        let panel = panel.read(cx);
        (panel.workspaces.clone(), switcher_items(panel))
    };
    let panel = panel.downgrade();
    workspace.toggle_modal(window, cx, move |window, cx| {
        palettes::Palette::new(
            "Switch to a workspace…",
            items,
            move |index, window, cx| {
                let Some(path) = workspaces.get(index).map(|w| w.path.clone()) else {
                    return;
                };
                panel
                    .update(cx, |panel, cx| panel.open_workspace(path, window, cx))
                    .log_err();
            },
            window,
            cx,
        )
    });
}

/// The Icarus server this client talks to; `ICARUS_HOME` selects a non-default one.
fn server_home() -> Option<PathBuf> {
    icarus_client::default_home().log_err()
}

#[derive(Debug, Clone, PartialEq)]
pub enum Connection {
    Connecting,
    Connected { server_version: String },
    Unavailable(String),
}

struct Snapshot {
    workspaces: Vec<WorkspaceSummary>,
    statuses: Vec<StatusColumn>,
    view: cards::CardView,
    project_names: collections::HashMap<String, String>,
}

async fn fetch_snapshot(client: Arc<Client>) -> Result<Snapshot> {
    let get = |key: &'static str| {
        let client = client.clone();
        async move {
            client
                .request("config.get", serde_json::json!({ "key": key }))
                .await
        }
    };
    let workspaces = client
        .request("workspace.list", serde_json::json!({}))
        .await?;
    let statuses = get("workspace.statuses").await?;
    let card_fields = get("workspace.cardFields").await.unwrap_or_default();
    let sidebar = get("sidebar").await.unwrap_or_default();
    let rules = get("rules.orca_compat").await.unwrap_or_default();
    let projects = client
        .request("project.list", serde_json::json!({}))
        .await
        .unwrap_or_default();
    let project_names = projects
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|project| {
            Some((
                project["id"].as_str()?.to_string(),
                project["name"].as_str()?.to_string(),
            ))
        })
        .collect();
    Ok(Snapshot {
        workspaces: serde_json::from_value(workspaces)?,
        statuses: serde_json::from_value(statuses).unwrap_or_default(),
        view: cards::CardView::from_settings(&card_fields, &sidebar, &rules),
        project_names,
    })
}

pub struct WorkspacesPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    position: DockPosition,
    connection: Connection,
    client: Option<Arc<Client>>,
    workspaces: Vec<WorkspaceSummary>,
    statuses: Vec<StatusColumn>,
    status_order: Vec<String>,
    view: cards::CardView,
    project_names: collections::HashMap<String, String>,
    ports_requested: HashSet<String>,
    filter: Option<Entity<editor::Editor>>,
    filter_query: String,
    selection: cards::CardSelection,
    _filter_subscription: Option<Subscription>,
    _poll: Task<()>,
    _ports_observation: Subscription,
    remote: Entity<servers::RemoteServers>,
    _remote_observation: Subscription,
}

impl WorkspacesPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        cx.spawn(async move |cx| cx.update(|_window, cx| cx.new(|cx| Self::new(workspace, cx))))
    }

    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let mut panel = Self::preview(workspace, cx);
        if !enabled(cx) {
            return panel;
        }
        panel.view = cards::CardView::default();
        panel._poll = cx.spawn(async move |this, cx| {
            loop {
                if this.upgrade().is_none() {
                    return;
                }
                // The app's own connection: this machine, or the saved server picked in the Servers menu.
                let Some(client) = cx.update(|cx| app_client(cx)) else {
                    let unavailable = anyhow::anyhow!("not connected to the Icarus server");
                    if this
                        .update(cx, |panel, cx| panel.set_unavailable(unavailable, cx))
                        .is_err()
                    {
                        return;
                    }
                    cx.background_executor().timer(REFRESH_INTERVAL).await;
                    continue;
                };
                let snapshot = Tokio::spawn_result(cx, fetch_snapshot(client.clone())).await;
                let updated = this.update(cx, |panel, cx| match snapshot {
                    Ok(snapshot) => {
                        panel.connection = Connection::Connected {
                            server_version: client.server_version.clone(),
                        };
                        panel.client = Some(client);
                        panel.apply_snapshot(snapshot, cx);
                    }
                    Err(error) => panel.set_unavailable(error, cx),
                });
                if updated.is_err() {
                    return;
                }
                cx.background_executor().timer(REFRESH_INTERVAL).await;
            }
        });
        panel
    }

    /// A panel that never contacts a server, for previews and visual tests.
    pub fn preview(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let ports = PortsStore::global(cx);
        let watch = enabled(cx);
        let remote = servers::store(cx, watch);
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            position: DockPosition::Left,
            connection: Connection::Connecting,
            client: None,
            workspaces: Vec::new(),
            statuses: Vec::new(),
            status_order: Vec::new(),
            view: cards::CardView::showing_all(),
            project_names: collections::HashMap::default(),
            ports_requested: HashSet::default(),
            filter: None,
            filter_query: String::new(),
            selection: cards::CardSelection::default(),
            _filter_subscription: None,
            _poll: Task::ready(()),
            _ports_observation: cx.observe(&ports, |_, _, cx| cx.notify()),
            remote: remote.clone(),
            _remote_observation: cx.observe(&remote, |_, _, cx| cx.notify()),
        }
    }

    pub fn remote_servers(&self) -> &Entity<servers::RemoteServers> {
        &self.remote
    }

    fn set_unavailable(&mut self, error: anyhow::Error, cx: &mut Context<Self>) {
        self.client = None;
        self.ports_requested.clear();
        self.connection = Connection::Unavailable(format!("{error:#}"));
        cx.notify();
    }

    fn apply_snapshot(&mut self, mut snapshot: Snapshot, cx: &mut Context<Self>) {
        snapshot.view.local_fields = client_settings::card_fields(cx);
        if self.workspaces != snapshot.workspaces
            || self.statuses != snapshot.statuses
            || self.view != snapshot.view
            || self.project_names != snapshot.project_names
        {
            self.status_order = snapshot.statuses.iter().map(|s| s.id.clone()).collect();
            self.workspaces = snapshot.workspaces;
            self.statuses = snapshot.statuses;
            self.view = snapshot.view;
            self.project_names = snapshot.project_names;
            cx.notify();
        }
        self.fetch_missing_ports(cx);
    }

    pub fn view(&self) -> &cards::CardView {
        &self.view
    }

    /// For previews and tests; the live panel takes these from the server's settings.
    pub fn set_view(&mut self, view: cards::CardView, cx: &mut Context<Self>) {
        self.view = view;
        cx.notify();
    }

    pub fn set_project_names(
        &mut self,
        names: collections::HashMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        self.project_names = names;
        cx.notify();
    }

    /// The Icarus workspace a window with these folders shows.
    pub fn workspace_for_roots(&self, roots: &[PathBuf]) -> Option<&WorkspaceSummary> {
        self.workspaces
            .iter()
            .find(|w| roots.iter().any(|root| root.starts_with(&w.path)))
    }

    /// Writes a view setting to the server; the panel shows it at once.
    fn save_setting(&self, key: &'static str, value: serde_json::Value, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        Tokio::spawn_result(cx, async move {
            client
                .request(
                    "config.set",
                    serde_json::json!({ "key": key, "value": value }),
                )
                .await
        })
        .detach_and_log_err(cx);
    }

    /// Card fields are per app: toggling one saves this app's own list, starting from the server's.
    pub fn toggle_card_field(&mut self, field: &str, cx: &mut Context<Self>) {
        let fields = self.view.toggled(field);
        self.set_local_card_fields(Some(fields), cx);
    }

    pub fn set_local_card_fields(&mut self, fields: Option<Vec<String>>, cx: &mut Context<Self>) {
        self.view.local_fields = fields.clone();
        cx.notify();
        client_settings::set(
            client_settings::CARD_FIELDS,
            fields.map(|fields| serde_json::json!(fields)),
            cx,
        )
        .log_err();
    }

    pub fn set_group_by(&mut self, group_by: cards::GroupBy, cx: &mut Context<Self>) {
        self.view.group_by = group_by;
        cx.notify();
        self.save_setting("sidebar.groupBy", serde_json::json!(group_by.setting()), cx);
    }

    fn set_stack_ascending(&mut self, ascending: bool, cx: &mut Context<Self>) {
        self.view.stack_ascending = ascending;
        cx.notify();
        self.save_setting(
            "sidebar.stackOrder",
            serde_json::json!(if ascending { "asc" } else { "desc" }),
            cx,
        );
    }

    /// Loads each workspace's ports once; `ports.changed` events keep them current afterwards.
    fn fetch_missing_ports(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let missing: Vec<String> = self
            .workspaces
            .iter()
            .map(|workspace| workspace.id.clone())
            .filter(|id| !self.ports_requested.contains(id))
            .collect();
        if missing.is_empty() {
            return;
        }
        self.ports_requested.extend(missing.iter().cloned());
        let fetch = Tokio::spawn_result(cx, async move {
            let mut fetched = Vec::new();
            for workspace_id in missing {
                match links::fetch_ports(client.clone(), workspace_id.clone()).await {
                    Ok(ports) => fetched.push((workspace_id, ports)),
                    Err(error) => log::debug!("no ports for workspace {workspace_id}: {error:#}"),
                }
            }
            Ok(fetched)
        });
        cx.spawn(async move |_, cx| {
            let fetched = fetch.await?;
            cx.update(|cx| {
                PortsStore::global(cx).update(cx, |store, cx| {
                    for (workspace_id, ports) in fetched {
                        store.set_ports(workspace_id, ports, cx);
                    }
                })
            });
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn render_port_chips(
        &self,
        workspace: &WorkspaceSummary,
        cx: &App,
    ) -> Option<impl IntoElement> {
        let store = PortsStore::try_global(cx)?;
        let ports = store.read(cx).ports(&workspace.id);
        if ports.is_empty() {
            return None;
        }
        let chips = ports.iter().enumerate().map(|(index, port)| {
            let url = port.url.clone();
            let openable = port.is_openable();
            let label_color = if port.kind == links::PortKind::Frontend {
                Color::Accent
            } else {
                Color::Muted
            };
            ButtonLike::new(SharedString::from(format!(
                "icarus-port-{}-{index}",
                workspace.id
            )))
            .style(ButtonStyle::Outlined)
            .size(ButtonSize::Compact)
            .disabled(!openable)
            .tooltip(Tooltip::text(port.url.clone()))
            .on_click(move |_, _window, cx| cx.open_url(&url))
            .child(
                h_flex()
                    .px_1()
                    .gap_1()
                    .child(Indicator::dot().color(Color::Success))
                    .child(
                        Label::new(port.chip_label())
                            .size(LabelSize::XSmall)
                            .color(label_color),
                    ),
            )
        });
        Some(h_flex().flex_wrap().gap_1().pt_0p5().children(chips))
    }

    pub fn set_workspaces(&mut self, workspaces: Vec<WorkspaceSummary>, cx: &mut Context<Self>) {
        self.connection = Connection::Connected {
            server_version: "preview".into(),
        };
        self.workspaces = workspaces;
        cx.notify();
    }

    pub fn set_statuses(&mut self, statuses: Vec<StatusColumn>, cx: &mut Context<Self>) {
        self.status_order = statuses.iter().map(|s| s.id.clone()).collect();
        self.statuses = statuses;
        cx.notify();
    }

    pub fn set_status_order(&mut self, order: Vec<String>, cx: &mut Context<Self>) {
        self.status_order = order;
        cx.notify();
    }

    fn status_label(&self, id: &str) -> String {
        self.statuses
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.label.clone())
            .unwrap_or_else(|| id.to_string())
    }

    fn status_def(&self, id: &str) -> Option<&StatusColumn> {
        self.statuses.iter().find(|s| s.id == id)
    }

    fn status_icon(&self, id: &str, cx: &App) -> Icon {
        let status = self.status_def(id);
        status_style::status_icon(
            id,
            status.and_then(|s| s.color.as_deref()),
            status.and_then(|s| s.icon.as_deref()),
            cx,
        )
    }

    fn status_color(&self, id: &str, cx: &App) -> Color {
        status_style::status_color(id, self.status_def(id).and_then(|s| s.color.as_deref()), cx)
    }

    /// Changes one status's colour or icon in the settings, keeping every other field as it is.
    fn restyle_status(
        &mut self,
        status_id: String,
        field: &'static str,
        value: String,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if let Some(status) = self.statuses.iter_mut().find(|s| s.id == status_id) {
            match field {
                "color" => status.color = Some(value.clone()),
                _ => status.icon = Some(value.clone()),
            }
            cx.notify();
        }
        let request = Tokio::spawn_result(cx, async move {
            let mut statuses = client
                .request(
                    "config.get",
                    serde_json::json!({ "key": "workspace.statuses" }),
                )
                .await?;
            let entry = statuses
                .as_array_mut()
                .and_then(|items| items.iter_mut().find(|s| s["id"] == status_id.as_str()))
                .ok_or_else(|| anyhow::anyhow!("status {status_id} is not in the settings"))?;
            entry[field] = serde_json::Value::String(value);
            client
                .request(
                    "config.set",
                    serde_json::json!({ "key": "workspace.statuses", "value": statuses }),
                )
                .await
        });
        cx.spawn(async move |this, cx| {
            request.await?;
            this.update(cx, |panel, cx| panel.refresh(cx))
        })
        .detach_and_log_err(cx);
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let snapshot = Tokio::spawn_result(cx, fetch_snapshot(client)).await;
            this.update(cx, |panel, cx| match snapshot {
                Ok(snapshot) => panel.apply_snapshot(snapshot, cx),
                Err(error) => panel.set_unavailable(error, cx),
            })
        })
        .detach_and_log_err(cx);
    }

    fn set_status(&mut self, workspace_id: String, status: Option<String>, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if let Some(workspace) = self.workspaces.iter_mut().find(|w| w.id == workspace_id) {
            workspace.status = status.clone();
            cx.notify();
        }
        let request = Tokio::spawn_result(cx, async move {
            client
                .request(
                    "workspace.set",
                    serde_json::json!({ "id": workspace_id, "status": status.unwrap_or_default() }),
                )
                .await
        });
        cx.spawn(async move |this, cx| {
            request.await?;
            this.update(cx, |panel, cx| panel.refresh(cx))
        })
        .detach_and_log_err(cx);
    }

    fn open_workspace(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_paths(vec![path], OpenOptions::default(), None, window, cx)
                    .detach();
            })
            .log_err();
    }

    fn render_status_menu(
        &self,
        workspace: &WorkspaceSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let label = workspace
            .status
            .as_deref()
            .map(|id| self.status_label(id))
            .unwrap_or_else(|| "Set status".into());
        let current = workspace.status.clone();
        let workspace_id = workspace.id.clone();
        let statuses = self.statuses.clone();
        let panel = cx.entity().downgrade();
        let (chip_icon, chip_color) = match workspace.status.as_deref() {
            Some(id) => (self.status_icon(id, cx), self.status_color(id, cx)),
            None => (
                Icon::from_path(status_style::icon_path("", Some("circle-dashed")))
                    .size(IconSize::Small)
                    .color(Color::Muted),
                Color::Muted,
            ),
        };
        PopoverMenu::new(SharedString::from(format!(
            "icarus-status-{}",
            workspace.id
        )))
        .trigger(
            ButtonLike::new(SharedString::from(format!(
                "icarus-status-chip-{}",
                workspace.id
            )))
            .style(ButtonStyle::Subtle)
            .child(
                h_flex()
                    .gap_1()
                    .px_0p5()
                    .child(chip_icon)
                    .child(Label::new(label).size(LabelSize::XSmall).color(chip_color)),
            ),
        )
        .menu(move |window, cx| {
            let statuses = statuses.clone();
            let current = current.clone();
            let workspace_id = workspace_id.clone();
            let panel = panel.clone();
            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                menu = menu.header("Status");
                for status in &statuses {
                    let set_panel = panel.clone();
                    let workspace_id = workspace_id.clone();
                    let status_id = status.id.clone();
                    let render_status = status.clone();
                    let selected = current.as_deref() == Some(status.id.as_str());
                    menu = menu.custom_entry(
                        move |_window, cx| {
                            h_flex()
                                .w_full()
                                .gap_2()
                                .child(status_style::status_icon(
                                    &render_status.id,
                                    render_status.color.as_deref(),
                                    render_status.icon.as_deref(),
                                    cx,
                                ))
                                .child(Label::new(render_status.label.clone()))
                                .child(div().flex_1())
                                .when(selected, |row| {
                                    row.child(
                                        Icon::new(IconName::Check)
                                            .size(IconSize::Small)
                                            .color(Color::Muted),
                                    )
                                })
                                .into_any_element()
                        },
                        move |_, cx| {
                            set_panel
                                .update(cx, |panel, cx| {
                                    panel.set_status(
                                        workspace_id.clone(),
                                        Some(status_id.clone()),
                                        cx,
                                    )
                                })
                                .log_err();
                        },
                    );
                }
                if let Some(current_id) = current.clone() {
                    let color_panel = panel.clone();
                    let color_status = current_id.clone();
                    menu = menu.separator().submenu("Color", move |mut submenu, _, _| {
                        for color in status_style::STATUS_COLORS {
                            let restyle = color_panel.clone();
                            let status_id = color_status.clone();
                            submenu = submenu.custom_entry(
                                move |_window, _cx| {
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Icon::from_path(status_style::icon_path(
                                                "",
                                                Some("conductor-done"),
                                            ))
                                            .size(IconSize::Small)
                                            .color(status_style::swatch_color(color.id)),
                                        )
                                        .child(Label::new(color.label))
                                        .into_any_element()
                                },
                                move |_, cx| {
                                    restyle
                                        .update(cx, |panel, cx| {
                                            panel.restyle_status(
                                                status_id.clone(),
                                                "color",
                                                color.id.into(),
                                                cx,
                                            )
                                        })
                                        .log_err();
                                },
                            );
                        }
                        submenu
                    });
                    let icon_panel = panel.clone();
                    let icon_status = current_id;
                    menu = menu.submenu("Icon", move |mut submenu, _, _| {
                        for icon in status_style::STATUS_ICONS {
                            let restyle = icon_panel.clone();
                            let status_id = icon_status.clone();
                            submenu = submenu.custom_entry(
                                move |_window, _cx| {
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Icon::from_path(status_style::icon_path(
                                                "",
                                                Some(icon.id),
                                            ))
                                            .size(IconSize::Small),
                                        )
                                        .child(Label::new(icon.label))
                                        .into_any_element()
                                },
                                move |_, cx| {
                                    restyle
                                        .update(cx, |panel, cx| {
                                            panel.restyle_status(
                                                status_id.clone(),
                                                "icon",
                                                icon.id.into(),
                                                cx,
                                            )
                                        })
                                        .log_err();
                                },
                            );
                        }
                        submenu
                    });
                }
                let panel = panel.clone();
                menu.separator().entry("Clear status", None, move |_, cx| {
                    panel
                        .update(cx, |panel, cx| {
                            panel.set_status(workspace_id.clone(), None, cx)
                        })
                        .log_err();
                })
            }))
        })
    }

    fn project_groups(&self) -> Vec<(String, String, Vec<WorkspaceSummary>)> {
        // Why: empty projects get a group too, so their header menu can remove them.
        let mut projects: Vec<(String, String)> = self
            .project_names
            .iter()
            .map(|(id, name)| (id.clone(), name.clone()))
            .collect();
        for workspace in &self.workspaces {
            if !projects.iter().any(|(id, _)| id == &workspace.project_id) {
                projects.push((workspace.project_id.clone(), workspace.project_id.clone()));
            }
        }
        projects.sort_by(|a, b| {
            a.1.to_lowercase()
                .cmp(&b.1.to_lowercase())
                .then(a.0.cmp(&b.0))
        });
        let arranged = self.arranged_workspaces();
        projects
            .into_iter()
            .map(|(id, name)| {
                let mut workspaces: Vec<WorkspaceSummary> = arranged
                    .iter()
                    .filter(|workspace| workspace.project_id == id)
                    .cloned()
                    .collect();
                pinned_first(&mut workspaces);
                (id, name, workspaces)
            })
            .collect()
    }

    /// The list's sections for the chosen grouping: header icon, title, colour and cards.
    fn sections(&self, cx: &App) -> Vec<(Icon, String, Color, Vec<WorkspaceSummary>)> {
        let no_status_icon = || {
            Icon::from_path(status_style::icon_path("", Some("circle-dashed")))
                .size(IconSize::Small)
                .color(Color::Muted)
        };
        let workspaces = self.arranged_workspaces();
        let mut sections: Vec<(Icon, String, Color, Vec<WorkspaceSummary>)> =
            match self.view.group_by {
                cards::GroupBy::Status => group_by_status(&workspaces, &self.status_order)
                    .into_iter()
                    .map(|group| match group.status.as_deref() {
                        Some(id) => (
                            self.status_icon(id, cx),
                            self.status_label(id),
                            self.status_color(id, cx),
                            group.workspaces,
                        ),
                        None => (
                            no_status_icon(),
                            "No status".into(),
                            Color::Default,
                            group.workspaces,
                        ),
                    })
                    .collect(),
                cards::GroupBy::PrStatus => group_by_pr(&workspaces)
                    .into_iter()
                    .map(|(lane, workspaces)| {
                        let (icon, color) = match lane {
                            PrGroup::Done => (IconName::Check, Color::Success),
                            PrGroup::InReview => (IconName::Eye, Color::Accent),
                            PrGroup::InProgress => (IconName::PullRequest, Color::Muted),
                            PrGroup::Closed => (IconName::XCircle, Color::Muted),
                        };
                        (
                            Icon::new(icon).size(IconSize::Small).color(color),
                            lane.label().into(),
                            Color::Default,
                            workspaces,
                        )
                    })
                    .collect(),
                cards::GroupBy::Stack => {
                    // Why: a stack lane is ordered by stack position, so pins don't reorder it.
                    return group_by_stack(
                        &workspaces,
                        &self.status_order,
                        self.view.reviewing_status.as_deref(),
                        self.view.stack_ascending,
                    )
                    .into_iter()
                    .map(|mut group| {
                        if !matches!(group.lane, StackLane::Stack { .. }) {
                            pinned_first(&mut group.workspaces);
                        }
                        group
                    })
                    .map(|group| match group.lane {
                        StackLane::Reviewing => {
                            let id = self.view.reviewing_status.clone().unwrap_or_default();
                            (
                                self.status_icon(&id, cx),
                                self.status_label(&id),
                                self.status_color(&id, cx),
                                group.workspaces,
                            )
                        }
                        StackLane::Stack { number, .. } => (
                            Icon::new(IconName::ListTree)
                                .size(IconSize::Small)
                                .color(Color::Accent),
                            format!("Stack #{number}"),
                            Color::Default,
                            group.workspaces,
                        ),
                        StackLane::None => (
                            no_status_icon(),
                            "Not in a stack".into(),
                            Color::Default,
                            group.workspaces,
                        ),
                    })
                    .collect();
                }
                cards::GroupBy::Project => self
                    .project_groups()
                    .into_iter()
                    .map(|(_, name, workspaces)| {
                        (
                            Icon::new(IconName::Folder)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                            name,
                            Color::Default,
                            workspaces,
                        )
                    })
                    .collect(),
                cards::GroupBy::None => vec![(
                    Icon::new(IconName::GitBranch)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    "All workspaces".into(),
                    Color::Default,
                    workspaces,
                )],
            };
        for (_, _, _, workspaces) in &mut sections {
            pinned_first(workspaces);
        }
        sections
    }

    /// Grouping, stack order and which card fields show (plan §3a).
    fn render_view_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = self.view.clone();
        let panel = cx.entity().downgrade();
        PopoverMenu::new("icarus-workspaces-view")
            .trigger_with_tooltip(
                IconButton::new("icarus-workspaces-view-button", IconName::Settings)
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted),
                Tooltip::text("Group, sort and card display"),
            )
            .menu(move |window, cx| {
                let view = view.clone();
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.header("Group by");
                    for (group_by, label) in [
                        (cards::GroupBy::Status, "Status"),
                        (cards::GroupBy::PrStatus, "PR"),
                        (cards::GroupBy::Stack, "Stack"),
                        (cards::GroupBy::Project, "Project"),
                        (cards::GroupBy::None, "None"),
                    ] {
                        let panel = panel.clone();
                        menu = menu.toggleable_entry(
                            label,
                            view.group_by == group_by,
                            ui::IconPosition::Start,
                            Some(Box::new(cards::SetWorkspaceGrouping {
                                group_by: group_by.setting().into(),
                            })),
                            move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.set_group_by(group_by, cx))
                                    .log_err();
                            },
                        );
                    }
                    if view.group_by == cards::GroupBy::Stack {
                        let panel = panel.clone();
                        let ascending = view.stack_ascending;
                        menu = menu.toggleable_entry(
                            "Stack from main up",
                            ascending,
                            ui::IconPosition::Start,
                            None,
                            move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| {
                                        panel.set_stack_ascending(!ascending, cx)
                                    })
                                    .log_err();
                            },
                        );
                    }
                    menu = menu.separator().header("Sort by");
                    for (sort_by, label) in cards::SortBy::CHOICES {
                        let panel = panel.clone();
                        menu = menu.toggleable_entry(
                            label,
                            view.sort_by == sort_by,
                            ui::IconPosition::Start,
                            Some(Box::new(cards::SetWorkspaceSort {
                                sort_by: sort_by.setting().into(),
                            })),
                            move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.set_sort_by(sort_by, cx))
                                    .log_err();
                            },
                        );
                    }
                    menu = menu.separator().header(if view.local_fields.is_some() {
                        "Card (this app)"
                    } else {
                        "Card"
                    });
                    for field in cards::CARD_FIELDS {
                        let panel = panel.clone();
                        menu = menu.toggleable_entry(
                            field.label,
                            view.shows(field.id),
                            ui::IconPosition::Start,
                            Some(Box::new(cards::ToggleCardField {
                                field: field.id.into(),
                            })),
                            move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.toggle_card_field(field.id, cx))
                                    .log_err();
                            },
                        );
                    }
                    if view.local_fields.is_some() {
                        menu = menu.entry(
                            "Use Shared Card Fields",
                            Some(Box::new(cards::UseSharedCardFields)),
                            move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.set_local_card_fields(None, cx))
                                    .log_err();
                            },
                        );
                    }
                    menu
                }))
            })
    }

    fn render_pr_chips(&self, workspace: &WorkspaceSummary) -> Option<impl IntoElement> {
        let pr = workspace.pr.as_ref()?;
        let view = &self.view;
        let state_color = match pr.state.as_str() {
            "merged" => Color::Accent,
            "closed" => Color::Error,
            "draft" => Color::Muted,
            _ => Color::Success,
        };
        let checks_color = match pr.checks.as_deref() {
            Some("failure") => Some(Color::Error),
            Some("pending") => Some(Color::Warning),
            Some("success") => Some(Color::Success),
            _ => None,
        };
        let mut row = h_flex().gap_1p5();
        if view.shows("pr") {
            row = row.child(
                h_flex()
                    .gap_0p5()
                    .child(
                        Icon::new(IconName::PullRequest)
                            .size(IconSize::XSmall)
                            .color(state_color),
                    )
                    .child(
                        Label::new(format!("#{}", pr.number))
                            .size(LabelSize::XSmall)
                            .color(state_color),
                    )
                    .when(pr.in_merge_queue, |chip| {
                        chip.child(
                            Label::new("queued")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .children(checks_color.map(|color| Indicator::dot().color(color))),
            );
        }
        if view.shows("pr-diff") && (pr.additions > 0 || pr.deletions > 0) {
            row = row.child(
                h_flex()
                    .gap_0p5()
                    .child(
                        Label::new(format!("+{}", pr.additions))
                            .size(LabelSize::XSmall)
                            .color(Color::Created),
                    )
                    .child(
                        Label::new(format!("−{}", pr.deletions))
                            .size(LabelSize::XSmall)
                            .color(Color::Deleted),
                    ),
            );
        }
        if let (true, Some(stack)) = (view.shows("stack"), pr.stack) {
            row = row.child(
                Label::new(format!(
                    "{}/{}",
                    stack.position,
                    stack.size.max(stack.position)
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            );
        }
        Some(row)
    }

    fn render_issue(&self, workspace: &WorkspaceSummary) -> Option<impl IntoElement> {
        if !self.view.shows("linear-title") {
            return None;
        }
        let subtitle = workspace.subtitle();
        let labels = workspace.issue_labels();
        if subtitle.is_none() && labels.is_empty() {
            return None;
        }
        let pills = labels.into_iter().map(|label| {
            let color = parse_hex_color(&label.color);
            h_flex()
                .px_1()
                .gap_1()
                .rounded_full()
                .border_1()
                .border_color(color.map_or(gpui::transparent_black(), |c| c.opacity(0.5)))
                .child(
                    div()
                        .size(px(6.))
                        .rounded_full()
                        .when_some(color, |dot, color| dot.bg(color)),
                )
                .child(Label::new(label.name).size(LabelSize::XSmall))
        });
        Some(
            v_flex()
                .gap_0p5()
                .when_some(subtitle, |column, subtitle| {
                    column.child(
                        Label::new(subtitle)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                })
                .child(h_flex().flex_wrap().gap_1().children(pills)),
        )
    }

    fn render_card(
        &self,
        workspace: &WorkspaceSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let path = workspace.path.clone();
        let working = workspace.working_agents();
        let agent_count = workspace.agents.len();
        let view = self.view.clone();
        let project = view
            .shows("project")
            .then(|| self.project_names.get(&workspace.project_id).cloned())
            .flatten();
        let card = v_flex()
            .id(SharedString::from(format!(
                "icarus-workspace-{}",
                workspace.id
            )))
            .w_full()
            .px_2()
            .py_1p5()
            .gap_0p5()
            .rounded_md()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().element_hover));
        let key = cards::CardKey {
            server: routing::ServerRef::Primary,
            id: workspace.id.clone(),
        };
        let card = self
            .with_card_behavior(
                key,
                workspace.display_title(),
                card,
                move |panel, window, cx| panel.open_workspace(path.clone(), window, cx),
                cx,
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::GitBranch)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(workspace.display_title()).size(LabelSize::Default))
                    .when_some(
                        workspace.unread.as_ref().filter(|_| view.shows("unread")),
                        |row, unread| {
                            row.child(cards::render_unread_dot(&workspace.id, unread, cx))
                        },
                    )
                    .when(workspace.pinned, |row| {
                        row.child(
                            Icon::new(IconName::Pin)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .child(div().flex_1())
                    .when_some(
                        workspace.setup.as_ref().and_then(SetupSummary::badge),
                        |row, badge| row.child(render_setup_badge(&workspace.id, badge)),
                    )
                    .when(workspace.sleeping, |row| {
                        row.child(
                            Label::new("Asleep")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .when(view.shows("inline-agents") && agent_count > 0, |row| {
                        row.child(
                            Label::new(if working > 0 {
                                format!("{working} working")
                            } else {
                                format!("{agent_count} idle")
                            })
                            .size(LabelSize::XSmall)
                            .color(if working > 0 {
                                Color::Accent
                            } else {
                                Color::Muted
                            }),
                        )
                    })
                    .when_some(
                        workspace
                            .pr_author
                            .as_ref()
                            .filter(|_| view.shows("pr-author")),
                        |row, author| {
                            row.child(
                                div()
                                    .id(SharedString::from(format!(
                                        "icarus-author-{}",
                                        workspace.id
                                    )))
                                    .tooltip(Tooltip::text(format!("@{}", author.login)))
                                    .child(render_avatar(
                                        author.avatar_url.as_deref(),
                                        &author.login,
                                        AvatarKind::Person,
                                        px(16.),
                                        cx,
                                    )),
                            )
                        },
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .when(view.shows_status_chip(), |row| {
                        row.child(self.render_status_menu(workspace, cx))
                    })
                    .when_some(workspace.branch.clone(), |row, branch| {
                        row.child(
                            Label::new(branch)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .children(self.render_pr_chips(workspace))
                    .when_some(project, |row, project| {
                        row.child(
                            h_flex()
                                .px_1()
                                .rounded_sm()
                                .bg(cx.theme().colors().element_background)
                                .child(
                                    Label::new(project)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                    }),
            )
            .when(view.shows("ports"), |card| {
                card.children(self.render_port_chips(workspace, cx))
            })
            .children(self.render_issue(workspace));
        self.with_card_menu(routing::ServerRef::Primary, workspace, card, cx)
    }

    fn render_add_menu(&self) -> impl IntoElement {
        PopoverMenu::new("icarus-workspaces-add")
            .trigger_with_tooltip(
                IconButton::new("icarus-workspaces-add-button", IconName::Plus)
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted),
                Tooltip::text("New workspace or project"),
            )
            .menu(move |window, cx| {
                Some(ContextMenu::build(window, cx, |menu, _, _| {
                    menu.action("New Workspace…", lifecycle::NewWorkspace.boxed_clone())
                        .action("Add Project…", lifecycle::AddProject.boxed_clone())
                }))
            })
    }

    fn with_project_menu(
        &self,
        project_id: String,
        project_name: String,
        workspace_count: usize,
        header: impl IntoElement + 'static,
    ) -> impl IntoElement {
        let weak_workspace = self.workspace.clone();
        right_click_menu(SharedString::from(format!(
            "icarus-project-menu-{project_id}"
        )))
        .trigger(move |_, _, _| header)
        .menu(move |window, cx| {
            let project_id = project_id.clone();
            let project_name = project_name.clone();
            let weak_workspace = weak_workspace.clone();
            ContextMenu::build(window, cx, move |menu, _, _| {
                let new_in = (project_id.clone(), weak_workspace.clone());
                let remove = (project_id, project_name, weak_workspace);
                menu.entry("New Workspace…", None, move |window, cx| {
                    let (project_id, workspace) = new_in.clone();
                    // The menu is still being updated; open the dialog once it is done.
                    window.defer(cx, move |window, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                lifecycle::open_new_workspace(
                                    Some(project_id),
                                    workspace,
                                    window,
                                    cx,
                                )
                            })
                            .log_err();
                    });
                })
                .separator()
                .entry("Remove Project…", None, move |window, cx| {
                    let (project_id, project_name, workspace) = remove.clone();
                    lifecycle::remove_project(
                        routing::ServerRef::Primary,
                        project_id,
                        project_name,
                        workspace_count,
                        workspace,
                        window,
                        cx,
                    );
                })
            })
        })
    }

    fn with_card_menu(
        &self,
        server: routing::ServerRef,
        workspace: &WorkspaceSummary,
        card: impl IntoElement + 'static,
        cx: &App,
    ) -> impl IntoElement {
        let summary = workspace.clone();
        let weak_workspace = self.workspace.clone();
        let bulk = self.bulk_targets(
            &cards::CardKey {
                server: server.clone(),
                id: workspace.id.clone(),
            },
            cx,
        );
        right_click_menu(SharedString::from(format!(
            "icarus-card-menu-{}-{}",
            server.saved_name().unwrap_or_default(),
            workspace.id
        )))
        .trigger(move |_, _, _| card)
        .menu(move |window, cx| {
            if !bulk.is_empty() {
                return lifecycle::bulk_card_menu(bulk.clone(), weak_workspace.clone(), window, cx);
            }
            lifecycle::card_menu(
                server.clone(),
                summary.clone(),
                weak_workspace.clone(),
                window,
                cx,
            )
        })
    }
}

impl WorkspacesPanel {
    /// One section per saved server: its name and state, then a compact row per workspace.
    fn render_remote_server(
        &self,
        server: &servers::RemoteServer,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (state, state_color) = match &server.state {
            servers::RemoteState::Connecting => ("connecting…".to_owned(), Color::Muted),
            servers::RemoteState::Connected { .. } => {
                (server.workspaces.len().to_string(), Color::Muted)
            }
            servers::RemoteState::Unavailable(_) => ("offline".to_owned(), Color::Warning),
        };
        let mut section = v_flex().gap_0p5().child(
            h_flex()
                .px_2()
                .pt_1()
                .gap_1p5()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Icon::new(IconName::Server)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(server.name.clone())
                        .size(LabelSize::Small)
                        .weight(gpui::FontWeight::SEMIBOLD),
                )
                .child(
                    Label::new(server.address.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                )
                .child(div().flex_1())
                .child(Label::new(state).size(LabelSize::XSmall).color(state_color)),
        );
        if let servers::RemoteState::Unavailable(reason) = &server.state {
            section = section.child(
                div().px_2().child(
                    Label::new(reason.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            );
        }
        for (index, workspace) in self.arranged_remote(&server.workspaces).iter().enumerate() {
            let working = workspace.working_agents();
            let path = workspace.path.clone();
            let server_name = server.name.clone();
            let ssh = server.ssh_destination();
            let weak = cx.entity().downgrade();
            let status_menu = self.render_remote_status_menu(server, workspace, index);
            let row = v_flex()
                .id(SharedString::from(format!(
                    "icarus-remote-{}-{index}",
                    server.name
                )))
                .mx_1()
                .px_2()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(cx.theme().colors().element_hover))
                .child(
                    h_flex()
                        .gap_1()
                        .child(Label::new(workspace.display_title()).truncate())
                        .when(workspace.pinned, |row| {
                            row.child(
                                Icon::new(IconName::Pin)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                        })
                        .child(div().flex_1())
                        .when(working > 0, |row| {
                            row.child(
                                Label::new(format!("{working} working"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Accent),
                            )
                        }),
                )
                .child(
                    h_flex().gap_1().child(status_menu).child(
                        Label::new(
                            workspace
                                .branch
                                .as_ref()
                                .map(|branch| format!("· {branch}"))
                                .unwrap_or_default(),
                        )
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
                );
            let row = self.with_card_behavior(
                cards::CardKey {
                    server: routing::ServerRef::Saved(server.name.clone()),
                    id: workspace.id.clone(),
                },
                workspace.display_title(),
                row,
                move |_, window, cx| {
                    let (server_name, ssh, path, weak) =
                        (server_name.clone(), ssh.clone(), path.clone(), weak.clone());
                    // Deferred: opening may report back through this panel, which is being updated.
                    window.defer(cx, move |_, cx| {
                        open_remote_workspace(&server_name, ssh, path, weak, cx);
                    });
                },
                cx,
            );
            section = section.child(self.with_card_menu(
                routing::ServerRef::Saved(server.name.clone()),
                workspace,
                row,
                cx,
            ));
        }
        section
    }

    /// The status chip of a remote card: shows the server's label and moves the card on that server.
    fn render_remote_status_menu(
        &self,
        server: &servers::RemoteServer,
        workspace: &WorkspaceSummary,
        index: usize,
    ) -> impl IntoElement {
        let label = server.status_label(workspace.status.as_deref());
        let columns: Vec<(String, String)> = server
            .statuses
            .iter()
            .map(|column| (column.id.clone(), column.label.clone()))
            .collect();
        let current = workspace.status.clone();
        let store = self.remote.downgrade();
        let server_name = server.name.clone();
        let workspace_id = workspace.id.clone();
        ui::PopoverMenu::new(SharedString::from(format!(
            "icarus-remote-status-{}-{index}",
            server.name
        )))
        .menu(move |window, cx| {
            let columns = columns.clone();
            let current = current.clone();
            let store = store.clone();
            let server_name = server_name.clone();
            let workspace_id = workspace_id.clone();
            Some(ui::ContextMenu::build(window, cx, move |mut menu, _, _| {
                menu = menu.header("Move to");
                for (id, label) in columns {
                    let store = store.clone();
                    let server_name = server_name.clone();
                    let workspace_id = workspace_id.clone();
                    let checked = current.as_deref() == Some(id.as_str());
                    menu = menu.toggleable_entry(
                        label,
                        checked,
                        ui::IconPosition::Start,
                        None,
                        move |_, cx| {
                            store
                                .update(cx, |store, cx| {
                                    store.set_status(
                                        &server_name,
                                        workspace_id.clone(),
                                        Some(id.clone()),
                                        cx,
                                    )
                                })
                                .log_err();
                        },
                    );
                }
                menu
            }))
        })
        .trigger(
            ui::Button::new(
                SharedString::from(format!(
                    "icarus-remote-status-button-{}-{index}",
                    server.name
                )),
                label,
            )
            .label_size(LabelSize::XSmall)
            .color(Color::Muted),
        )
    }

    /// The servers this window watches: the local one and every saved remote.
    fn render_servers_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let own = routing::primary(cx).unwrap_or_else(|| "This machine".to_owned());
        let local = match &self.connection {
            Connection::Connected { server_version } => format!("{own} — {server_version}"),
            Connection::Connecting => format!("{own} — connecting…"),
            Connection::Unavailable(_) => format!("{own} — offline"),
        };
        let remote: Vec<(String, String)> = self
            .remote
            .read(cx)
            .servers()
            .iter()
            .map(|server| {
                let state = match &server.state {
                    servers::RemoteState::Connecting => "connecting…".to_owned(),
                    servers::RemoteState::Connected { server_version } => server_version.clone(),
                    servers::RemoteState::Unavailable(_) => "offline".to_owned(),
                };
                let ssh = server
                    .ssh_destination()
                    .map(|ssh| format!(", opens over SSH as {ssh}"))
                    .unwrap_or_default();
                (
                    server.name.clone(),
                    format!("{} — {state}{ssh}", server.address),
                )
            })
            .collect();
        let primary = routing::primary(cx);
        let workspace = self.workspace.clone();
        ui::PopoverMenu::new("icarus-servers-menu")
            .menu(move |window, cx| {
                let local = local.clone();
                let remote = remote.clone();
                let primary = primary.clone();
                let workspace = workspace.clone();
                Some(ui::ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.header("Servers").label(local);
                    for (name, detail) in &remote {
                        menu = menu.label(format!("{name}: {detail}"));
                    }
                    menu = menu.separator().header("This app connects to");
                    menu = menu.toggleable_entry(
                        "This machine",
                        primary.is_none(),
                        ui::IconPosition::Start,
                        None,
                        |_, cx| routing::set_primary(None, cx),
                    );
                    for (name, _) in &remote {
                        let pick = name.clone();
                        menu = menu.toggleable_entry(
                            name.clone(),
                            primary.as_deref() == Some(name.as_str()),
                            ui::IconPosition::Start,
                            None,
                            move |_, cx| routing::set_primary(Some(pick.clone()), cx),
                        );
                    }
                    menu.separator().entry("Add a server…", None, move |_, cx| {
                        show_add_server_help(workspace.clone(), cx);
                    })
                }))
            })
            .anchor(gpui::Anchor::TopRight)
            .trigger_with_tooltip(
                ui::IconButton::new("icarus-servers-button", IconName::Server)
                    .icon_size(IconSize::Small),
                ui::Tooltip::text("Servers"),
            )
    }
}

/// How a saved server's workspace opens here: over SSH as a Zed remote project when the server
/// has an SSH destination, else as a local folder when it's reachable (same machine, shared disk).
#[derive(Debug, PartialEq)]
enum RemoteOpen {
    Ssh(String),
    Local,
    Unreachable,
}

fn remote_open_plan(ssh: Option<&str>, folder_exists: bool) -> RemoteOpen {
    match ssh {
        Some(destination) => RemoteOpen::Ssh(destination.to_owned()),
        None if folder_exists => RemoteOpen::Local,
        None => RemoteOpen::Unreachable,
    }
}

pub(crate) fn open_remote_workspace(
    server: &str,
    ssh: Option<String>,
    path: PathBuf,
    panel: WeakEntity<WorkspacesPanel>,
    cx: &mut App,
) {
    let message = match remote_open_plan(ssh.as_deref(), path.is_dir()) {
        RemoteOpen::Local => {
            with_workspace_window(path, cx, |_, _, _| {});
            return;
        }
        RemoteOpen::Ssh(destination) => match open_over_ssh(&destination, path.clone(), cx) {
            Ok(()) => return,
            Err(error) => format!(
                "Couldn't open {} on {server} over SSH: {error:#}",
                path.display()
            ),
        },
        RemoteOpen::Unreachable => format!(
            "{} is on {server}, and this machine can't reach that folder. Save an SSH destination for it to open it here: `icarus remote add {server} <host:port> --code <code> --ssh <ssh host>`, or save it as `ssh://<host>`.",
            path.display()
        ),
    };
    panel
        .update(cx, |panel, cx| {
            panel
                .workspace
                .update(cx, |workspace, cx| {
                    struct RemoteWorkspace;
                    workspace.show_notification(
                        workspace::notifications::NotificationId::unique::<RemoteWorkspace>(),
                        cx,
                        |cx| {
                            cx.new(|cx| {
                                workspace::notifications::simple_message_notification::MessageNotification::new(message, cx)
                            })
                        },
                    );
                })
                .log_err();
        })
        .log_err();
}

/// Opens `path` on `destination` as a Zed remote project, reusing a window that already shows it.
/// Inside it Zed runs `icarus acp` and terminals on that host, so they reach that host's server.
fn open_over_ssh(destination: &str, path: PathBuf, cx: &mut App) -> Result<()> {
    let options = remote::SshConnectionOptions::parse_command_line(destination)?;
    let app_state = AppState::try_global(cx)
        .ok_or_else(|| anyhow::anyhow!("the app isn't ready to open windows"))?;
    cx.spawn(async move |cx| {
        recent_projects::open_remote_project(
            remote::RemoteConnectionOptions::Ssh(options),
            vec![path],
            app_state,
            OpenOptions::default(),
            cx,
        )
        .await
        .map(|_| ())
    })
    .detach_and_log_err(cx);
    Ok(())
}

fn show_add_server_help(workspace: WeakEntity<Workspace>, cx: &mut App) {
    let command = "icarus remote add <name> <host:port> --code <code from `icarus server pair` on that machine>";
    workspace
        .update(cx, |workspace, cx| {
            struct AddServer;
            workspace.show_notification(
                workspace::notifications::NotificationId::unique::<AddServer>(),
                cx,
                |cx| {
                    cx.new(|cx| {
                        workspace::notifications::simple_message_notification::MessageNotification::new(
                            "Pair another machine: run `icarus server start --listen <tailscale-ip>:<port>` and `icarus server pair` there, then this here. It shows up in this list within a few seconds.",
                            cx,
                        )
                        .with_title(command)
                        .copy_text(command)
                    })
                },
            );
        })
        .log_err();
}

impl Render for WorkspacesPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let filter = self.render_filter(window, cx);
        // Same height as the centre's tab bar, so the two bottom borders line up across the window.
        let header = h_flex()
            .flex_none()
            .h(ui::Tab::container_height(cx))
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new("Workspaces")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(self.render_add_menu())
            .child(self.render_servers_menu(cx))
            .child(self.render_view_menu(cx))
            .child(match &self.connection {
                Connection::Connecting => Label::new("connecting…")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                Connection::Connected { .. } => Label::new(self.workspaces.len().to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                Connection::Unavailable(_) => Label::new("server offline")
                    .size(LabelSize::XSmall)
                    .color(Color::Warning),
            });
        let mut body = v_flex()
            .id("icarus-workspaces")
            .size_full()
            .overflow_y_scroll()
            .p_1()
            .gap_2();
        if let Connection::Unavailable(reason) = &self.connection {
            body = body.child(
                v_flex()
                    .p_2()
                    .gap_1()
                    .child(Label::new("The Icarus server isn't running.").size(LabelSize::Small))
                    .child(
                        Label::new("Start it with `icarus server start`.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(reason.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            );
        }
        let project_ids: Vec<String> = if self.view.group_by == cards::GroupBy::Project {
            self.project_groups()
                .into_iter()
                .map(|(id, _, _)| id)
                .collect()
        } else {
            Vec::new()
        };
        for (index, (header_icon, title, header_color, workspaces)) in
            self.sections(cx).into_iter().enumerate()
        {
            let header = h_flex()
                .id(SharedString::from(format!("icarus-section-{index}")))
                .px_2()
                .gap_1p5()
                .child(header_icon)
                .child(
                    Label::new(title.clone())
                        .size(LabelSize::Small)
                        .color(header_color)
                        .weight(gpui::FontWeight::SEMIBOLD),
                )
                .child(
                    Label::new(workspaces.len().to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                );
            let header = match project_ids.get(index).cloned() {
                Some(project_id) => self
                    .with_project_menu(project_id, title, workspaces.len(), header)
                    .into_any_element(),
                None => header.into_any_element(),
            };
            let mut section = v_flex().gap_0p5().child(header);
            for workspace in &workspaces {
                section = section.child(self.render_card(workspace, cx));
            }
            body = body.child(section);
        }
        for server in self.remote.read(cx).servers().to_vec() {
            body = body.child(self.render_remote_server(&server, cx));
        }
        v_flex()
            .key_context("IcarusWorkspacesPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(header)
            .child(filter)
            .child(body)
    }
}

impl Focusable for WorkspacesPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WorkspacesPanel {}

impl Panel for WorkspacesPanel {
    fn persistent_name() -> &'static str {
        "IcarusWorkspacesPanel"
    }

    fn panel_key() -> &'static str {
        "IcarusWorkspacesPanel"
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
        Some("Icarus Workspaces")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        ToggleWorkspacesPanel.boxed_clone()
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        true
    }

    fn activation_priority(&self) -> u32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_workspaces_open_over_ssh_first_then_locally() {
        assert_eq!(
            remote_open_plan(Some("devbox"), true),
            RemoteOpen::Ssh("devbox".into()),
            "a folder with the same path here may be a different checkout"
        );
        assert_eq!(remote_open_plan(None, true), RemoteOpen::Local);
        assert_eq!(remote_open_plan(None, false), RemoteOpen::Unreachable);
    }

    pub(crate) fn summary(name: &str, path: &str, status: Option<&str>) -> WorkspaceSummary {
        WorkspaceSummary {
            id: format!("ws-{name}"),
            project_id: "p".into(),
            name: name.into(),
            path: PathBuf::from(path),
            branch: Some(name.into()),
            status: status.map(str::to_string),
            title: None,
            issue: None,
            agents: Vec::new(),
            pr_author: None,
            pr: None,
            setup: None,
            sleeping: false,
            pinned: false,
            created_at_ms: 0,
            manual_order: None,
            activity_at_ms: None,
            unread: None,
        }
    }

    fn stacked(name: &str, stack: u64, position: u32) -> WorkspaceSummary {
        let mut w = summary(name, &format!("/w/{name}"), None);
        w.pr = Some(PrSummary {
            number: u64::from(position),
            state: "open".into(),
            stack: Some(StackPlace {
                number: stack,
                position,
                size: 2,
            }),
            ..PrSummary::default()
        });
        w
    }

    /// A window on `/project` with the workspaces panel (preview mode, no server) and its actions.
    pub(crate) async fn panel_window(
        workspaces: Vec<WorkspaceSummary>,
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<WorkspacesPanel>,
        gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            // The palettes' query box is an editor.
            editor::init(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({ "a.txt": "" }))
            .await;
        let project = project::Project::test(fs, [Path::new("/project")], cx).await;
        let multi_workspace = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let mut cx = gpui::VisualTestContext::from_window(multi_workspace.into(), cx);
        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            register_panel_actions(workspace);
            let panel = cx.new(|cx| WorkspacesPanel::preview(workspace.weak_handle(), cx));
            panel.update(cx, |panel, cx| panel.set_workspaces(workspaces, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        cx.run_until_parked();
        (workspace, panel, cx)
    }

    fn section_titles(
        panel: &Entity<WorkspacesPanel>,
        cx: &mut gpui::VisualTestContext,
    ) -> Vec<String> {
        panel.read_with(cx, |panel, cx| {
            panel
                .sections(cx)
                .into_iter()
                .map(|(_, title, _, items)| {
                    format!(
                        "{title}: {}",
                        items
                            .iter()
                            .map(|w| w.name.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                })
                .collect()
        })
    }

    #[gpui::test]
    async fn card_fields_and_grouping_follow_their_actions(cx: &mut gpui::TestAppContext) {
        let (_, panel, mut cx) = panel_window(
            vec![
                summary("loose", "/w/loose", None),
                stacked("bottom", 7, 1),
                stacked("top", 7, 2),
                stacked("newer", 9, 1),
            ],
            cx,
        )
        .await;
        assert!(panel.read_with(&cx, |panel, _| panel.view().shows("pr")));
        cx.dispatch_action(cards::ToggleCardField { field: "pr".into() });
        assert!(!panel.read_with(&cx, |panel, _| panel.view().shows("pr")));
        let local = cx.update(|_, cx| client_settings::card_fields(cx));
        assert!(
            local.as_ref().is_some_and(
                |fields| !fields.is_empty() && !fields.iter().any(|field| field == "pr")
            ),
            "the toggle starts this app's own list from the shown fields: {local:?}"
        );
        assert!(
            panel.read_with(&cx, |panel, _| panel
                .view()
                .fields
                .iter()
                .any(|f| f == "pr")),
            "the server's fields stay as they were"
        );
        cx.dispatch_action(cards::ToggleCardField { field: "pr".into() });
        assert!(panel.read_with(&cx, |panel, _| panel.view().shows("pr")));
        cx.dispatch_action(cards::ToggleCardField {
            field: "ports".into(),
        });
        cx.dispatch_action(cards::UseSharedCardFields);
        assert_eq!(cx.update(|_, cx| client_settings::card_fields(cx)), None);
        assert!(
            panel.read_with(&cx, |panel, _| panel.view().local_fields.is_none()
                && panel.view().shows("ports")),
            "back on the server's fields"
        );

        cx.dispatch_action(cards::SetWorkspaceGrouping {
            group_by: "stack".into(),
        });
        assert_eq!(
            section_titles(&panel, &mut cx),
            [
                "Stack #9: newer",
                "Stack #7: top,bottom",
                "Not in a stack: loose"
            ]
        );
        cx.dispatch_action(cards::ToggleGroupByStack);
        assert_eq!(
            panel.read_with(&cx, |panel, _| panel.view().group_by),
            cards::GroupBy::Status
        );
        assert_eq!(
            section_titles(&panel, &mut cx),
            ["No status: loose,bottom,top,newer"]
        );
        cx.dispatch_action(cards::ToggleGroupByStack);
        assert_eq!(
            panel.read_with(&cx, |panel, _| panel.view().group_by),
            cards::GroupBy::Stack
        );
    }

    #[gpui::test]
    async fn open_linear_issue_opens_the_windows_issue(cx: &mut gpui::TestAppContext) {
        let mut linked = summary("abc-12", "/project", None);
        linked.issue = Some(serde_json::json!({
            "provider": "linear",
            "id": "ABC-12",
            "url": "https://linear.app/acme/issue/ABC-12",
            "labels": [{ "name": "Bug", "color": "#eb5757" }],
        }));
        let (_, panel, mut cx) = panel_window(vec![linked], cx).await;
        cx.dispatch_action(cards::OpenLinearIssue);
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://linear.app/acme/issue/ABC-12")
        );
        let labels = panel.read_with(&cx, |panel, _| panel.workspaces[0].issue_labels());
        assert_eq!(labels[0].name, "Bug");
        assert!(parse_hex_color(&labels[0].color).is_some());
        assert!(parse_hex_color("not-a-colour").is_none());
    }

    #[gpui::test]
    async fn the_switcher_filters_workspaces_and_enter_picks_one(cx: &mut gpui::TestAppContext) {
        let mut issue = summary("abc-12", "/w/abc-12", Some("todo"));
        issue.issue =
            Some(serde_json::json!({ "provider": "linear", "id": "ABC-12", "title": "Fix login" }));
        let (workspace, _, mut cx) = panel_window(
            vec![
                summary("main", "/w/main", None),
                issue,
                summary("docs", "/w/docs", None),
            ],
            cx,
        )
        .await;
        cx.dispatch_action(OpenWorkspaceSwitcher);
        cx.run_until_parked();
        let palette = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.active_modal::<palettes::Palette>(cx)
            })
            .expect("the switcher is open");
        let shown = |cx: &mut gpui::VisualTestContext| {
            palette.read_with(cx, |palette, cx| {
                palette.picker().read(cx).delegate.visible()
            })
        };
        assert_eq!(shown(&mut cx), [0, 1, 2]);
        cx.simulate_input("login");
        cx.run_until_parked();
        assert_eq!(shown(&mut cx), [1], "the issue title matches");
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert!(
            workspace.read_with(&cx, |workspace, cx| workspace
                .active_modal::<palettes::Palette>(cx)
                .is_none()),
            "Enter closes the switcher"
        );
    }

    #[gpui::test]
    async fn a_palette_confirms_the_original_index_of_the_filtered_row(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        let picked = std::rc::Rc::new(std::cell::Cell::new(None));
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let picked = picked.clone();
            workspace.toggle_modal(window, cx, move |window, cx| {
                palettes::Palette::new(
                    "Run a quick command…",
                    ["Run", "Local Backend", "Preview"]
                        .iter()
                        .map(|label| palettes::PaletteItem {
                            label: label.to_string(),
                            detail: None,
                            keywords: String::new(),
                        })
                        .collect(),
                    move |index, _, _| picked.set(Some(index)),
                    window,
                    cx,
                )
            });
        });
        cx.simulate_input("prev");
        cx.run_until_parked();
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(picked.get(), Some(2));
    }

    #[test]
    fn parses_composer_attach_payload() {
        let payload = serde_json::json!({
            "workspace_path": "/w/feature",
            "attachment": { "id": "att_1", "kind": "comment", "title": "Fix the retry loop" },
        });
        let request: ComposerAttachRequest =
            serde_json::from_value(payload).expect("valid composer.attach payload");
        assert_eq!(
            request,
            ComposerAttachRequest {
                workspace_path: PathBuf::from("/w/feature"),
                attachment: ComposerAttachment {
                    id: "att_1".into(),
                    kind: "comment".into(),
                    title: "Fix the retry loop".into(),
                },
            }
        );
    }

    #[test]
    fn rejects_composer_attach_payload_without_attachment() {
        let payload = serde_json::json!({ "workspace_path": "/w/feature" });
        assert!(serde_json::from_value::<ComposerAttachRequest>(payload).is_err());
    }
}
