//! A small fuzzy palette shared by the workspace switcher (⌘J) and the quick commands menu.

use std::sync::Arc;

use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{App, Context, DismissEvent, Entity, EventEmitter, Focusable, Render, Window};
use picker::{Picker, PickerDelegate};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::ModalView;

#[derive(Debug, Clone, PartialEq)]
pub struct PaletteItem {
    pub label: String,
    pub detail: Option<String>,
    /// Extra text the filter matches on but the row doesn't show (branch, issue id, status…).
    pub keywords: String,
}

type OnConfirm = Box<dyn Fn(usize, &mut Window, &mut App)>;

pub struct Palette {
    picker: Entity<Picker<PaletteDelegate>>,
}

impl Palette {
    pub fn new(
        placeholder: &'static str,
        items: Vec<PaletteItem>,
        on_confirm: impl Fn(usize, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = PaletteDelegate {
            palette: cx.entity().downgrade(),
            placeholder,
            matches: all_matches(&items),
            items,
            selected_index: 0,
            on_confirm: Box::new(on_confirm),
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }

    pub fn picker(&self) -> &Entity<Picker<PaletteDelegate>> {
        &self.picker
    }
}

fn haystack(item: &PaletteItem) -> String {
    format!("{} {}", item.label, item.keywords)
}

fn all_matches(items: &[PaletteItem]) -> Vec<StringMatch> {
    items
        .iter()
        .enumerate()
        .map(|(index, item)| StringMatch {
            candidate_id: index,
            score: 0.0,
            positions: Vec::new(),
            string: haystack(item),
        })
        .collect()
}

impl EventEmitter<DismissEvent> for Palette {}
impl ModalView for Palette {}

impl Focusable for Palette {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for Palette {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("IcarusPalette")
            .w(rems(34.))
            .child(self.picker.clone())
    }
}

pub struct PaletteDelegate {
    palette: gpui::WeakEntity<Palette>,
    placeholder: &'static str,
    items: Vec<PaletteItem>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    on_confirm: OnConfirm,
}

impl PaletteDelegate {
    /// The original indices of the rows now shown, in order.
    pub fn visible(&self) -> Vec<usize> {
        self.matches.iter().map(|m| m.candidate_id).collect()
    }
}

impl PickerDelegate for PaletteDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "icarus palette"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        self.placeholder.into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
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
    ) -> gpui::Task<()> {
        if query.is_empty() {
            self.matches = all_matches(&self.items);
            self.selected_index = 0;
            return gpui::Task::ready(());
        }
        let background = cx.background_executor().clone();
        let candidates: Vec<StringMatchCandidate> = self
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| StringMatchCandidate::new(index, &haystack(item)))
            .collect();
        cx.spawn_in(window, async move |this, cx| {
            let matches = match_strings(
                &candidates,
                &query,
                false,
                true,
                100,
                &Default::default(),
                background,
            )
            .await;
            this.update(cx, |this, cx| {
                this.delegate.matches = matches;
                this.delegate.selected_index = 0;
                cx.notify();
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(index) = self
            .matches
            .get(self.selected_index)
            .map(|m| m.candidate_id)
        else {
            return;
        };
        (self.on_confirm)(index, window, cx);
        self.palette
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.palette
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let found = self.matches.get(index)?;
        let item = self.items.get(found.candidate_id)?;
        let label_length = item.label.len();
        let positions: Vec<usize> = found
            .positions
            .iter()
            .copied()
            .filter(|position| *position < label_length)
            .collect();
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(HighlightedLabel::new(item.label.clone(), positions))
                        .child(div().flex_1())
                        .children(item.detail.clone().map(|detail| {
                            Label::new(detail)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                        })),
                ),
        )
    }
}
