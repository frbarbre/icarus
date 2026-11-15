use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString, Task,
    WeakEntity, Window,
};
use project::Project;
use project_panel::ProjectPanel;
use ui::prelude::*;
use workspace::item::{Item, ItemEvent, SerializableItem};
use workspace::{ItemId, Pane, Workspace, WorkspaceId};

/// The file tree as a centre tab, so it can sit in any split; the dock's project panel keeps working beside it.
pub struct ExplorerTab {
    panel: Entity<ProjectPanel>,
}

impl ExplorerTab {
    pub fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        Self {
            panel: ProjectPanel::new(workspace, window, cx),
        }
    }

    pub fn panel(&self) -> &Entity<ProjectPanel> {
        &self.panel
    }
}

/// Adds an explorer tab to `pane`.
pub fn open_explorer_tab(
    pane: &Entity<Pane>,
    focus: bool,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<ExplorerTab> {
    let explorer = ExplorerTab::new(workspace, window, cx);
    let explorer = cx.new(|_| explorer);
    workspace.add_item(
        pane.clone(),
        Box::new(explorer.clone()),
        None,
        focus,
        focus,
        window,
        cx,
    );
    explorer
}

impl EventEmitter<ItemEvent> for ExplorerTab {}

impl Focusable for ExplorerTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.panel.focus_handle(cx)
    }
}

impl Item for ExplorerTab {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Explorer".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileTree).color(Color::Muted))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for ExplorerTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.panel.clone())
    }
}

impl SerializableItem for ExplorerTab {
    fn serialized_item_kind() -> &'static str {
        "IcarusExplorerTab"
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
        workspace: WeakEntity<Workspace>,
        _workspace_id: WorkspaceId,
        _item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        window.spawn(cx, async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                let explorer = ExplorerTab::new(workspace, window, cx);
                cx.new(|_| explorer)
            })
        })
    }

    fn serialize(
        &mut self,
        _workspace: &mut Workspace,
        _item_id: ItemId,
        _closing: bool,
        _cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        Some(Task::ready(Ok(())))
    }

    fn should_serialize(&self, _event: &Self::Event) -> bool {
        false
    }
}

pub fn register(cx: &mut App) {
    workspace::register_serializable_item::<ExplorerTab>(cx);
}
