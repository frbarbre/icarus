pub mod accounts;
pub mod browser;
pub mod cards;
pub mod changes_tab;
pub mod client_settings;
pub mod comment_input;
pub mod commit_modal;
pub mod diff_tab;
mod driver;
mod focus;
pub mod github_panel;
pub mod history;
pub mod layout;
pub mod lifecycle;
pub mod links;
pub mod media;
mod model;
pub mod new_tab_menu;
mod next_action;
pub mod notifications;
pub mod palettes;
pub mod pane_nav;
#[cfg(test)]
mod perf_tests;
pub mod pr_comments;
pub mod quick_commands;
pub mod remote_agent;
pub mod remote_control;
pub mod review;
pub mod routing;
pub mod sections;
pub mod servers;
pub mod session_history;
pub mod settings_view;
pub mod sidebar;
pub mod source_control_settings;
mod ssh_install;
pub mod status_style;
pub(crate) mod terminals;
mod worktree_inbox;
mod worktree_trust;
pub mod zed_import;

pub use changes_tab::SourceControlPanel;
pub use diff_tab::DiffTab;
pub use github_panel::GitHubPanel;
pub use links::{PortEntry, PortsStore, PreviewDeployment, WorkspaceLinks};
pub use next_action::{NextAction, NextActionButton};
pub use session_history::SessionHistoryPanel;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use collections::HashSet;
use futures::FutureExt as _;
use futures::future::Shared;
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
/// How often a window that isn't in front re-reads the workspaces; coming forward catches up within one tick.
const BACKGROUND_INTERVAL: Duration = Duration::from_secs(30);
/// A server round trip slower than this is logged; a local socket answers in a few milliseconds.
pub(crate) const SLOW_REQUEST: Duration = Duration::from_millis(20);

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
            bool,
            &mut Workspace,
            &mut Window,
            &mut Context<Workspace>,
        ) + 'static,
>;

struct GlobalOpenAgentHandler(OpenAgentHandler);

impl gpui::Global for GlobalOpenAgentHandler {}

/// The app crate owns agent tabs; this crate asks it to open one, focused or not.
pub fn set_open_agent_handler(
    cx: &mut App,
    handler: impl Fn(
        Option<String>,
        Option<Entity<workspace::Pane>>,
        bool,
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
    request_agent_tab(agent_id, pane, true, workspace, window, cx);
}

fn request_agent_tab(
    agent_id: Option<String>,
    pane: Option<Entity<workspace::Pane>>,
    focus: bool,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match cx
        .try_global::<GlobalOpenAgentHandler>()
        .map(|handler| handler.0.clone())
    {
        Some(handler) => handler(agent_id, pane, focus, workspace, window, cx),
        None => log::error!("no open-agent handler registered"),
    }
}

/// Ends the server session of an agent whose tab was closed, as Orca does; history can resume it.
pub fn end_closed_agent(agent_id: String, cx: &mut App) {
    let client = routing::client_for_agent(&agent_id, cx);
    if let Some(kill) = review::request(
        client,
        "agent.kill",
        serde_json::json!({ "id": agent_id, "remove": true }),
        cx,
    ) {
        kill.detach_and_log_err(cx);
    }
}

/// Opens tabs, without taking focus, for the agents the server started in the workspace at `path`
/// that no tab shows yet: review and automation agents, or ones started from the CLI.
pub(crate) fn open_offered_agent_tabs(path: PathBuf, cx: &mut App) {
    let Some(client) = app_client(cx) else {
        return;
    };
    cx.spawn(async move |cx| {
        let key = path.to_string_lossy().into_owned();
        let taken = Tokio::spawn_result(cx, async move {
            client
                .request(
                    "workspace.take_agent_tabs",
                    serde_json::json!({ "id": key }),
                )
                .await
        })
        .await?;
        let agents: Vec<String> = serde_json::from_value(taken["agents"].clone())?;
        if agents.is_empty() {
            return anyhow::Ok(());
        }
        cx.update(|cx| {
            let Some((window, workspace)) = window_for_path(&path, cx) else {
                return anyhow::Ok(());
            };
            window.update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    for agent_id in agents {
                        request_agent_tab(Some(agent_id), None, false, workspace, window, cx);
                    }
                })
            })
        })?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// Shows the workspace's window (opening it if needed) and arranges its tabs as `pending` says.
fn open_layout(pending: layout::PendingLayout, cx: &mut App) {
    let path = pending.path.clone();
    with_workspace_window(path, cx, move |workspace, window, cx| {
        layout::apply_layout(&pending.layout, &pending.path, workspace, window, cx);
        layout_applied(workspace, window, cx);
    });
}

/// A workspace in a window, shown as soon as it exists; `loaded` ends once its saved tabs are restored.
#[derive(Clone)]
struct OpenedWorkspace {
    window: WindowHandle<MultiWorkspace>,
    workspace: Entity<Workspace>,
    loaded: Shared<Task<()>>,
}

type OpeningWorkspace = Shared<Task<Option<OpenedWorkspace>>>;

#[derive(Default)]
struct OpeningWorkspaces {
    /// Opens still in flight, so a second request for the same workspace waits instead of opening it twice.
    opening: collections::HashMap<PathBuf, OpeningWorkspace>,
    /// The workspace asked for last; an older open that finishes later doesn't take the window.
    latest: Option<PathBuf>,
}

impl gpui::Global for OpeningWorkspaces {}

/// Shows the Icarus workspace at `path`: the window holding it comes to the front, else the front
/// window switches to it (Orca's one window whose centre swaps per workspace).
pub fn show_workspace(path: PathBuf, cx: &mut App) {
    with_workspace_window(path, cx, |_, _, _| {});
}

/// Opens a workspace's sidebar on Source Control, unless its saved layout already chose a panel.
pub fn open_source_control_first(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<SourceControlPanel>(cx) else {
        return;
    };
    let dock = workspace
        .dock_at_position(panel.read(cx).position(window, cx))
        .clone();
    if dock.read(cx).restored_saved_layout() {
        return;
    }
    dock.update(cx, |dock, cx| {
        if let Some(index) = dock.panel_index_for_type::<SourceControlPanel>() {
            dock.activate_panel(index, window, cx);
            dock.set_open(true, window, cx);
        }
    });
}

/// Brings the window showing `path` to the front (opening one if needed), then runs `then` in its workspace.
pub(crate) fn with_workspace_window(
    path: PathBuf,
    cx: &mut App,
    then: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    // Deferred: callers often run inside a window's update, and that window can't be read until it ends.
    cx.defer(move |cx| show_workspace_window(path, cx, then));
}

fn show_workspace_window(
    path: PathBuf,
    cx: &mut App,
    then: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    cx.default_global::<OpeningWorkspaces>().latest = Some(path.clone());
    let window = match window_for_path(&path, cx) {
        Some((window, workspace)) => Task::ready(Some(OpenedWorkspace {
            window,
            workspace,
            loaded: Task::ready(()).shared(),
        }))
        .shared(),
        None => match cx.default_global::<OpeningWorkspaces>().opening.get(&path) {
            Some(opening) => opening.clone(),
            None => {
                let Some(app_state) = AppState::try_global(cx) else {
                    log::error!("no app state to open {} with", path.display());
                    return;
                };
                worktree_trust::add_icarus_workspace_path(&path, cx);
                let opening = open_local_workspace(path.clone(), app_state, cx);
                cx.default_global::<OpeningWorkspaces>()
                    .opening
                    .insert(path.clone(), opening.clone());
                opening
            }
        },
    };
    cx.spawn(async move |cx| {
        let Some(opened) = window.await else {
            return anyhow::Ok(());
        };
        let latest =
            cx.update(|cx| cx.default_global::<OpeningWorkspaces>().latest.as_ref() == Some(&path));
        opened.window.update(cx, |multi_workspace, window, cx| {
            if latest {
                multi_workspace.activate(opened.workspace.clone(), None, window, cx);
                window.activate_window();
            } else {
                multi_workspace.add(opened.workspace.clone(), window, cx);
            }
        })?;
        // Layouts wait for the saved tabs, which would otherwise be restored on top of them.
        opened.loaded.await;
        opened.window.update(cx, |_, window, cx| {
            opened
                .workspace
                .update(cx, |workspace, cx| then(workspace, window, cx))
        })?;
        cx.update(|cx| open_offered_agent_tabs(path, cx));
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// Opens `path` as a new workspace of the front window, resolving as soon as the workspace exists
/// so the window can switch to it while its tabs restore and its files are scanned.
fn open_local_workspace(path: PathBuf, app_state: Arc<AppState>, cx: &mut App) -> OpeningWorkspace {
    let started = std::time::Instant::now();
    let (created_tx, created_rx) = futures::channel::oneshot::channel();
    let init: Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + Send> =
        Box::new(move |_, window, cx| {
            if let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() {
                created_tx.send((handle, cx.entity())).ok();
            }
        });
    // Added without switching to it, so only the latest of several clicks takes the window.
    let open = Workspace::new_local(
        vec![path.clone()],
        app_state,
        front_workspace(cx).map(|(window, _)| window),
        None,
        Some(init),
        workspace::OpenMode::Add,
        cx,
    );
    let loaded = cx
        .spawn({
            let path = path.clone();
            async move |cx| {
                open.await.log_err();
                cx.update(|cx| {
                    cx.default_global::<OpeningWorkspaces>()
                        .opening
                        .remove(&path)
                });
            }
        })
        .shared();
    cx.spawn(async move |cx| {
        let (window, workspace) = created_rx.await.ok()?;
        let switched = started.elapsed();
        let scanned =
            workspace.read_with(cx, |workspace, cx| workspace.worktree_scans_complete(cx));
        cx.spawn(async move |_| {
            scanned.await;
            log::info!(
                "opened workspace {} in {switched:?}, scan complete in {:?}",
                path.display(),
                started.elapsed()
            );
        })
        .detach();
        Some(OpenedWorkspace {
            window,
            workspace,
            loaded,
        })
    })
    .shared()
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
        "account.usage" => accounts::usage_changed(cx),
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
    /// Woken as soon as a connection is up, so views refresh then rather than on their next poll.
    pub(crate) waiters: Vec<futures::channel::oneshot::Sender<()>>,
}

impl gpui::Global for AppConnection {}

/// Fetches the context block an attachment expands to when the prompt is sent.
pub fn fetch_attachment_text(id: String, cx: &mut App) -> Task<Result<String>> {
    #[derive(serde::Deserialize)]
    struct AttachmentText {
        text: String,
    }

    let shared = app_client(cx);
    Tokio::spawn_result(cx, async move {
        let client = match shared {
            Some(client) => client,
            None => {
                let home = server_home().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
                Client::connect(&home, "icarus-panel").await?.0
            }
        };
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
        let mut lost_at = Instant::now();
        let mut ever_connected = false;
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
                cx.background_executor()
                    .timer(reconnect_delay(lost_at.elapsed()))
                    .await;
                continue;
            };
            log::info!(
                "icarus: {} the server (v{}) after {:.0?}",
                if ever_connected {
                    "reconnected to"
                } else {
                    "connected to"
                },
                client.server_version,
                lost_at.elapsed()
            );
            ever_connected = true;
            let (reconnect, mut switched) = futures::channel::oneshot::channel();
            cx.update(|cx| {
                let connection = cx.default_global::<AppConnection>();
                connection.client = Some(client.clone());
                connection.reconnect = Some(reconnect);
                for waiter in connection.waiters.drain(..) {
                    let _ = waiter.send(());
                }
            });
            let listed =
                Tokio::spawn_result(cx, worktree_trust::list_workspace_paths(client.clone())).await;
            if let Some(listed) = listed.log_err() {
                cx.update(|cx| {
                    let paths: Vec<PathBuf> = listed.into_iter().map(|(_, path)| path).collect();
                    for path in &paths {
                        if window_for_path(path, cx).is_some() {
                            open_offered_agent_tabs(path.clone(), cx);
                        }
                    }
                    worktree_trust::set_icarus_workspace_paths(paths, cx)
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
                    "client.drive" => {
                        let Some(request) =
                            serde_json::from_value::<driver::DriveRequest>(event.payload).log_err()
                        else {
                            continue;
                        };
                        cx.update(|cx| driver::handle_request(request, cx));
                    }
                    "review.changed" | "note.changed" | "git.changed" | "ports.changed"
                    | "config.changed" | "account.usage" => {
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
                    "agent_tabs" => {
                        let Some(path) = event.payload["path"].as_str().map(PathBuf::from) else {
                            continue;
                        };
                        cx.update(|cx| {
                            if window_for_path(&path, cx).is_some() {
                                open_offered_agent_tabs(path, cx);
                            }
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
            log::info!("icarus: lost the server connection; reconnecting");
            lost_at = Instant::now();
        }
    })
    .detach();
}

/// Retries quickly while a restarting server comes back, then settles to once a second.
fn reconnect_delay(since_lost: Duration) -> Duration {
    if since_lost < Duration::from_secs(10) {
        Duration::from_millis(100)
    } else {
        Duration::from_secs(1)
    }
}

/// Resolves once the app has a server connection (at once when it already has one).
pub(crate) fn app_connected(cx: &mut App) -> futures::channel::oneshot::Receiver<()> {
    let (sender, receiver) = futures::channel::oneshot::channel();
    let connection = cx.default_global::<AppConnection>();
    if connection.client.is_some() {
        let _ = sender.send(());
    } else {
        connection.waiters.push(sender);
    }
    receiver
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
    file_finder::set_changed_files_source(changes_tab::SourceControlChanges, cx);
    diff_tab::register(cx);
    review::attach_notes_to_file_editors(cx);
    sidebar::register(cx);
    zed_import::register(cx);
    quick_commands::register(cx);
    terminals::register(cx);
    notifications::register(cx);
    history::register(cx);
    session_history::register(cx);
    accounts::register(cx);
    remote_control::register(cx);
    settings_view::register(cx);
    client_settings::init(cx);
    focus::register(cx);
    pane_nav::register(cx);
    new_tab_menu::register(cx);
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
    discovered: worktree_inbox::Discovered,
}

/// The snapshot's requests go out together: one round trip, not seven in a row.
async fn fetch_snapshot(client: Arc<Client>) -> Result<Snapshot> {
    let get = |key: &'static str| {
        let client = client.clone();
        async move {
            client
                .request("config.get", serde_json::json!({ "key": key }))
                .await
        }
    };
    let (workspaces, statuses, card_fields, sidebar, rules, projects, inbox) = futures::join!(
        client.request("workspace.list", serde_json::json!({})),
        get("workspace.statuses"),
        get("workspace.cardFields"),
        get("sidebar"),
        get("rules.pr"),
        client.request("project.list", serde_json::json!({})),
        worktree_inbox::fetch_inbox(&client),
    );
    let workspaces = workspaces?;
    let statuses = statuses?;
    let card_fields = card_fields.unwrap_or_default();
    let sidebar = sidebar.unwrap_or_default();
    let rules = rules.unwrap_or_default();
    let projects = projects.unwrap_or_default();
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
    let discovered = worktree_inbox::Discovered::new(&workspaces, inbox);
    Ok(Snapshot {
        discovered,
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
    discovered: worktree_inbox::Discovered,
    ports_requested: HashSet<String>,
    filter: Option<Entity<editor::Editor>>,
    filter_query: String,
    selection: cards::CardSelection,
    card_list: Entity<cards::CardList>,
    _card_list_observation: Subscription,
    reveal_shown_card: bool,
    revealing_shown_card: bool,
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
        cx.spawn(async move |cx| {
            cx.update(|window, cx| {
                let handle = window.window_handle();
                cx.new(|cx| Self::new(workspace, Some(handle), cx))
            })
        })
    }

    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: Option<gpui::AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self::preview(workspace, cx);
        if !enabled(cx) {
            return panel;
        }
        panel.view = cards::CardView::default();
        panel._poll = cx.spawn(async move |this, cx| {
            let mut last_poll: Option<Instant> = None;
            loop {
                if this.upgrade().is_none() {
                    return;
                }
                let in_front = window.is_none_or(|window| {
                    cx.update_window(window, |_, window, _| window.is_window_active())
                        .unwrap_or(false)
                });
                if !in_front && last_poll.is_some_and(|at| at.elapsed() < BACKGROUND_INTERVAL) {
                    cx.background_executor().timer(REFRESH_INTERVAL).await;
                    continue;
                }
                last_poll = Some(Instant::now());
                // The app's own connection: this machine, or the saved server picked in the Servers menu.
                let Some(client) = cx.update(|cx| app_client(cx)) else {
                    let unavailable = anyhow::anyhow!("not connected to the Icarus server");
                    if this
                        .update(cx, |panel, cx| panel.set_unavailable(unavailable, cx))
                        .is_err()
                    {
                        return;
                    }
                    let connected = cx.update(|cx| app_connected(cx));
                    let timer = cx.background_executor().timer(REFRESH_INTERVAL);
                    futures::future::select(connected, timer).await;
                    continue;
                };
                let started = Instant::now();
                let snapshot = Tokio::spawn_result(cx, fetch_snapshot(client.clone())).await;
                let took = started.elapsed();
                if took > SLOW_REQUEST {
                    log::info!("icarus: the workspaces snapshot took {took:.0?}");
                }
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
        let card_list = cx.new(|_| cards::CardList::default());
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
            discovered: worktree_inbox::Discovered::default(),
            ports_requested: HashSet::default(),
            filter: None,
            filter_query: String::new(),
            selection: cards::CardSelection::default(),
            _card_list_observation: cx.observe(&card_list, |_, _, cx| cx.notify()),
            card_list,
            reveal_shown_card: false,
            revealing_shown_card: false,
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
        if self.discovered != snapshot.discovered {
            self.discovered = snapshot.discovered;
            cx.notify();
        }
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

    /// Whether this panel's window shows the workspace at `path` (each workspace in a window has its own panel).
    pub fn shows_workspace(&self, path: &Path, cx: &App) -> bool {
        self.workspace.upgrade().is_some_and(|workspace| {
            workspace
                .read(cx)
                .root_paths(cx)
                .iter()
                .any(|root| root.as_ref() == path)
        })
    }

    /// Folds a group's cards away under its header, or shows them again; remembered per grouping.
    pub fn toggle_section(&mut self, title: &str, cx: &mut Context<Self>) {
        client_settings::toggle_group_collapsed(self.view.group_by.setting(), title, cx).log_err();
        cx.notify();
    }

    /// Whether a section's cards are folded away.
    pub fn section_collapsed(&self, title: &str, cx: &App) -> bool {
        client_settings::group_collapsed(self.view.group_by.setting(), title, cx)
    }

    fn open_workspace(&mut self, path: PathBuf, _window: &mut Window, cx: &mut Context<Self>) {
        show_workspace(path, cx);
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
        let chip_icon = match workspace.status.as_deref() {
            Some(id) => self.status_icon(id, cx),
            None => Icon::from_path(status_style::icon_path("", Some("circle-dashed")))
                .color(Color::Muted),
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
            .size(ButtonSize::None)
            .child(
                cards::chip(cx)
                    .child(chip_icon.size(IconSize::XSmall))
                    .child(
                        Label::new(label)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
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

    /// The list's sections for the chosen grouping: header icon, title and cards.
    fn sections(&self, cx: &App) -> Vec<(Icon, String, Vec<WorkspaceSummary>)> {
        let no_status_icon = || {
            Icon::from_path(status_style::icon_path("", Some("circle-dashed")))
                .size(IconSize::Small)
                .color(Color::Muted)
        };
        let workspaces = self.arranged_workspaces();
        let mut sections: Vec<(Icon, String, Vec<WorkspaceSummary>)> = match self.view.group_by {
            cards::GroupBy::Status => group_by_status(&workspaces, &self.status_order)
                .into_iter()
                .map(|group| match group.status.as_deref() {
                    Some(id) => (
                        self.status_icon(id, cx),
                        self.status_label(id),
                        group.workspaces,
                    ),
                    None => (no_status_icon(), "No status".into(), group.workspaces),
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
                            group.workspaces,
                        )
                    }
                    StackLane::Stack { number, .. } => (
                        Icon::new(IconName::ListTree)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                        format!("Stack #{number}"),
                        group.workspaces,
                    ),
                    StackLane::None => {
                        (no_status_icon(), "Not in a stack".into(), group.workspaces)
                    }
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
                        workspaces,
                    )
                })
                .collect(),
            cards::GroupBy::None => vec![(
                Icon::new(IconName::GitBranch)
                    .size(IconSize::Small)
                    .color(Color::Muted),
                "All workspaces".into(),
                workspaces,
            )],
        };
        for (_, _, workspaces) in &mut sections {
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

    /// The title row's right side: Linear link, stack position, PR diff and author, each behind its field.
    fn render_card_meta(&self, workspace: &WorkspaceSummary, cx: &App) -> impl IntoElement {
        let view = &self.view;
        let pr = workspace.pr.as_ref();
        h_flex()
            .flex_none()
            .gap_1p5()
            .when_some(
                workspace.issue_url().filter(|_| view.shows("linear-title")),
                |row, url| {
                    let tooltip = format!("Open {} in Linear", workspace.display_title());
                    row.child(
                        IconButton::new(
                            SharedString::from(format!("icarus-linear-{}", workspace.id)),
                            IconName::Link,
                        )
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .size(ButtonSize::None)
                        .tooltip(Tooltip::text(tooltip))
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            cx.open_url(&url);
                        }),
                    )
                },
            )
            .when_some(
                pr.and_then(|pr| pr.stack).filter(|_| view.shows("stack")),
                |row, stack| {
                    row.child(
                        h_flex().gap_0p5().child(cards::stack_icon()).child(
                            Label::new(format!(
                                "{}/{}",
                                stack.position,
                                stack.size.max(stack.position)
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        ),
                    )
                },
            )
            .when_some(
                pr.filter(|pr| view.shows("pr-diff") && (pr.additions > 0 || pr.deletions > 0)),
                |row, pr| {
                    row.child(
                        h_flex()
                            .gap_1()
                            .child(
                                Label::new(format!("+{}", cards::with_thousands(pr.additions)))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Created),
                            )
                            .child(
                                Label::new(format!("−{}", cards::with_thousands(pr.deletions)))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Deleted),
                            ),
                    )
                },
            )
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
            )
    }

    /// The chips under the title: status (outside status grouping), merge queue, Linear labels, project.
    fn render_card_chips(
        &self,
        workspace: &WorkspaceSummary,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let view = self.view.clone();
        let mut chips = Vec::new();
        if view.shows_status_chip() {
            chips.push(self.render_status_menu(workspace, cx).into_any_element());
        }
        if view.shows("pr") && workspace.pr.as_ref().is_some_and(|pr| pr.in_merge_queue) {
            chips.push(
                cards::chip(cx)
                    .child(
                        Label::new("Merge queue")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            );
        }
        if view.shows("linear-title") {
            for label in workspace.issue_labels() {
                let color = parse_hex_color(&label.color);
                chips.push(
                    cards::chip(cx)
                        .child(
                            div()
                                .size(px(7.))
                                .rounded_full()
                                .when_some(color, |dot, color| dot.bg(color)),
                        )
                        .child(
                            Label::new(label.name)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .into_any_element(),
                );
            }
        }
        if let Some(project) = view
            .shows("project")
            .then(|| self.project_names.get(&workspace.project_id).cloned())
            .flatten()
        {
            chips.push(
                cards::chip(cx)
                    .child(
                        Icon::new(IconName::Folder)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(project)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            );
        }
        chips
    }

    /// A card with nothing going on: asleep, or a worktree found on disk with no PR and no agents.
    fn is_inactive(&self, workspace: &WorkspaceSummary) -> bool {
        workspace.sleeping
            || (self.discovered.is_discovered(&workspace.id)
                && workspace.pr.is_none()
                && workspace.agents.is_empty())
    }

    fn render_card(
        &self,
        workspace: &WorkspaceSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let path = workspace.path.clone();
        let view = self.view.clone();
        let shown = self.shows_workspace(&workspace.path, cx);
        let inactive = self.is_inactive(workspace);
        let title = match (&workspace.issue, &workspace.title, &workspace.branch) {
            (None, None, Some(branch)) if inactive => branch.clone(),
            _ => workspace.display_title(),
        };
        let lead_icon = match workspace.pr.as_ref().filter(|_| view.shows("pr")) {
            _ if inactive => Icon::from_path(cards::MOON_ICON)
                .size(IconSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            Some(pr) => div()
                .id(SharedString::from(format!("icarus-pr-{}", workspace.id)))
                .tooltip(Tooltip::text(cards::pr_tooltip(pr)))
                .child(
                    Icon::new(IconName::PullRequest)
                        .size(IconSize::Small)
                        .color(cards::pr_state_color(&pr.state)),
                )
                .into_any_element(),
            None => Icon::new(IconName::GitBranch)
                .size(IconSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        };
        let unread = workspace.unread.as_ref().filter(|_| view.shows("unread"));
        let chips = self.render_card_chips(workspace, cx);
        let issue_title = workspace.subtitle().filter(|_| view.shows("linear-title"));
        let agents = self.render_agents(workspace, cx);
        let colors = cx.theme().colors();
        let card = v_flex()
            .id(SharedString::from(format!(
                "icarus-workspace-{}",
                workspace.id
            )))
            .w_full()
            .px_2p5()
            .py_2()
            .gap_1()
            .rounded_lg()
            .border_1()
            .cursor_pointer()
            .map(|card| {
                if shown {
                    let (background, border) = cards::current_card_colors(cx);
                    card.bg(background).border_color(border).shadow_xs()
                } else {
                    card.border_color(gpui::transparent_black())
                        .hover(|style| style.bg(colors.element_hover))
                }
            });
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
                    .child(div().relative().flex_none().child(lead_icon).when_some(
                        unread,
                        |icon, unread| {
                            icon.child(
                                div()
                                    .absolute()
                                    .left(px(-9.))
                                    .top(px(3.))
                                    .child(cards::render_unread_dot(&workspace.id, unread, cx)),
                            )
                        },
                    ))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .child(
                                div().min_w_0().child(
                                    Label::new(title)
                                        .weight(gpui::FontWeight::SEMIBOLD)
                                        .color(if inactive {
                                            Color::Muted
                                        } else {
                                            Color::Default
                                        })
                                        .truncate(),
                                ),
                            )
                            .when(workspace.primary, |row| {
                                row.child(
                                    cards::chip(cx).child(
                                        Label::new("primary")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                                )
                            }),
                    )
                    .when(workspace.pinned, |row| {
                        row.child(
                            Icon::new(IconName::Pin)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .when_some(
                        workspace.setup.as_ref().and_then(SetupSummary::badge),
                        |row, badge| row.child(render_setup_badge(&workspace.id, badge)),
                    )
                    .child(self.render_card_meta(workspace, cx)),
            )
            .when(!chips.is_empty(), |card| {
                card.child(
                    h_flex()
                        .pl(cards::CARD_INDENT)
                        .gap_1()
                        .flex_wrap()
                        .when(inactive, |row| row.opacity(0.6))
                        .children(chips),
                )
            })
            .when_some(issue_title, |card, issue_title| {
                card.child(
                    div().pl(cards::CARD_INDENT).child(
                        Label::new(issue_title)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
                )
            })
            .when(view.shows("ports"), |card| {
                card.children(
                    self.render_port_chips(workspace, cx)
                        .map(|ports| div().pl(cards::CARD_INDENT).child(ports)),
                )
            })
            .when_some(agents, |card, agents| {
                card.child(div().pl(cards::CARD_INDENT).child(agents))
            })
            .when(shown && self.revealing_shown_card, |card| {
                card.relative().child(self.render_reveal_probe())
            });
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
        let origin = server
            .saved_name()
            .is_none()
            .then(|| self.discovered.origin_note(&workspace.id))
            .flatten();
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
                origin.clone(),
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

impl WorkspacesPanel {
    fn join_window_card_list(&mut self, window: &Window, cx: &mut Context<Self>) {
        let list = cards::window_card_list(window.window_handle().window_id(), &self.card_list, cx);
        if list != self.card_list {
            self._card_list_observation = cx.observe(&list, |_, _, cx| cx.notify());
            self.card_list = list;
        }
        let workspace = self.workspace.entity_id();
        if self
            .card_list
            .update(cx, |list, _| list.show_workspace(workspace))
        {
            self.reveal_shown_card = true;
        }
        // The worktree may still be loading, so its card isn't marked as shown yet.
        self.revealing_shown_card = self.reveal_shown_card
            && self
                .workspaces
                .iter()
                .any(|workspace| self.shows_workspace(&workspace.path, cx));
        if self.revealing_shown_card {
            self.reveal_shown_card = false;
        }
    }

    fn render_reveal_probe(&self) -> impl IntoElement {
        let list = self.card_list.downgrade();
        gpui::canvas(
            move |card, _, cx| {
                let Some(list) = list.upgrade() else {
                    return;
                };
                if list.read(cx).reveal(card) {
                    // The new offset only applies to the next frame.
                    cx.defer(move |cx| list.update(cx, |_, cx| cx.notify()));
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }
}

impl Render for WorkspacesPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.join_window_card_list(window, cx);
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
            .debug_selector(|| "icarus-workspaces".into())
            .size_full()
            .overflow_y_scroll()
            // A panel that has no cards yet would clamp the shared position to the top.
            .when(!self.workspaces.is_empty(), |body| {
                body.track_scroll(&self.card_list.read(cx).scroll)
            })
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
        let grouping = self.view.group_by.setting();
        body = body.children(self.discovered.render_other_inboxes(&project_ids, cx));
        for (index, (header_icon, title, workspaces)) in self.sections(cx).into_iter().enumerate() {
            let collapsed = client_settings::group_collapsed(grouping, &title, cx);
            let header = h_flex()
                .id(SharedString::from(format!("icarus-section-{index}")))
                .debug_selector(|| format!("icarus-section-{index}"))
                .px_2p5()
                .pt_1()
                .pb_0p5()
                .gap_1p5()
                .cursor_pointer()
                .on_click({
                    let title = title.clone();
                    cx.listener(move |panel, _, _, cx| panel.toggle_section(&title, cx))
                })
                .child(header_icon)
                .child(
                    Label::new(title.clone())
                        .weight(gpui::FontWeight::BOLD)
                        .truncate(),
                )
                .when(collapsed, |header| {
                    header
                        .child(
                            Label::new(workspaces.len().to_string())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Icon::new(IconName::ChevronRight)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                });
            let header = match project_ids.get(index).cloned() {
                Some(project_id) => self
                    .with_project_menu(project_id, title, workspaces.len(), header)
                    .into_any_element(),
                None => header.into_any_element(),
            };
            let mut section = v_flex().gap_0p5().child(header);
            if let Some(project_id) = project_ids.get(index) {
                section = section.children(self.discovered.render_project_inbox(project_id, cx));
            }
            if !collapsed {
                for workspace in &workspaces {
                    section = section.child(self.render_card(workspace, cx));
                }
            }
            body = body.child(section);
        }
        for server in self.remote.read(cx).servers().to_vec() {
            body = body.child(self.render_remote_server(&server, cx));
        }
        v_flex()
            .key_context("IcarusWorkspacesPanel")
            .debug_selector(|| "icarus-workspaces-panel".into())
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
            primary: false,
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
                .map(|(_, title, items)| {
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
    async fn clicking_a_group_header_folds_its_cards_per_grouping(cx: &mut gpui::TestAppContext) {
        let (_, panel, mut cx) = panel_window(
            vec![
                summary("loose", "/w/loose", None),
                stacked("bottom", 7, 1),
                stacked("top", 7, 2),
            ],
            cx,
        )
        .await;
        cx.dispatch_action(cards::SetWorkspaceGrouping {
            group_by: "stack".into(),
        });
        cx.run_until_parked();
        assert_eq!(
            section_titles(&panel, &mut cx),
            ["Stack #7: top,bottom", "Not in a stack: loose"]
        );
        let second_top =
            |cx: &mut gpui::VisualTestContext| cx.debug_bounds("icarus-section-1").expect("drawn");
        let open = second_top(&mut cx);
        let first = cx.debug_bounds("icarus-section-0").expect("drawn");
        cx.simulate_click(first.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(panel.read_with(&cx, |panel, cx| panel.section_collapsed("Stack #7", cx)));
        assert!(
            second_top(&mut cx).origin.y < open.origin.y,
            "the folded group's cards are gone"
        );

        cx.dispatch_action(cards::SetWorkspaceGrouping {
            group_by: "workspace-status".into(),
        });
        assert!(!panel.read_with(&cx, |panel, cx| panel.section_collapsed("Stack #7", cx)));
        cx.dispatch_action(cards::SetWorkspaceGrouping {
            group_by: "stack".into(),
        });
        assert!(
            panel.read_with(&cx, |panel, cx| panel.section_collapsed("Stack #7", cx)),
            "each grouping remembers its own folds"
        );
        assert_eq!(
            cx.update(|_, cx| client_settings::get(Some(client_settings::COLLAPSED_GROUPS), cx)),
            serde_json::json!({ "stack": ["Stack #7"] }),
            "kept in this app's client.json"
        );
        let first = cx.debug_bounds("icarus-section-0").expect("drawn");
        cx.simulate_click(first.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(!panel.read_with(&cx, |panel, cx| panel.section_collapsed("Stack #7", cx)));
    }

    #[gpui::test]
    async fn the_workspaces_panel_keeps_its_width_while_the_other_sidebar_switches(
        cx: &mut gpui::TestAppContext,
    ) {
        use workspace::dock::test::TestPanel;
        let (workspace, _, mut cx) = panel_window(Vec::new(), cx).await;
        cx.simulate_resize(gpui::size(px(1280.), px(800.)));
        let (fixed, flexible) = workspace.update_in(&mut cx, |workspace, window, cx| {
            let fixed = cx.new(|cx| TestPanel::new(DockPosition::Right, 1, cx));
            let flexible = cx.new(|cx| TestPanel::new_flexible(DockPosition::Right, 2, cx));
            workspace.add_panel(fixed.clone(), window, cx);
            workspace.add_panel(flexible.clone(), window, cx);
            workspace.open_panel::<WorkspacesPanel>(window, cx);
            (fixed, flexible)
        });
        cx.run_until_parked();
        let width = |cx: &mut gpui::VisualTestContext| {
            cx.debug_bounds("icarus-workspaces-panel")
                .expect("the panel is drawn")
                .size
                .width
        };
        let alone = width(&mut cx);
        for index in [0, 1, 0] {
            workspace.update_in(&mut cx, |workspace, window, cx| {
                let dock = workspace.right_dock().clone();
                dock.update(cx, |dock, cx| {
                    dock.set_open(true, window, cx);
                    dock.activate_panel(index, window, cx);
                });
            });
            cx.run_until_parked();
            assert_eq!(width(&mut cx), alone, "right panel {index}");
        }
        drop((fixed, flexible));
    }

    #[gpui::test]
    async fn clicking_a_card_shows_its_workspace_once(cx: &mut gpui::TestAppContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            AppState::set_global(app_state.clone(), cx);
            <project::DisableAiSettings as settings::Settings>::register(cx);
            editor::init(cx);
            project::trusted_worktrees::init(Default::default(), cx);
            worktree_trust::register(cx);
            worktree_trust::add_icarus_workspace_path(Path::new("/project"), cx);
            app_state
        });
        let fs = app_state.fs.as_fake();
        fs.insert_tree("/project", serde_json::json!({ "a.txt": "" }))
            .await;
        fs.insert_tree("/w/feature", serde_json::json!({ "b.txt": "" }))
            .await;
        fs.insert_tree("/w/other", serde_json::json!({ "c.txt": "" }))
            .await;
        let project =
            project::Project::test(app_state.fs.clone(), [Path::new("/project")], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            register_panel_actions(workspace);
            let panel = cx.new(|cx| WorkspacesPanel::preview(workspace.weak_handle(), cx));
            panel.update(cx, |panel, cx| {
                panel.set_workspaces(
                    vec![
                        summary("main", "/project", None),
                        summary("feature", "/w/feature", None),
                    ],
                    cx,
                )
            });
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        cx.run_until_parked();

        let shown = |cx: &mut gpui::VisualTestContext| {
            window
                .read_with(cx, |multi_workspace, cx| {
                    let roots = multi_workspace.workspace().read(cx).root_paths(cx);
                    let roots: Vec<PathBuf> = roots.iter().map(|root| root.to_path_buf()).collect();
                    (roots, multi_workspace.workspaces().count())
                })
                .expect("window")
        };
        let click = |path: &str, cx: &mut gpui::VisualTestContext| {
            panel.update_in(cx, |panel, window, cx| {
                panel.open_workspace(PathBuf::from(path), window, cx)
            });
        };

        click("/w/feature", &mut cx);
        click("/w/feature", &mut cx);
        cx.run_until_parked();
        assert_eq!(
            shown(&mut cx),
            (vec![PathBuf::from("/w/feature")], 2),
            "the window now shows the workspace, opened once even when clicked twice"
        );
        assert_eq!(cx.update(|_, cx| cx.windows().len()), 1);
        let feature = window
            .read_with(&cx, |multi_workspace, _| {
                multi_workspace.workspace().clone()
            })
            .expect("window");
        let restricted = cx.update(|_, cx| {
            let feature = feature.read(cx).project().read(cx).worktree_store();
            project::trusted_worktrees::TrustedWorktrees::try_get_global(cx)
                .expect("trust store")
                .read(cx)
                .restricted_worktrees(&feature, cx)
                .len()
        });
        assert_eq!(restricted, 0, "an Icarus workspace opens trusted");
        assert!(
            panel.read_with(&cx, |panel, cx| panel
                .shows_workspace(Path::new("/project"), cx)
                && !panel.shows_workspace(Path::new("/w/feature"), cx)),
            "each window's panel marks the workspace it shows"
        );

        click("/project", &mut cx);
        cx.run_until_parked();
        assert_eq!(shown(&mut cx), (vec![PathBuf::from("/project")], 2));
        click("/w/feature", &mut cx);
        cx.run_until_parked();
        assert_eq!(
            shown(&mut cx),
            (vec![PathBuf::from("/w/feature")], 2),
            "an open workspace is activated, not opened again"
        );
        assert_eq!(cx.update(|_, cx| cx.windows().len()), 1);

        click("/w/other", &mut cx);
        click("/project", &mut cx);
        cx.run_until_parked();
        assert_eq!(
            shown(&mut cx),
            (vec![PathBuf::from("/project")], 3),
            "the last click wins over an open that finishes after it"
        );
    }

    #[gpui::test]
    async fn switching_worktrees_keeps_the_cards_list_where_it_was(cx: &mut gpui::TestAppContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            AppState::set_global(app_state.clone(), cx);
            <project::DisableAiSettings as settings::Settings>::register(cx);
            editor::init(cx);
            project::trusted_worktrees::init(Default::default(), cx);
            worktree_trust::register(cx);
            worktree_trust::add_icarus_workspace_path(Path::new("/project"), cx);
            app_state
        });
        let fs = app_state.fs.as_fake();
        fs.insert_tree("/project", serde_json::json!({ "a.txt": "" }))
            .await;
        let mut summaries = vec![summary("main", "/project", None)];
        for index in 0..16 {
            let path = format!("/w/feature{index}");
            fs.insert_tree(&path, serde_json::json!({ "b.txt": "" }))
                .await;
            summaries.push(summary(&format!("feature{index}"), &path, None));
        }
        cx.update(|cx| {
            let summaries = summaries.clone();
            cx.observe_new(move |workspace: &mut Workspace, window, cx| {
                let Some(window) = window else {
                    return;
                };
                let summaries = summaries.clone();
                let handle = workspace.weak_handle();
                let panel = cx.new(|cx| {
                    let mut panel = WorkspacesPanel::preview(handle, cx);
                    panel.set_workspaces(summaries, cx);
                    panel
                });
                workspace.add_panel(panel, window, cx);
                workspace.open_panel::<WorkspacesPanel>(window, cx);
            })
            .detach();
        });
        let project =
            project::Project::test(app_state.fs.clone(), [Path::new("/project")], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        cx.simulate_resize(gpui::size(px(1280.), px(500.)));
        cx.run_until_parked();

        let list = cx
            .debug_bounds("icarus-workspaces")
            .expect("the cards list is drawn");
        let card = |name: &str, cx: &mut gpui::VisualTestContext| {
            cx.debug_bounds(format!("card:ws-{name}").leak())
        };
        let in_view = |bounds: gpui::Bounds<Pixels>| {
            bounds.top() >= list.top() && bounds.bottom() <= list.bottom()
        };
        cx.simulate_event(gpui::ScrollEvent {
            position: list.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-400.))),
            ..Default::default()
        });
        cx.run_until_parked();
        let names: Vec<String> = (0..16).map(|index| format!("feature{index}")).collect();
        let clicked = names
            .iter()
            .find(|name| {
                card(name, &mut cx)
                    .is_some_and(|bounds| in_view(bounds) && bounds.center().y > list.center().y)
            })
            .cloned()
            .expect("a card is in the lower half of the list after scrolling");
        let before = card(&clicked, &mut cx).expect("drawn");
        let scroll_offset = |cx: &mut gpui::VisualTestContext| {
            window
                .read_with(cx, |multi_workspace, cx| {
                    let panel = multi_workspace
                        .workspace()
                        .read(cx)
                        .panel::<WorkspacesPanel>(cx)
                        .expect("the shown workspace has the panel");
                    panel.read(cx).card_list.read(cx).scroll.offset()
                })
                .expect("window")
        };
        let offset_before = scroll_offset(&mut cx);
        assert!(offset_before.y < px(0.), "the list scrolled");

        cx.simulate_click(before.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let shown = window
            .read_with(&cx, |multi_workspace, cx| {
                multi_workspace.workspace().read(cx).root_paths(cx)[0].to_path_buf()
            })
            .expect("window");
        assert_eq!(shown, PathBuf::from(format!("/w/{clicked}")));
        assert_eq!(
            scroll_offset(&mut cx),
            offset_before,
            "the list keeps its scroll"
        );
        assert_eq!(
            card(&clicked, &mut cx),
            Some(before),
            "the clicked card stays where it was"
        );

        cx.simulate_event(gpui::ScrollEvent {
            position: list.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(4000.))),
            ..Default::default()
        });
        cx.run_until_parked();
        let hidden = names
            .iter()
            .rev()
            .filter(|name| **name != clicked)
            .find(|name| card(name, &mut cx).is_some_and(|bounds| bounds.top() > list.bottom()))
            .cloned()
            .expect("a card is below the fold at the top of the list");
        cx.update(|_, cx| show_workspace(PathBuf::from(format!("/w/{hidden}")), cx));
        cx.run_until_parked();
        // The test window draws only when it handles input.
        for _ in 0..2 {
            cx.simulate_mouse_move(list.center(), None, gpui::Modifiers::none());
        }
        assert!(
            card(&hidden, &mut cx).is_some_and(in_view),
            "a worktree switched to from elsewhere scrolls its card into view"
        );
    }

    #[gpui::test]
    async fn a_clicked_workspace_shows_before_its_files_are_scanned(cx: &mut gpui::TestAppContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            AppState::set_global(app_state.clone(), cx);
            <project::DisableAiSettings as settings::Settings>::register(cx);
            editor::init(cx);
            project::trusted_worktrees::init(Default::default(), cx);
            worktree_trust::register(cx);
            worktree_trust::add_icarus_workspace_path(Path::new("/project"), cx);
            app_state
        });
        let fs = app_state.fs.as_fake();
        fs.insert_tree("/project", serde_json::json!({ "a.txt": "" }))
            .await;
        let mut big = serde_json::Map::new();
        for directory in 0..40 {
            let files: serde_json::Map<String, serde_json::Value> = (0..20)
                .map(|file| (format!("f{file}.rs"), serde_json::json!("")))
                .collect();
            big.insert(format!("d{directory}"), serde_json::Value::Object(files));
        }
        fs.insert_tree("/w/big", serde_json::Value::Object(big))
            .await;
        let project =
            project::Project::test(app_state.fs.clone(), [Path::new("/project")], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let multi_workspace = window.entity(cx).expect("window");
        let scanning_when_shown = std::rc::Rc::new(std::cell::Cell::new(None));
        let _observation = cx.update({
            let scanning_when_shown = scanning_when_shown.clone();
            |cx| {
                cx.observe(&multi_workspace, move |multi_workspace, cx| {
                    let shown = multi_workspace.read(cx).workspace().read(cx);
                    let is_big = shown
                        .root_paths(cx)
                        .iter()
                        .any(|root| root.as_ref() == Path::new("/w/big"));
                    if is_big && scanning_when_shown.get().is_none() {
                        let scanning = shown.worktree_scans_complete(cx).now_or_never().is_none();
                        let restoring =
                            cx.try_global::<OpeningWorkspaces>().is_some_and(|opening| {
                                opening.opening.contains_key(Path::new("/w/big"))
                            });
                        scanning_when_shown.set(Some((scanning, restoring)));
                    }
                })
            }
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        cx.update(|_, cx| show_workspace(PathBuf::from("/w/big"), cx));
        cx.run_until_parked();
        assert_eq!(
            scanning_when_shown.get(),
            Some((true, true)),
            "the window switches while the worktree is scanned and its tabs restore"
        );
        let shown = window
            .read_with(&cx, |multi_workspace, _| {
                multi_workspace.workspace().clone()
            })
            .expect("window");
        let scanned = shown.read_with(&cx, |workspace, cx| {
            workspace
                .worktree_scans_complete(cx)
                .now_or_never()
                .is_some()
        });
        assert!(scanned, "and the scan finishes behind it");
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

    #[gpui::test]
    fn a_closed_agent_tab_ends_its_session_on_the_server(cx: &mut gpui::TestAppContext) {
        let calls = cx.update(|cx| {
            let calls = review::tests::stub_rpc(cx, |_, _| Ok(serde_json::Value::Null));
            end_closed_agent("agent-1".into(), cx);
            calls
        });
        cx.run_until_parked();
        assert_eq!(
            review::tests::calls_to(&calls, "agent.kill"),
            [serde_json::json!({ "id": "agent-1", "remove": true })]
        );
    }
}
