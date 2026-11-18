use std::sync::Arc;
use std::time::Duration;

use editor::{Editor, EditorEvent};
use gpui::{
    App, BoxShadow, ClickEvent, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, MouseButton, Render, StyledText, Subscription, Task, TextRun,
    TextStyleRefinement, WeakEntity, Window, anchored, hsla, point, px,
};
use icarus_client::Client;
use serde::Deserialize;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::ModalView;

use crate::changes_tab::SourceControlPanel;

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(200);
const SEARCH_LIMIT: usize = 20;
const MIN_QUERY_CHARS: usize = 2;

/// The server's `workspace.refs` answer.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct BaseRefs {
    /// The workspace's own base; `None` follows the repository's primary branch.
    #[serde(default)]
    pub base_ref: Option<String>,
    #[serde(default)]
    pub default_base: Option<String>,
    #[serde(default)]
    pub remote_count: usize,
    #[serde(default)]
    pub refs: Vec<String>,
}

/// Orca's "Change Base Ref" dialog: what the workspace compares against, and a branch search to
/// pick another.
pub struct BaseRefDialog {
    panel: WeakEntity<SourceControlPanel>,
    workspace_id: String,
    client: Option<Arc<Client>>,
    focus_handle: FocusHandle,
    search: Entity<Editor>,
    pinned: Option<String>,
    default_base: Option<String>,
    remote_count: usize,
    query: String,
    results: Vec<String>,
    searching: bool,
    preview: bool,
    _search: Task<()>,
    _load: Task<()>,
    _edits: Subscription,
}

impl BaseRefDialog {
    pub fn new(
        panel: WeakEntity<SourceControlPanel>,
        workspace_id: String,
        client: Option<Arc<Client>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search branches by name...", window, cx);
            editor.set_text_style_refinement(TextStyleRefinement {
                font_size: Some(px(14.).into()),
                ..Default::default()
            });
            editor
        });
        let edits = cx.subscribe(&search, |this, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                this.query_changed(cx);
            }
        });
        window.focus(&search.focus_handle(cx), cx);
        let load = crate::review::request(
            client.clone(),
            "workspace.refs",
            serde_json::json!({ "id": workspace_id, "query": "", "limit": 1 }),
            cx,
        );
        let load = cx.spawn_in(window, async move |this, cx| {
            let Some(load) = load else {
                return;
            };
            let Some(refs) = load
                .await
                .and_then(|value| Ok(serde_json::from_value::<BaseRefs>(value)?))
                .log_err()
            else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.pinned = refs.base_ref;
                this.default_base = refs.default_base;
                this.remote_count = refs.remote_count;
                // As Orca's dialog opens: focus lands on the first control that takes it, which is
                // Use Primary while a base is pinned and the search otherwise.
                if this.pinned.is_some()
                    && this.query.is_empty()
                    && this.search.focus_handle(cx).is_focused(window)
                {
                    window.focus(&this.focus_handle, cx);
                }
                cx.notify();
            })
            .log_err();
        });
        Self {
            panel,
            workspace_id,
            client,
            focus_handle: cx.focus_handle(),
            search,
            pinned: None,
            default_base: None,
            remote_count: 0,
            query: String::new(),
            results: Vec::new(),
            searching: false,
            preview: false,
            _search: Task::ready(()),
            _load: load,
            _edits: edits,
        }
    }

    /// A dialog that never contacts a server, for visual tests.
    pub fn preview(
        refs: BaseRefs,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut dialog = Self::new(WeakEntity::new_invalid(), String::new(), None, window, cx);
        dialog.preview = true;
        dialog.pinned = refs.base_ref;
        dialog.default_base = refs.default_base;
        dialog.remote_count = refs.remote_count;
        dialog.query = query.to_string();
        dialog.results = refs.refs;
        dialog
            .search
            .update(cx, |editor, cx| editor.set_text(query, window, cx));
        if dialog.pinned.is_some() && query.is_empty() {
            window.focus(&dialog.focus_handle, cx);
        }
        dialog
    }

    pub fn search_editor(&self) -> &Entity<Editor> {
        &self.search
    }

    pub fn results(&self) -> &[String] {
        &self.results
    }

    pub fn pinned(&self) -> Option<&str> {
        self.pinned.as_deref()
    }

    pub fn is_searching(&self) -> bool {
        self.searching
    }

    fn effective(&self) -> Option<&String> {
        self.pinned.as_ref().or(self.default_base.as_ref())
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        self.query = self.search.read(cx).text(cx);
        if self.preview {
            return;
        }
        let trimmed = self.query.trim().to_string();
        if trimmed.chars().count() < MIN_QUERY_CHARS {
            self.results.clear();
            self.searching = false;
            self._search = Task::ready(());
            cx.notify();
            return;
        }
        self.searching = true;
        cx.notify();
        let (client, id) = (self.client.clone(), self.workspace_id.clone());
        self._search = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let Ok(Some(search)) = this.update(cx, |_, cx| {
                crate::review::request(
                    client,
                    "workspace.refs",
                    serde_json::json!({ "id": id, "query": trimmed, "limit": SEARCH_LIMIT }),
                    cx,
                )
            }) else {
                this.update(cx, |this, cx| {
                    this.searching = false;
                    cx.notify();
                })
                .log_err();
                return;
            };
            let found = search
                .await
                .and_then(|value| Ok(serde_json::from_value::<BaseRefs>(value)?))
                .log_err();
            this.update(cx, |this, cx| {
                this.results = found.map(|found| found.refs).unwrap_or_default();
                this.searching = false;
                cx.notify();
            })
            .log_err();
        });
    }

    pub fn select(&mut self, base: String, cx: &mut Context<Self>) {
        self.panel
            .update(cx, |panel, cx| panel.set_base_ref(base, cx))
            .log_err();
        cx.emit(DismissEvent);
    }

    pub fn use_primary(&mut self, cx: &mut Context<Self>) {
        if self.pinned.is_none() {
            return;
        }
        self.select(String::new(), cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// Enter presses Use Primary while it holds focus, as a focused button does in Orca; in the
    /// search it does nothing.
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus_handle.is_focused(window) {
            self.use_primary(cx);
        }
    }

    fn render_current(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let current = self
            .effective()
            .cloned()
            .unwrap_or_else(|| "No default base ref".to_string());
        let detail = match (&self.pinned, &self.default_base) {
            (Some(_), _) => "Pinned for this repo".to_string(),
            (None, Some(default)) => format!("Following primary branch ({default})"),
            (None, None) => "Pick a base branch below".to_string(),
        };
        // No focus ring on Use Primary: Orca's is focus-visible, which a pointer-opened dialog never shows.
        let can_use_primary = self.pinned.is_some();
        h_flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .child(
                v_flex()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(14.))
                            .line_height(px(20.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.text)
                            .child(current),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .text_color(colors.text_muted)
                            .child(detail),
                    )
                    .when(self.remote_count > 1, |column| {
                        column.child(self.render_remote_hint(cx))
                    }),
            )
            .child(
                div()
                    .id("base-ref-use-primary")
                    .debug_selector(|| "base-ref-use-primary".to_string())
                    .flex_none()
                    .h(px(32.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.element_background)
                    .text_size(px(14.))
                    .line_height(px(20.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(colors.text)
                    .when(can_use_primary, |button| {
                        button
                            .cursor_pointer()
                            .hover(|style| style.bg(colors.element_hover))
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.use_primary(cx)),
                            )
                    })
                    .when(!can_use_primary, |button| button.opacity(0.5))
                    .child("Use Primary"),
            )
    }

    fn render_remote_hint(&self, cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let base = theme::theme_settings(cx).ui_font(cx).clone();
        let code = theme::theme_settings(cx).buffer_font(cx).clone();
        let parts = [
            (
                "Multiple remotes detected. Type a remote name (e.g. ",
                false,
            ),
            ("upstream", true),
            (") or a full ref (e.g. ", false),
            ("upstream/main", true),
            (") to scope results.", false),
        ];
        let text: String = parts.iter().map(|(part, _)| *part).collect();
        let runs = parts
            .iter()
            .map(|(part, is_code)| TextRun {
                len: part.len(),
                font: if *is_code { code.clone() } else { base.clone() },
                color: colors.text_muted,
                background_color: None,
                underline: None,
                strikethrough: None,
            })
            .collect();
        div()
            .text_size(px(12.))
            .line_height(px(16.))
            .child(StyledText::new(text).with_runs(runs))
    }

    fn render_results(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let colors = cx.theme().colors().clone();
        let note = |text: &'static str| {
            div()
                .text_size(px(12.))
                .line_height(px(16.))
                .text_color(colors.text_muted)
                .child(text)
                .into_any_element()
        };
        if self.searching {
            return Some(note("Searching branches..."));
        }
        if self.query.trim().chars().count() < MIN_QUERY_CHARS {
            return None;
        }
        if self.results.is_empty() {
            return Some(note("No matching branches found."));
        }
        let effective = self.effective().cloned();
        let rows = self.results.iter().enumerate().map(|(index, name)| {
            let current = effective.as_ref() == Some(name);
            let pick = name.clone();
            let selector = format!("base-ref-result-{name}");
            h_flex()
                .id(("base-ref-result", index))
                .debug_selector(move || selector)
                .w_full()
                .justify_between()
                .items_center()
                .gap(px(8.))
                .rounded(px(6.))
                .px(px(12.))
                .py(px(8.))
                .text_size(px(14.))
                .line_height(px(20.))
                .cursor_pointer()
                .text_color(colors.text)
                .when(current, |row| row.bg(colors.element_selected))
                .hover(|style| style.bg(colors.element_hover))
                .child(div().min_w_0().truncate().child(name.clone()))
                .when(current, |row| row.child(tracked_caps("CURRENT")))
                .on_click(
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.select(pick.clone(), cx)),
                )
        });
        Some(
            div()
                .id("base-ref-results")
                .max_h(px(192.))
                .overflow_y_scroll()
                .rounded(px(8.))
                .border_1()
                .border_color(colors.border.opacity(0.5))
                .child(v_flex().p(px(4.)).children(rows))
                .into_any_element(),
        )
    }
}

/// Orca's `focus-visible:ring-[3px] ring-ring/50`.
fn focus_ring(color: gpui::Hsla) -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: color.opacity(0.5),
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(3.),
        inset: false,
    }]
}

/// Small caps spaced 0.18em apart, as Orca's `tracking-[0.18em]` "Current" tag; GPUI text has no
/// letter spacing.
fn tracked_caps(text: &'static str) -> impl IntoElement {
    h_flex()
        .flex_none()
        .gap(px(1.8))
        .pr(px(1.8))
        .text_size(px(10.))
        .line_height(px(15.))
        .children(text.chars().map(|letter| div().child(letter.to_string())))
}

impl ModalView for BaseRefDialog {}

impl EventEmitter<DismissEvent> for BaseRefDialog {}

impl Focusable for BaseRefDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BaseRefDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let colors = cx.theme().colors().clone();
        let search_focused = self.search.focus_handle(cx).is_focused(window);
        let dialog = v_flex()
            .id("base-ref-dialog")
            .key_context("IcarusBaseRefDialog")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .relative()
            .w(px(512.).min(viewport.width - px(32.)))
            .max_h(px(576.).min(viewport.height * 0.85))
            .p(px(24.))
            .gap(px(16.))
            .rounded(px(10.))
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface_background)
            .text_color(colors.text)
            .shadow(vec![
                BoxShadow {
                    color: hsla(0., 0., 0., 0.55),
                    offset: point(px(0.), px(24.)),
                    blur_radius: px(72.),
                    spread_radius: px(0.),
                    inset: false,
                },
                BoxShadow {
                    color: hsla(0., 0., 1., 0.06),
                    offset: point(px(0.), px(1.)),
                    blur_radius: px(0.),
                    spread_radius: px(0.),
                    inset: true,
                },
            ])
            .child(
                v_flex()
                    .flex_none()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(14.))
                            .line_height(px(19.25))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Change Base Ref"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .text_color(colors.text_muted)
                            .child("Pick the branch compare target for this repository."),
                    ),
            )
            .child(
                v_flex()
                    .id("base-ref-body")
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap(px(10.))
                    .child(self.render_current(cx))
                    .child(
                        div()
                            .h(px(36.))
                            .w_full()
                            .max_w(px(448.))
                            .px(px(12.))
                            .flex()
                            .items_center()
                            .rounded(px(8.))
                            .border_1()
                            .border_color(if search_focused {
                                colors.border_focused
                            } else {
                                colors.border
                            })
                            .bg(colors.element_background)
                            .when(search_focused, |input| {
                                input.shadow(focus_ring(colors.border_focused))
                            })
                            .child(div().w_full().child(self.search.clone())),
                    )
                    .children(self.render_results(cx)),
            )
            .child(
                div()
                    .id("base-ref-close")
                    .debug_selector(|| "base-ref-close".to_string())
                    .absolute()
                    .top(px(16.))
                    .right(px(16.))
                    .rounded(px(2.))
                    .cursor_pointer()
                    .opacity(0.7)
                    .hover(|style| style.opacity(1.))
                    .child(Icon::new(IconName::Close).size(IconSize::Medium))
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(DismissEvent))),
            );
        anchored().position(point(px(0.), px(0.))).child(
            div()
                .id("base-ref-overlay")
                .font(theme::theme_settings(cx).ui_font(cx).clone())
                .size_full()
                .w(viewport.width)
                .h(viewport.height)
                .flex()
                .items_center()
                .justify_center()
                .bg(hsla(0., 0., 0., 0.55))
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
                )
                .child(dialog),
        )
    }
}
