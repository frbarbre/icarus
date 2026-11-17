use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use db::kvp::KeyValueStore;
use editor::Editor;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, Render,
    SharedString, Task, WeakEntity, Window, canvas,
};
use project::Project;
use ui::{Tooltip, prelude::*};
use workspace::item::{Item, ItemEvent, SerializableItem};
use workspace::{ItemId, Pane, Workspace, WorkspaceId};

/// How often a browser tab checks that its page is still on screen; GPUI reuses cached panes without painting
/// them, so only a fresh frame tells whether the tab is still shown.
const VISIBILITY_CHECK: Duration = Duration::from_millis(150);

/// What the page reports back from the native view.
#[derive(Debug)]
pub enum BrowserEvent {
    Navigated(String),
    Loaded(String),
    Title(String),
    NewWindow(String),
}

/// A web page in the pane tree, drawn by the system's web engine in a native view over the tab.
pub struct BrowserTab {
    url: SharedString,
    title: Option<SharedString>,
    focus_handle: FocusHandle,
    address: Entity<Editor>,
    native: Rc<native::NativeWebView>,
    painted: Rc<Cell<bool>>,
    workspace: Option<WeakEntity<Workspace>>,
    events: mpsc::UnboundedSender<BrowserEvent>,
    embed: bool,
    _tasks: Vec<Task<()>>,
}

impl BrowserTab {
    pub fn new(url: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let address = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Enter an address", window, cx);
            editor.set_text(url.clone(), window, cx);
            editor
        });
        let (events, mut receiver) = mpsc::unbounded();
        let receive = cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = receiver.next().await {
                if this
                    .update_in(cx, |this, window, cx| this.handle_event(event, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let check = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(VISIBILITY_CHECK).await;
                if this
                    .update_in(cx, |this, window, cx| this.check_visibility(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let handle = window.window_handle();
        let this = cx.entity().downgrade();
        cx.default_global::<BrowserRegistry>()
            .0
            .push((handle, this));
        Self {
            url: url.into(),
            title: None,
            focus_handle: cx.focus_handle(),
            address,
            native: Rc::new(native::NativeWebView::default()),
            painted: Rc::new(Cell::new(false)),
            workspace: None,
            events,
            embed: true,
            _tasks: vec![receive, check],
        }
    }

    /// A tab that shows its toolbar and a placeholder instead of a live page, for visual tests.
    pub fn preview(url: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut tab = Self::new(url, window, cx);
        tab.embed = false;
        tab._tasks.clear();
        tab
    }

    pub fn url(&self) -> &SharedString {
        &self.url
    }

    pub fn title(&self) -> Option<&SharedString> {
        self.title.as_ref()
    }

    /// Whether the page is drawn by a native web view (macOS) or only offered as a link (elsewhere).
    pub fn is_embedded(&self) -> bool {
        native::SUPPORTED
    }

    pub fn navigate(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        let url = normalize_address(&url);
        self.url = url.clone().into();
        self.title = None;
        self.address
            .update(cx, |editor, cx| editor.set_text(url.clone(), window, cx));
        self.native.load(&url);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn handle_event(&mut self, event: BrowserEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            BrowserEvent::Navigated(url) | BrowserEvent::Loaded(url) => {
                if url.is_empty() || url == "about:blank" || self.url.as_ref() == url {
                    return;
                }
                self.url = url.clone().into();
                let editing = self.address.focus_handle(cx).is_focused(window);
                if !editing {
                    self.address
                        .update(cx, |editor, cx| editor.set_text(url, window, cx));
                }
                cx.emit(ItemEvent::UpdateTab);
            }
            BrowserEvent::Title(title) => {
                let title = title.trim().to_string();
                self.title = (!title.is_empty()).then(|| title.into());
                cx.emit(ItemEvent::UpdateTab);
            }
            BrowserEvent::NewWindow(url) => {
                let Some(workspace) = self.workspace.as_ref().and_then(|w| w.upgrade()) else {
                    cx.open_url(&url);
                    return;
                };
                workspace.update(cx, |workspace, cx| {
                    let pane = workspace.active_pane().clone();
                    open_browser_tab(url, &pane, true, workspace, window, cx);
                });
            }
        }
        cx.notify();
    }

    fn check_visibility(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.native.exists() {
            return;
        }
        let covered = cx.has_active_drag()
            || self
                .workspace
                .as_ref()
                .and_then(|workspace| workspace.upgrade())
                .is_some_and(|workspace| {
                    workspace.update(cx, |workspace, cx| workspace.has_active_modal(window, cx))
                });
        if covered {
            self.native.hide();
            return;
        }
        self.painted.set(false);
        cx.notify();
        let painted = self.painted.clone();
        let native = self.native.clone();
        // The first callback runs before the frame this notify causes; the second runs after it was drawn.
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |_, _| {
                if !painted.get() {
                    native.hide();
                }
            });
        });
    }

    fn submit_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.address.read(cx).text(cx);
        if !text.trim().is_empty() {
            self.navigate(text, window, cx);
        }
        window.focus(&self.focus_handle, cx);
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let url = self.url.clone();
        h_flex()
            .h(px(36.))
            .px_1p5()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().toolbar_background)
            .child(
                IconButton::new("browser-back", IconName::ArrowLeft)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Back"))
                    .on_click(cx.listener(|this, _, _, _| this.native.go_back())),
            )
            .child(
                IconButton::new("browser-forward", IconName::ArrowRight)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Forward"))
                    .on_click(cx.listener(|this, _, _, _| this.native.go_forward())),
            )
            .child(
                IconButton::new("browser-reload", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Reload"))
                    .on_click(cx.listener(|this, _, _, _| this.native.reload())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(24.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .bg(cx.theme().colors().editor_background)
                    .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                        this.submit_address(window, cx)
                    }))
                    .child(self.address.clone()),
            )
            .child(
                IconButton::new("browser-external", IconName::ArrowUpRight)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Open in your default browser"))
                    .disabled(url.is_empty())
                    .on_click(move |_, _, cx| cx.open_url(&url)),
            )
    }

    fn render_page(&self, cx: &mut Context<Self>) -> AnyElement {
        if !native::SUPPORTED || !self.embed || self.url.is_empty() {
            return self.render_placeholder(cx).into_any_element();
        }
        let native = self.native.clone();
        let painted = self.painted.clone();
        let url = self.url.to_string();
        let events = self.events.clone();
        let background = cx.theme().colors().editor_background;
        div()
            .size_full()
            .bg(background)
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds: Bounds<Pixels>, _, window, cx| {
                        let bounds = bounds.intersect(&window.content_mask().bounds);
                        if cx.has_active_drag() || bounds.is_empty() {
                            native.hide();
                            return;
                        }
                        painted.set(true);
                        let held_back =
                            covered_by_popover(bounds, &window.deferred_hitbox_bounds());
                        if !held_back {
                            native.place(&url, bounds, window, events.clone());
                        }
                        // Popovers paint after this element, so only the finished frame tells whether one covers the page.
                        window.on_next_frame(move |window, _| {
                            if covered_by_popover(bounds, &window.deferred_hitbox_bounds()) {
                                native.hide();
                            } else if held_back {
                                native.place(&url, bounds, window, events);
                            }
                        });
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }

    fn render_placeholder(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let url = self.url.clone();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .bg(cx.theme().colors().editor_background)
            .child(
                Icon::from_path(crate::links::GLOBE_ICON)
                    .size(IconSize::Medium)
                    .color(Color::Muted),
            )
            .child(
                Label::new(if url.is_empty() {
                    SharedString::from("No address")
                } else {
                    url.clone()
                })
                .color(Color::Muted),
            )
            .when(!url.is_empty(), |this| {
                this.child(
                    Button::new("open-in-browser", "Open in browser")
                        .tooltip(Tooltip::text("Opens the page in your default browser"))
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
            })
    }
}

/// The native page draws above GPUI, so it has to step aside while a menu or popover overlaps it.
fn covered_by_popover(page: Bounds<Pixels>, popovers: &[Bounds<Pixels>]) -> bool {
    popovers.iter().any(|popover| page.intersects(popover))
}

/// A typed address becomes a URL: a bare host gets https, anything with a scheme is kept.
fn normalize_address(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() || text.contains("://") || text.starts_with("about:") {
        return text.to_string();
    }
    if text.starts_with("localhost") || text.starts_with("127.0.0.1") {
        return format!("http://{text}");
    }
    format!("https://{text}")
}

/// Adds a browser tab for `url` to `pane`.
pub fn open_browser_tab(
    url: String,
    pane: &Entity<Pane>,
    focus: bool,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<BrowserTab> {
    let browser = cx.new(|cx| BrowserTab::new(url, window, cx));
    workspace.add_item(
        pane.clone(),
        Box::new(browser.clone()),
        None,
        focus,
        focus,
        window,
        cx,
    );
    browser
}

impl Drop for BrowserTab {
    fn drop(&mut self) {
        self.native.close();
    }
}

impl EventEmitter<ItemEvent> for BrowserTab {}

impl Focusable for BrowserTab {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for BrowserTab {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        if let Some(title) = &self.title {
            return title.clone();
        }
        let host = self
            .url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .filter(|host| !host.is_empty());
        host.map(SharedString::from)
            .unwrap_or_else(|| "Browser".into())
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::from_path(crate::links::GLOBE_ICON).color(Color::Muted))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.url.clone())
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.workspace = Some(workspace.weak_handle());
    }

    fn deactivated(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.native.hide();
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let url = self.url.to_string();
        Task::ready(Some(cx.new(|cx| BrowserTab::new(url, window, cx))))
    }
}

impl Render for BrowserTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("IcarusBrowser")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(self.render_toolbar(cx))
            .child(div().flex_1().min_h_0().child(self.render_page(cx)))
    }
}

fn serialized_browser_key(workspace_id: WorkspaceId, item_id: ItemId) -> String {
    format!("icarus-browser-tab:{}:{item_id}", i64::from(workspace_id))
}

impl SerializableItem for BrowserTab {
    fn serialized_item_kind() -> &'static str {
        "IcarusBrowserTab"
    }

    fn cleanup(
        _workspace_id: WorkspaceId,
        _alive_items: Vec<ItemId>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        Task::ready(Ok(()))
    }

    fn deserialize(
        _project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let saved =
            KeyValueStore::global(cx).read_kvp(&serialized_browser_key(workspace_id, item_id));
        window.spawn(cx, async move |cx| {
            let url = saved?.unwrap_or_default();
            cx.update(|window, cx| cx.new(|cx| BrowserTab::new(url, window, cx)))
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let url = self.url.to_string();
        let store = KeyValueStore::global(cx);
        Some(cx.background_spawn(async move {
            store
                .write_kvp(serialized_browser_key(workspace_id, item_id), url)
                .await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(event, ItemEvent::UpdateTab)
    }
}

pub fn register(cx: &mut App) {
    workspace::register_serializable_item::<BrowserTab>(cx);
}

/// Every browser tab and the window it lives in, so `icarus app capture` can add the pages to its images.
#[derive(Default)]
struct BrowserRegistry(Vec<(gpui::AnyWindowHandle, WeakEntity<BrowserTab>)>);

impl gpui::Global for BrowserRegistry {}

/// The live browser tabs in `window`.
pub fn browsers_in(window: gpui::AnyWindowHandle, cx: &mut App) -> Vec<Entity<BrowserTab>> {
    let registry = cx.default_global::<BrowserRegistry>();
    registry.0.retain(|(_, tab)| tab.upgrade().is_some());
    registry
        .0
        .iter()
        .filter(|(handle, _)| *handle == window)
        .filter_map(|(_, tab)| tab.upgrade())
        .collect()
}

/// A browser tab's state for `icarus app capture`, since the page itself is drawn outside GPUI's frame.
#[derive(serde::Serialize)]
pub struct BrowserCapture {
    pub url: String,
    pub title: Option<String>,
    pub embedded: bool,
    pub shown: bool,
    pub bounds: Option<[f32; 4]>,
}

impl BrowserTab {
    pub fn capture_info(&self) -> BrowserCapture {
        BrowserCapture {
            url: self.url.to_string(),
            title: self.title.as_ref().map(|title| title.to_string()),
            embedded: native::SUPPORTED,
            shown: self.native.is_shown(),
            bounds: self.native.bounds().map(|bounds| {
                [
                    f32::from(bounds.origin.x),
                    f32::from(bounds.origin.y),
                    f32::from(bounds.size.width),
                    f32::from(bounds.size.height),
                ]
            }),
        }
    }

    /// Snapshots the page as PNG bytes; `None` where there's no native view yet.
    pub fn snapshot_png(&self) -> Option<futures::channel::oneshot::Receiver<Option<Vec<u8>>>> {
        self.native.snapshot_png()
    }
}

#[cfg(target_os = "macos")]
mod native {
    use std::cell::{Cell, RefCell};

    use futures::channel::{mpsc, oneshot};
    use gpui::{Bounds, Pixels, Window};
    use util::ResultExt as _;
    use wry::dpi::{LogicalPosition, LogicalSize};
    use wry::{NewWindowResponse, PageLoadEvent, Rect, WebView, WebViewBuilder};

    use super::BrowserEvent;

    pub const SUPPORTED: bool = true;

    #[derive(Default)]
    pub struct NativeWebView {
        view: RefCell<Option<WebView>>,
        bounds: Cell<Option<Bounds<Pixels>>>,
        shown: Cell<bool>,
        failed: Cell<bool>,
    }

    fn rect(bounds: Bounds<Pixels>) -> Rect {
        Rect {
            position: LogicalPosition::new(
                f64::from(f32::from(bounds.origin.x)),
                f64::from(f32::from(bounds.origin.y)),
            )
            .into(),
            size: LogicalSize::new(
                f64::from(f32::from(bounds.size.width)),
                f64::from(f32::from(bounds.size.height)),
            )
            .into(),
        }
    }

    impl NativeWebView {
        pub fn exists(&self) -> bool {
            self.view.borrow().is_some()
        }

        pub fn is_shown(&self) -> bool {
            self.shown.get()
        }

        pub fn bounds(&self) -> Option<Bounds<Pixels>> {
            self.bounds.get()
        }

        /// Creates the web view on first paint, then keeps it over `bounds`.
        pub fn place(
            &self,
            url: &str,
            bounds: Bounds<Pixels>,
            window: &Window,
            events: mpsc::UnboundedSender<BrowserEvent>,
        ) {
            if self.failed.get() {
                return;
            }
            let mut view = self.view.borrow_mut();
            match view.as_ref() {
                Some(existing) => {
                    if self.bounds.get() != Some(bounds) {
                        existing.set_bounds(rect(bounds)).log_err();
                    }
                    if !self.shown.get() {
                        existing.set_visible(true).log_err();
                    }
                }
                None => {
                    let navigated = events.clone();
                    let loaded = events.clone();
                    let titled = events.clone();
                    let opened = events;
                    let built = WebViewBuilder::new()
                        .with_url(url)
                        .with_bounds(rect(bounds))
                        .with_back_forward_navigation_gestures(true)
                        .with_navigation_handler(move |url| {
                            navigated
                                .unbounded_send(BrowserEvent::Navigated(url))
                                .log_err();
                            true
                        })
                        .with_on_page_load_handler(move |event, url| {
                            if let PageLoadEvent::Finished = event {
                                loaded.unbounded_send(BrowserEvent::Loaded(url)).log_err();
                            }
                        })
                        .with_document_title_changed_handler(move |title| {
                            titled.unbounded_send(BrowserEvent::Title(title)).log_err();
                        })
                        .with_new_window_req_handler(move |url, _| {
                            opened
                                .unbounded_send(BrowserEvent::NewWindow(url))
                                .log_err();
                            NewWindowResponse::Deny
                        })
                        .build_as_child(window);
                    match built {
                        Ok(built) => *view = Some(built),
                        Err(error) => {
                            log::error!("browser tab: cannot create the web view: {error}");
                            self.failed.set(true);
                            return;
                        }
                    }
                }
            }
            self.bounds.set(Some(bounds));
            self.shown.set(true);
        }

        pub fn hide(&self) {
            if !self.shown.get() {
                return;
            }
            if let Some(view) = self.view.borrow().as_ref() {
                view.set_visible(false).log_err();
                // A hidden page must not keep the keyboard; GPUI's view takes it back.
                view.focus_parent().log_err();
            }
            self.shown.set(false);
        }

        pub fn close(&self) {
            self.hide();
            self.view.borrow_mut().take();
        }

        pub fn load(&self, url: &str) {
            if let Some(view) = self.view.borrow().as_ref() {
                view.load_url(url).log_err();
            }
        }

        pub fn go_back(&self) {
            if let Some(view) = self.view.borrow().as_ref() {
                view.go_back().log_err();
            }
        }

        pub fn go_forward(&self) {
            if let Some(view) = self.view.borrow().as_ref() {
                view.go_forward().log_err();
            }
        }

        pub fn reload(&self) {
            if let Some(view) = self.view.borrow().as_ref() {
                view.reload().log_err();
            }
        }

        pub fn snapshot_png(&self) -> Option<oneshot::Receiver<Option<Vec<u8>>>> {
            use block2::RcBlock;
            use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage};
            use objc2_foundation::{NSDictionary, NSError};
            use wry::WebViewExtMacOS as _;

            let view = self.view.borrow().as_ref().map(|view| view.webview())?;
            let (sender, receiver) = oneshot::channel();
            let sender = std::cell::RefCell::new(Some(sender));
            let handler = RcBlock::new(move |image: *mut NSImage, _error: *mut NSError| {
                let png = (|| {
                    // SAFETY: WebKit hands the completion handler a valid image or null.
                    let image = unsafe { image.as_ref() }?;
                    let tiff = image.TIFFRepresentation()?;
                    let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
                    // SAFETY: an empty property dictionary is a valid argument.
                    let data = unsafe {
                        rep.representationUsingType_properties(
                            NSBitmapImageFileType::PNG,
                            &NSDictionary::new(),
                        )
                    }?;
                    Some(data.to_vec())
                })();
                if let Some(sender) = sender.borrow_mut().take() {
                    sender.send(png).ok();
                }
            });
            // SAFETY: a nil configuration snapshots the visible page; the handler outlives the call.
            unsafe {
                view.takeSnapshotWithConfiguration_completionHandler(None, &handler);
            }
            Some(receiver)
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use futures::channel::{mpsc, oneshot};
    use gpui::{Bounds, Pixels, Window};

    use super::BrowserEvent;

    // TODO(icarus D31 browser tab): embed WebKitGTK on Linux (plan §3d); pages open in the default browser.
    pub const SUPPORTED: bool = false;

    #[derive(Default)]
    pub struct NativeWebView;

    impl NativeWebView {
        pub fn exists(&self) -> bool {
            false
        }
        pub fn is_shown(&self) -> bool {
            false
        }
        pub fn bounds(&self) -> Option<Bounds<Pixels>> {
            None
        }
        pub fn place(
            &self,
            _url: &str,
            _bounds: Bounds<Pixels>,
            _window: &Window,
            _events: mpsc::UnboundedSender<BrowserEvent>,
        ) {
        }
        pub fn hide(&self) {}
        pub fn close(&self) {}
        pub fn load(&self, _url: &str) {}
        pub fn go_back(&self) {}
        pub fn go_forward(&self) {}
        pub fn reload(&self) {}
        pub fn snapshot_png(&self) -> Option<oneshot::Receiver<Option<Vec<u8>>>> {
            None
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;

    /// A workspace with its own database id and a test key-value store, so tabs can save and restore.
    pub(crate) async fn workspace_with_database(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<Workspace>, gpui::VisualTestContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            cx.set_global(db::AppDatabase::test_new());
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
        let cx = gpui::VisualTestContext::from_window(multi_workspace.into(), cx);
        (workspace, cx)
    }

    #[gpui::test]
    async fn a_browser_tab_restores_its_address(cx: &mut gpui::TestAppContext) {
        let (workspace, mut cx) = workspace_with_database(cx).await;
        let cx = &mut cx;
        let (project, workspace_id, item_id, save) =
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.set_random_database_id();
                let pane = workspace.active_pane().clone();
                let tab = open_browser_tab(
                    "https://github.com/acme/widgets/pull/7".into(),
                    &pane,
                    true,
                    workspace,
                    window,
                    cx,
                );
                let item_id = tab.entity_id().as_u64() as ItemId;
                let save = tab.update(cx, |tab, cx| tab.serialize(workspace, item_id, false, cx));
                (
                    workspace.project().clone(),
                    workspace.database_id().expect("database id"),
                    item_id,
                    save,
                )
            });
        save.expect("saved").await.expect("save");
        let restore = workspace.update_in(cx, |workspace, window, cx| {
            BrowserTab::deserialize(
                project,
                workspace.weak_handle(),
                workspace_id,
                item_id,
                window,
                cx,
            )
        });
        let restored = restore.await.expect("restore");
        restored.read_with(cx, |tab, _| {
            assert_eq!(tab.url().as_ref(), "https://github.com/acme/widgets/pull/7");
        });
    }

    struct PageWithMenu {
        menu_open: bool,
    }

    impl Render for PageWithMenu {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(div().size(px(400.)))
                // Like a dock's resize handle: deferred, but not something to step aside for.
                .child(gpui::deferred(
                    div()
                        .id("resize-handle")
                        .absolute()
                        .top(px(0.))
                        .left(px(396.))
                        .w(px(8.))
                        .h(px(400.))
                        .occlude(),
                ))
                .when(self.menu_open, |this| {
                    this.child(
                        gpui::deferred(
                            gpui::anchored()
                                .position(gpui::point(px(50.), px(60.)))
                                .child(div().id("menu").occlude().w(px(120.)).h(px(80.))),
                        )
                        .with_priority(1),
                    )
                })
        }
    }

    #[gpui::test]
    fn an_open_menu_hides_the_page_under_it(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| PageWithMenu { menu_open: false });
        cx.run_until_parked();
        let page = Bounds::new(gpui::point(px(0.), px(0.)), gpui::size(px(400.), px(400.)));
        let popovers = cx.update(|window, _| window.deferred_hitbox_bounds());
        assert!(!covered_by_popover(page, &popovers));

        view.update(cx, |view, cx| {
            view.menu_open = true;
            cx.notify();
        });
        cx.run_until_parked();
        let popovers = cx.update(|window, _| window.deferred_hitbox_bounds());
        assert!(covered_by_popover(page, &popovers), "{popovers:?}");
        let elsewhere = Bounds::new(
            gpui::point(px(300.), px(300.)),
            gpui::size(px(50.), px(50.)),
        );
        assert!(!covered_by_popover(elsewhere, &popovers));
    }

    #[test]
    fn typed_addresses_become_urls() {
        assert_eq!(
            normalize_address("github.com/a/b"),
            "https://github.com/a/b"
        );
        assert_eq!(normalize_address("localhost:5173"), "http://localhost:5173");
        assert_eq!(
            normalize_address(" https://example.com/x "),
            "https://example.com/x"
        );
        assert_eq!(normalize_address("about:blank"), "about:blank");
    }
}
