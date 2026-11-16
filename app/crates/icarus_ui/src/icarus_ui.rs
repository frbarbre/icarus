pub mod browser;
pub mod changes_tab;
pub mod diff_tab;
pub mod github_panel;
pub mod layout;
pub mod links;
mod model;
mod next_action;
pub mod review;
pub mod sidebar;
pub mod status_style;
mod terminals;
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
use ui::{ButtonLike, ContextMenu, Indicator, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::dock::{DockPosition, Panel, PanelEvent, PanelSizeState};
use workspace::{AppState, MultiWorkspace, OpenOptions, Workspace};

use crate::github_panel::{AvatarKind, render_avatar};

pub use model::{
    AgentSummary, PrAuthor, StatusColumn, StatusGroup, WorkspaceSummary, group_by_status,
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
    ]
);

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
            workspace.update(cx, |workspace, cx| {
                layout::apply_layout(&pending.layout, &pending.path, workspace, window, cx);
                layout_applied(workspace, window, cx);
            });
        })?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
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

/// The app-wide connection to the server and the Icarus workspace paths it last listed.
#[derive(Default)]
pub(crate) struct AppConnection {
    pub(crate) client: Option<Arc<Client>>,
    pub(crate) workspace_paths: Vec<PathBuf>,
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
            let connect = Tokio::spawn_result(cx, async move {
                let home = server_home().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
                Client::connect(&home, "icarus-app").await
            });
            let Ok((client, mut events)) = connect.await else {
                cx.background_executor().timer(REFRESH_INTERVAL * 2).await;
                continue;
            };
            cx.update(|cx| cx.default_global::<AppConnection>().client = Some(client.clone()));
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
            loop {
                let next = Tokio::spawn(cx, async move {
                    let event = events.recv().await;
                    (events, event)
                })
                .await;
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
                    "client.dispatch" => {
                        let Some(request) =
                            serde_json::from_value::<DispatchRequest>(event.payload).log_err()
                        else {
                            continue;
                        };
                        cx.update(|cx| dispatch_action(request, cx));
                    }
                    // Notes render in the same diff-tab layer, so one reload covers both.
                    "review.changed" | "note.changed" => cx.update(review::review_changed),
                    "ports.changed" => {
                        cx.update(|cx| links::ports_changed(event.payload, cx));
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

pub fn init(cx: &mut App) {
    cx.set_global(IcarusEnabled);
    start_app_connection(cx);
    next_action::register(cx);
    links::register(cx);
    worktree_trust::register(cx);
    github_panel::register(cx);
    browser::register(cx);
    changes_tab::register(cx);
    diff_tab::register(cx);
    sidebar::register(cx);
    zed_import::register(cx);
    terminals::register(cx);
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
    })
    .detach();
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
}

async fn fetch_snapshot(client: Arc<Client>) -> Result<Snapshot> {
    let workspaces = client
        .request("workspace.list", serde_json::json!({}))
        .await?;
    let statuses = client
        .request(
            "config.get",
            serde_json::json!({ "key": "workspace.statuses" }),
        )
        .await?;
    Ok(Snapshot {
        workspaces: serde_json::from_value(workspaces)?,
        statuses: serde_json::from_value(statuses).unwrap_or_default(),
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
    ports_requested: HashSet<String>,
    _poll: Task<()>,
    _ports_observation: Subscription,
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
        panel._poll = cx.spawn(async move |this, cx| {
            loop {
                let client = match this.read_with(cx, |panel, _| panel.client.clone()) {
                    Ok(client) => client,
                    Err(_) => return,
                };
                let client = match client {
                    Some(client) => client,
                    None => {
                        let connect = Tokio::spawn_result(cx, async move {
                            let home = server_home()
                                .ok_or_else(|| anyhow::anyhow!("no home directory"))?;
                            let (client, _events) = Client::connect(&home, "icarus-panel").await?;
                            Ok(client)
                        });
                        match connect.await {
                            Ok(client) => client,
                            Err(error) => {
                                if this
                                    .update(cx, |panel, cx| panel.set_unavailable(error, cx))
                                    .is_err()
                                {
                                    return;
                                }
                                cx.background_executor().timer(REFRESH_INTERVAL).await;
                                continue;
                            }
                        }
                    }
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
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            position: DockPosition::Left,
            connection: Connection::Connecting,
            client: None,
            workspaces: Vec::new(),
            statuses: Vec::new(),
            status_order: Vec::new(),
            ports_requested: HashSet::default(),
            _poll: Task::ready(()),
            _ports_observation: cx.observe(&ports, |_, _, cx| cx.notify()),
        }
    }

    fn set_unavailable(&mut self, error: anyhow::Error, cx: &mut Context<Self>) {
        self.client = None;
        self.ports_requested.clear();
        self.connection = Connection::Unavailable(format!("{error:#}"));
        cx.notify();
    }

    fn apply_snapshot(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        if self.workspaces != snapshot.workspaces || self.statuses != snapshot.statuses {
            self.status_order = snapshot.statuses.iter().map(|s| s.id.clone()).collect();
            self.workspaces = snapshot.workspaces;
            self.statuses = snapshot.statuses;
            cx.notify();
        }
        self.fetch_missing_ports(cx);
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

    fn render_card(
        &self,
        workspace: &WorkspaceSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let path = workspace.path.clone();
        let working = workspace.working_agents();
        let agent_count = workspace.agents.len();
        v_flex()
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
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .on_click(cx.listener(move |panel, _, window, cx| {
                panel.open_workspace(path.clone(), window, cx);
            }))
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::GitBranch)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(workspace.display_title()).size(LabelSize::Default))
                    .child(div().flex_1())
                    .when(agent_count > 0, |row| {
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
                    .when_some(workspace.pr_author.as_ref(), |row, author| {
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
                    }),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(self.render_status_menu(workspace, cx))
                    .when_some(workspace.branch.clone(), |row, branch| {
                        row.child(
                            Label::new(branch)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .children(self.render_port_chips(workspace, cx))
            .when_some(workspace.subtitle(), |card, subtitle| {
                card.child(
                    Label::new(subtitle)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                )
            })
    }
}

impl Render for WorkspacesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let groups = group_by_status(&self.workspaces, &self.status_order);
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
        for group in groups {
            let title = group
                .status
                .as_deref()
                .map(|id| self.status_label(id))
                .unwrap_or_else(|| "No status".into());
            let (header_icon, header_color) = match group.status.as_deref() {
                Some(id) => (self.status_icon(id, cx), self.status_color(id, cx)),
                None => (
                    Icon::from_path(status_style::icon_path("", Some("circle-dashed")))
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    Color::Default,
                ),
            };
            let mut section = v_flex().gap_0p5().child(
                h_flex()
                    .px_2()
                    .gap_1p5()
                    .child(header_icon)
                    .child(
                        Label::new(title)
                            .size(LabelSize::Small)
                            .color(header_color)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(group.workspaces.len().to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            );
            for workspace in &group.workspaces {
                section = section.child(self.render_card(workspace, cx));
            }
            body = body.child(section);
        }
        v_flex()
            .key_context("IcarusWorkspacesPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(header)
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
