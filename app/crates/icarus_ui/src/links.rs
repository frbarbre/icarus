use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use gpui::{
    Action, AnyWindowHandle, App, ClipboardItem, Context, Entity, Global, Render, Subscription,
    Task, WeakEntity, Window,
};
use gpui_tokio::Tokio;
use icarus_client::Client;
use project::Project;
use serde::Deserialize;
use ui::{ButtonLike, ContextMenu, PopoverMenu, PopoverMenuHandle, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::notifications::NotificationId;
use workspace::{Toast, Workspace};

use crate::github_panel::{format_relative, parse_timestamp};
use crate::{CopyPullRequestUrl, OpenFrontend, OpenPreview, OpenPullRequest, server_home};

pub const GLOBE_ICON: &str = "icons/icarus/globe.svg";
const LINKS_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
/// Previews come from GitHub, so they are refreshed less often than local ports.
const PREVIEWS_REFRESH_EVERY: u32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortKind {
    Frontend,
    Api,
    Docs,
    Storybook,
    Database,
    Cache,
    #[serde(other)]
    Other,
}

impl PortKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Frontend => "frontend",
            Self::Api => "api",
            Self::Docs => "docs",
            Self::Storybook => "storybook",
            Self::Database => "database",
            Self::Cache => "cache",
            Self::Other => "port",
        }
    }
}

/// A listening port the server attributed to a workspace, as `ports.list` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PortEntry {
    pub port: u16,
    pub url: String,
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default)]
    pub label: Option<String>,
    pub kind: PortKind,
    #[serde(default)]
    pub framework: Option<String>,
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub source: Option<String>,
}

impl PortEntry {
    /// The chip text on a workspace card, e.g. `frontend :5173`.
    pub fn chip_label(&self) -> String {
        match (&self.kind, &self.label) {
            (PortKind::Other, Some(label)) => format!("{label} :{}", self.port),
            (kind, _) => format!("{} :{}", kind.label(), self.port),
        }
    }

    pub fn display_name(&self) -> String {
        self.label
            .clone()
            .or_else(|| self.framework.clone())
            .unwrap_or_else(|| format!("{} :{}", self.kind.label(), self.port))
    }

    /// Databases and caches report URLs like `postgres://…` that a browser can't open.
    pub fn is_openable(&self) -> bool {
        self.url.starts_with("http://") || self.url.starts_with("https://")
    }
}

/// The frontend to open: the server's primary one, else the lowest port.
pub fn frontend_port(ports: &[PortEntry]) -> Option<&PortEntry> {
    ports
        .iter()
        .filter(|port| port.kind == PortKind::Frontend && port.is_openable())
        .max_by_key(|port| (port.primary, std::cmp::Reverse(port.port)))
}

fn default_true() -> bool {
    true
}

/// A preview deployment of the workspace's pull request, as `pr.previews` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PreviewDeployment {
    pub app: String,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    pub state: String,
    #[serde(default)]
    pub sha: Option<String>,
    #[serde(default = "default_true")]
    pub is_head: bool,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub log_url: Option<String>,
}

impl PreviewDeployment {
    pub fn short_sha(&self) -> Option<&str> {
        self.sha.as_deref().map(|sha| sha.get(..7).unwrap_or(sha))
    }

    /// The tooltip line: app, commit, age and whether the deployment lags the PR head.
    pub fn summary(&self, now: i64) -> String {
        let mut parts = vec![self.app.clone()];
        if let Some(sha) = self.short_sha() {
            parts.push(sha.to_string());
        }
        if let Some(created) = self.created_at.as_deref().and_then(parse_timestamp) {
            parts.push(format_relative(now - created));
        }
        if !self.is_head {
            parts.push("behind head".into());
        }
        parts.join(" · ")
    }

    fn same_deployment(&self, other: &Self) -> bool {
        self.app == other.app && self.environment == other.environment && self.url == other.url
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PreviewList {
    pub previews: Vec<PreviewDeployment>,
    pub primary: Option<PreviewDeployment>,
}

/// All previews with the primary one first.
pub fn ordered_previews(
    previews: &[PreviewDeployment],
    primary: Option<&PreviewDeployment>,
) -> Vec<PreviewDeployment> {
    let mut ordered = Vec::with_capacity(previews.len() + 1);
    if let Some(primary) = primary {
        ordered.push(primary.clone());
    }
    for preview in previews {
        if !ordered
            .iter()
            .any(|existing| existing.same_deployment(preview))
        {
            ordered.push(preview.clone());
        }
    }
    ordered
}

/// The previews that can be opened in a browser, the primary one first.
pub fn openable_previews(
    previews: &[PreviewDeployment],
    primary: Option<&PreviewDeployment>,
) -> Vec<PreviewDeployment> {
    ordered_previews(previews, primary)
        .into_iter()
        .filter(|preview| preview.url.is_some())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewChoice {
    Nothing,
    Open(String),
    Pick(Vec<PreviewDeployment>),
}

pub fn choose_preview(openable: Vec<PreviewDeployment>) -> PreviewChoice {
    match openable.as_slice() {
        [] => PreviewChoice::Nothing,
        [only] => only
            .url
            .clone()
            .map_or(PreviewChoice::Nothing, PreviewChoice::Open),
        _ => PreviewChoice::Pick(openable),
    }
}

/// Ports per Icarus workspace id, shared by every window's title bar and Workspaces panel.
#[derive(Default)]
pub struct PortsStore {
    by_workspace: HashMap<String, Vec<PortEntry>>,
}

struct GlobalPortsStore(Entity<PortsStore>);

impl Global for GlobalPortsStore {}

impl PortsStore {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(store) = cx.try_global::<GlobalPortsStore>() {
            return store.0.clone();
        }
        let store = cx.new(|_| Self::default());
        cx.set_global(GlobalPortsStore(store.clone()));
        store
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalPortsStore>()
            .map(|store| store.0.clone())
    }

    pub fn ports(&self, workspace_id: &str) -> &[PortEntry] {
        self.by_workspace
            .get(workspace_id)
            .map_or(&[], Vec::as_slice)
    }

    pub fn contains(&self, workspace_id: &str) -> bool {
        self.by_workspace.contains_key(workspace_id)
    }

    pub fn set_ports(
        &mut self,
        workspace_id: String,
        ports: Vec<PortEntry>,
        cx: &mut Context<Self>,
    ) {
        if self.by_workspace.get(&workspace_id) != Some(&ports) {
            self.by_workspace.insert(workspace_id, ports);
            cx.notify();
        }
    }
}

/// Applies a `ports.changed` event to the shared store.
pub(crate) fn ports_changed(payload: serde_json::Value, cx: &mut App) {
    #[derive(Deserialize)]
    struct PortsChanged {
        workspace: String,
        ports: Vec<PortEntry>,
    }

    let Some(changed) = serde_json::from_value::<PortsChanged>(payload).log_err() else {
        return;
    };
    PortsStore::global(cx).update(cx, |store, cx| {
        store.set_ports(changed.workspace, changed.ports, cx)
    });
}

pub(crate) async fn fetch_ports(
    client: Arc<Client>,
    workspace_id: String,
) -> Result<Vec<PortEntry>> {
    let ports = client
        .request(
            "ports.list",
            serde_json::json!({ "workspace": workspace_id }),
        )
        .await?;
    Ok(serde_json::from_value(ports)?)
}

#[derive(Debug, Clone, Default, PartialEq)]
struct LinkTarget {
    workspace_id: Option<String>,
    pull_request_url: Option<String>,
}

struct FetchedLinks {
    target: LinkTarget,
    ports: Option<Vec<PortEntry>>,
    previews: Option<PreviewList>,
}

async fn fetch_links(
    client: Arc<Client>,
    paths: Vec<PathBuf>,
    previous_workspace_id: Option<String>,
    previews_due: bool,
) -> Result<FetchedLinks> {
    let workspaces = client
        .request("workspace.list", serde_json::json!({}))
        .await?;
    let target = workspaces
        .as_array()
        .into_iter()
        .flatten()
        .find(|workspace| {
            workspace["path"]
                .as_str()
                .is_some_and(|path| paths.contains(&PathBuf::from(path)))
        })
        .map(|workspace| LinkTarget {
            workspace_id: workspace["id"].as_str().map(str::to_string),
            pull_request_url: workspace["pull_request"]["url"]
                .as_str()
                .map(str::to_string),
        })
        .unwrap_or_default();
    let Some(workspace_id) = target.workspace_id.clone() else {
        return Ok(FetchedLinks {
            target,
            ports: None,
            previews: Some(PreviewList::default()),
        });
    };
    let ports = fetch_ports(client.clone(), workspace_id.clone()).await?;
    let previews = if previews_due || previous_workspace_id.as_ref() != Some(&workspace_id) {
        let reply = client
            .request(
                "pr.previews",
                serde_json::json!({ "workspace": workspace_id }),
            )
            .await;
        match reply.and_then(|reply| Ok(serde_json::from_value::<PreviewList>(reply)?)) {
            Ok(previews) => Some(previews),
            Err(error) => {
                log::debug!("no previews for workspace {workspace_id}: {error:#}");
                Some(PreviewList::default())
            }
        }
    } else {
        None
    };
    Ok(FetchedLinks {
        target,
        ports: Some(ports),
        previews,
    })
}

fn project_paths(project: &WeakEntity<Project>, cx: &App) -> Vec<PathBuf> {
    project
        .upgrade()
        .map(|project| {
            project
                .read(cx)
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect()
        })
        .unwrap_or_default()
}

/// Each window's links, so the keybindings act on the window they were pressed in.
#[derive(Default)]
struct WorkspaceLinksByWindow(HashMap<AnyWindowHandle, WeakEntity<WorkspaceLinks>>);

impl Global for WorkspaceLinksByWindow {}

/// A preview of the title bar's links, for visual tests: `now` (RFC 3339), `workspace_id`,
/// `pull_request_url`, `ports` as `ports.list` returns them and `previews` as `pr.previews` does.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LinksFixture {
    now: Option<String>,
    workspace_id: Option<String>,
    pull_request_url: Option<String>,
    ports: Vec<PortEntry>,
    previews: PreviewList,
}

/// The title bar's "open frontend" and "open preview" buttons for this window's Icarus workspace.
pub struct WorkspaceLinks {
    client: Option<Arc<Client>>,
    target: LinkTarget,
    previews: PreviewList,
    fixed_now: Option<i64>,
    preview_menu: PopoverMenuHandle<ContextMenu>,
    _poll: Task<()>,
    _ports_observation: Subscription,
}

impl WorkspaceLinks {
    pub fn new(project: Entity<Project>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let handle = window.window_handle();
        let weak_self = cx.entity().downgrade();
        cx.default_global::<WorkspaceLinksByWindow>()
            .0
            .insert(handle, weak_self);
        let mut links = Self::empty(cx);
        if !crate::enabled(cx) {
            return links;
        }
        let project = project.downgrade();
        links._poll = cx.spawn(async move |this, cx| {
            let mut tick: u32 = 0;
            loop {
                let Ok((client, previous_workspace_id)) = this.read_with(cx, |links, _| {
                    (links.client.clone(), links.target.workspace_id.clone())
                }) else {
                    return;
                };
                let client = match client {
                    Some(client) => Some(client),
                    None => {
                        let connect = Tokio::spawn_result(cx, async move {
                            let home = server_home()
                                .ok_or_else(|| anyhow::anyhow!("no home directory"))?;
                            let (client, _events) = Client::connect(&home, "icarus-links").await?;
                            Ok(client)
                        });
                        match connect.await {
                            Ok(client) => Some(client),
                            Err(error) => {
                                log::debug!("icarus server unavailable: {error:#}");
                                None
                            }
                        }
                    }
                };
                if let Some(client) = client {
                    let paths = cx.update(|cx| project_paths(&project, cx));
                    let previews_due = tick.is_multiple_of(PREVIEWS_REFRESH_EVERY);
                    let fetched = Tokio::spawn_result(
                        cx,
                        fetch_links(client.clone(), paths, previous_workspace_id, previews_due),
                    )
                    .await;
                    let updated = this.update(cx, |links, cx| match fetched {
                        Ok(fetched) => {
                            links.client = Some(client);
                            links.apply(fetched, cx);
                        }
                        Err(error) => {
                            log::debug!("couldn't load Icarus links: {error:#}");
                            links.client = None;
                            links.apply(
                                FetchedLinks {
                                    target: LinkTarget::default(),
                                    ports: None,
                                    previews: Some(PreviewList::default()),
                                },
                                cx,
                            );
                        }
                    });
                    if updated.is_err() {
                        return;
                    }
                }
                tick = tick.wrapping_add(1);
                cx.background_executor().timer(LINKS_REFRESH_INTERVAL).await;
            }
        });
        links
    }

    fn empty(cx: &mut Context<Self>) -> Self {
        let store = PortsStore::global(cx);
        Self {
            client: None,
            target: LinkTarget::default(),
            previews: PreviewList::default(),
            fixed_now: None,
            preview_menu: PopoverMenuHandle::default(),
            _poll: Task::ready(()),
            _ports_observation: cx.observe(&store, |_, _, cx| cx.notify()),
        }
    }

    /// Links that never contact a server and show the fixture described on `LinksFixture`.
    pub fn preview(fixture: serde_json::Value, cx: &mut Context<Self>) -> Self {
        let mut links = Self::empty(cx);
        match serde_json::from_value::<LinksFixture>(fixture) {
            Ok(fixture) => {
                links.fixed_now = fixture.now.as_deref().and_then(parse_timestamp);
                links.target = LinkTarget {
                    workspace_id: fixture.workspace_id.clone(),
                    pull_request_url: fixture.pull_request_url,
                };
                links.previews = fixture.previews;
                if let Some(workspace_id) = fixture.workspace_id {
                    PortsStore::global(cx).update(cx, |store, cx| {
                        store.set_ports(workspace_id, fixture.ports, cx)
                    });
                }
            }
            Err(error) => log::error!("invalid links fixture: {error}"),
        }
        links
    }

    fn apply(&mut self, fetched: FetchedLinks, cx: &mut Context<Self>) {
        if let (Some(workspace_id), Some(ports)) = (&fetched.target.workspace_id, fetched.ports) {
            let workspace_id = workspace_id.clone();
            PortsStore::global(cx).update(cx, |store, cx| store.set_ports(workspace_id, ports, cx));
        }
        if self.target != fetched.target {
            self.target = fetched.target;
            cx.notify();
        }
        if let Some(previews) = fetched.previews
            && self.previews != previews
        {
            self.previews = previews;
            cx.notify();
        }
    }

    fn now(&self) -> i64 {
        self.fixed_now
            .unwrap_or_else(|| time::OffsetDateTime::now_utc().unix_timestamp())
    }

    pub fn frontend(&self, cx: &App) -> Option<PortEntry> {
        let workspace_id = self.target.workspace_id.as_deref()?;
        let store = PortsStore::try_global(cx)?;
        frontend_port(store.read(cx).ports(workspace_id)).cloned()
    }

    pub fn openable_previews(&self) -> Vec<PreviewDeployment> {
        openable_previews(&self.previews.previews, self.previews.primary.as_ref())
    }

    /// The frontend button's tooltip title and detail.
    pub fn frontend_tooltip(&self, cx: &App) -> Option<(String, String)> {
        self.frontend(cx)
            .map(|port| (format!("Open {}", port.display_name()), port.url))
    }

    /// The preview button's tooltip title and detail.
    pub fn preview_tooltip(&self) -> Option<(String, String)> {
        let previews = self.openable_previews();
        let first = previews.first()?;
        let title = match previews.len() {
            1 => "Open preview".to_string(),
            count => format!("Open preview ({count} deployments)"),
        };
        Some((title, first.summary(self.now())))
    }

    pub fn preview_menu_handle(&self) -> PopoverMenuHandle<ContextMenu> {
        self.preview_menu.clone()
    }

    fn render_frontend_button(&self, cx: &App) -> Option<AnyElement> {
        let port = self.frontend(cx)?;
        let (title, meta) = self.frontend_tooltip(cx)?;
        let url = port.url;
        Some(
            ButtonLike::new("icarus-open-frontend")
                .child(Icon::from_path(GLOBE_ICON).size(IconSize::Small))
                .tooltip(move |_window, cx| {
                    Tooltip::with_meta(
                        title.clone(),
                        Some(&OpenFrontend as &dyn Action),
                        meta.clone(),
                        cx,
                    )
                })
                .on_click(move |_, _window, cx| cx.open_url(&url))
                .into_any_element(),
        )
    }

    fn render_preview_button(&self) -> Option<AnyElement> {
        let previews = self.openable_previews();
        let (title, meta) = self.preview_tooltip()?;
        let behind_head = previews.first().is_some_and(|preview| !preview.is_head);
        let button = IconButton::new("icarus-open-preview", IconName::Eye)
            .icon_size(IconSize::Small)
            .icon_color(if behind_head {
                Color::Warning
            } else {
                Color::Default
            })
            .tooltip(move |_window, cx| {
                Tooltip::with_meta(
                    title.clone(),
                    Some(&OpenPreview as &dyn Action),
                    meta.clone(),
                    cx,
                )
            });
        match choose_preview(previews) {
            PreviewChoice::Nothing => None,
            PreviewChoice::Open(url) => Some(
                button
                    .on_click(move |_, _window, cx| cx.open_url(&url))
                    .into_any_element(),
            ),
            PreviewChoice::Pick(previews) => {
                let now = self.now();
                Some(
                    PopoverMenu::new("icarus-preview-menu")
                        .with_handle(self.preview_menu.clone())
                        .trigger(button)
                        .anchor(gpui::Anchor::TopRight)
                        .menu(move |window, cx| {
                            Some(preview_menu(previews.clone(), now, window, cx))
                        })
                        .into_any_element(),
                )
            }
        }
    }
}

pub fn preview_menu(
    previews: Vec<PreviewDeployment>,
    now: i64,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |mut menu, _, _| {
        menu = menu.header("Previews");
        for preview in previews {
            let Some(url) = preview.url.clone() else {
                continue;
            };
            let age = preview
                .created_at
                .as_deref()
                .and_then(parse_timestamp)
                .map(|created| format_relative(now - created));
            menu = menu.custom_entry(
                move |_window, _cx| {
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(Label::new(preview.app.clone()))
                        .when_some(preview.short_sha().map(str::to_string), |row, sha| {
                            row.child(Label::new(sha).size(LabelSize::Small).color(Color::Muted))
                        })
                        .when_some(age.clone(), |row, age| {
                            row.child(Label::new(age).size(LabelSize::Small).color(Color::Muted))
                        })
                        .child(div().flex_1())
                        .when(!preview.is_head, |row| {
                            row.child(
                                Label::new("behind head")
                                    .size(LabelSize::Small)
                                    .color(Color::Warning),
                            )
                        })
                        .into_any_element()
                },
                move |_window, cx| cx.open_url(&url),
            );
        }
        menu
    })
}

impl Render for WorkspaceLinks {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_0p5()
            .children(self.render_frontend_button(cx))
            .children(self.render_preview_button())
    }
}

fn links_for_window(window: &Window, cx: &App) -> Option<Entity<WorkspaceLinks>> {
    cx.try_global::<WorkspaceLinksByWindow>()
        .and_then(|links| links.0.get(&window.window_handle()))
        .and_then(WeakEntity::upgrade)
}

fn show_message(
    workspace: &mut Workspace,
    message: impl Into<Cow<'static, str>>,
    cx: &mut Context<Workspace>,
) {
    struct IcarusLinksToast;
    workspace.show_toast(
        Toast::new(NotificationId::unique::<IcarusLinksToast>(), message).autohide(),
        cx,
    );
}

const NO_WORKSPACE: &str = "This window isn't an Icarus workspace.";

fn open_frontend(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(links) = links_for_window(window, cx) else {
        show_message(workspace, NO_WORKSPACE, cx);
        return;
    };
    let (client, workspace_id, cached) = {
        let links = links.read(cx);
        (
            links.client.clone(),
            links.target.workspace_id.clone(),
            links.frontend(cx),
        )
    };
    let (Some(client), Some(workspace_id)) = (client, workspace_id) else {
        match cached {
            Some(port) => cx.open_url(&port.url),
            None => show_message(workspace, NO_WORKSPACE, cx),
        }
        return;
    };
    let request = Tokio::spawn_result(cx, async move {
        client
            .request(
                "ports.primary",
                serde_json::json!({ "workspace": workspace_id, "kind": "frontend" }),
            )
            .await
    });
    cx.spawn(async move |workspace, cx| {
        let reply = request.await;
        workspace.update(cx, |workspace, cx| match reply {
            Ok(reply) => match reply["url"].as_str() {
                Some(url) => cx.open_url(url),
                None => show_message(workspace, "No frontend is running in this workspace.", cx),
            },
            Err(error) => show_message(
                workspace,
                format!("Couldn't ask Icarus for the frontend: {error:#}"),
                cx,
            ),
        })
    })
    .detach_and_log_err(cx);
}

fn open_preview(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(links) = links_for_window(window, cx) else {
        show_message(workspace, NO_WORKSPACE, cx);
        return;
    };
    match choose_preview(links.read(cx).openable_previews()) {
        PreviewChoice::Nothing => show_message(
            workspace,
            "This pull request has no preview deployment.",
            cx,
        ),
        PreviewChoice::Open(url) => cx.open_url(&url),
        PreviewChoice::Pick(_) => {
            let menu = links.read(cx).preview_menu.clone();
            menu.show(window, cx);
        }
    }
}

fn pull_request_url(
    workspace: &mut Workspace,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> Option<String> {
    let Some(links) = links_for_window(window, cx) else {
        show_message(workspace, NO_WORKSPACE, cx);
        return None;
    };
    let url = links.read(cx).target.pull_request_url.clone();
    if url.is_none() {
        show_message(workspace, "This workspace has no pull request yet.", cx);
    }
    url
}

pub(crate) fn register(cx: &mut App) {
    PortsStore::global(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenFrontend, window, cx| {
            open_frontend(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenPreview, window, cx| {
            open_preview(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenPullRequest, window, cx| {
            if let Some(url) = pull_request_url(workspace, window, cx) {
                cx.open_url(&url);
            }
        });
        workspace.register_action(|workspace, _: &CopyPullRequestUrl, window, cx| {
            if let Some(url) = pull_request_url(workspace, window, cx) {
                cx.write_to_clipboard(ClipboardItem::new_string(url));
                show_message(workspace, "Copied the pull request URL.", cx);
            }
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(json: serde_json::Value) -> PortEntry {
        serde_json::from_value(json).expect("valid port")
    }

    fn preview(app: &str, url: Option<&str>, is_head: bool) -> PreviewDeployment {
        PreviewDeployment {
            app: app.into(),
            environment: Some(format!("{app}-preview")),
            url: url.map(str::to_string),
            state: "success".into(),
            sha: Some("a1b2c3d4e5f6".into()),
            is_head,
            created_at: Some("2026-11-15T11:55:00Z".into()),
            source: Some("deployment".into()),
            log_url: None,
        }
    }

    #[test]
    fn chips_name_the_kind_and_port() {
        let vite = port(serde_json::json!({
            "port": 5173, "url": "http://localhost:5173", "kind": "frontend", "framework": "vite",
        }));
        assert_eq!(vite.chip_label(), "frontend :5173");
        assert_eq!(vite.display_name(), "vite");
        let custom = port(serde_json::json!({
            "port": 9000, "url": "http://localhost:9000", "kind": "something-new", "label": "worker",
        }));
        assert_eq!(custom.kind, PortKind::Other);
        assert_eq!(custom.chip_label(), "worker :9000");
        let postgres = port(serde_json::json!({
            "port": 5432, "url": "postgres://localhost:5432", "kind": "database",
        }));
        assert!(!postgres.is_openable());
    }

    #[test]
    fn frontend_prefers_the_primary_then_the_lowest_port() {
        let ports = vec![
            port(
                serde_json::json!({ "port": 8000, "url": "http://localhost:8000", "kind": "api", "primary": true }),
            ),
            port(
                serde_json::json!({ "port": 5174, "url": "http://localhost:5174", "kind": "frontend" }),
            ),
            port(
                serde_json::json!({ "port": 5173, "url": "http://localhost:5173", "kind": "frontend" }),
            ),
        ];
        assert_eq!(frontend_port(&ports).map(|port| port.port), Some(5173));
        let mut with_primary = ports.clone();
        with_primary[1].primary = true;
        assert_eq!(
            frontend_port(&with_primary).map(|port| port.port),
            Some(5174)
        );
        assert!(frontend_port(&ports[..1]).is_none());
    }

    #[test]
    fn preview_summary_mentions_commit_age_and_staleness() {
        let now = parse_timestamp("2026-11-15T12:00:00Z").expect("valid timestamp");
        assert_eq!(
            preview("frontend", Some("https://a.example"), true).summary(now),
            "frontend · a1b2c3d · 5 min ago"
        );
        assert_eq!(
            preview("docs", Some("https://b.example"), false).summary(now),
            "docs · a1b2c3d · 5 min ago · behind head"
        );
    }

    #[test]
    fn preview_choice_opens_one_and_picks_among_several() {
        let frontend = preview("frontend", Some("https://a.example"), true);
        let docs = preview("docs", Some("https://b.example"), true);
        let building = preview("storybook", None, true);
        assert_eq!(choose_preview(Vec::new()), PreviewChoice::Nothing);
        assert_eq!(
            choose_preview(openable_previews(
                &[frontend.clone(), building.clone()],
                None
            )),
            PreviewChoice::Open("https://a.example".into())
        );
        let ordered = openable_previews(&[frontend.clone(), docs.clone(), building], Some(&docs));
        assert_eq!(ordered, vec![docs.clone(), frontend.clone()]);
        assert_eq!(
            choose_preview(ordered),
            PreviewChoice::Pick(vec![docs, frontend])
        );
    }

    #[test]
    fn previews_deserialize_with_missing_optional_fields() {
        let list: PreviewList = serde_json::from_value(serde_json::json!({
            "previews": [{ "app": "frontend", "state": "in_progress" }],
            "primary": null,
        }))
        .expect("valid previews");
        assert!(list.previews[0].is_head);
        assert_eq!(list.previews[0].short_sha(), None);
    }
}
