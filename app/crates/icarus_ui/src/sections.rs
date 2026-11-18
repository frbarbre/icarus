//! The section chrome the Source Control and PR panels share (source-control.md §4): a saved order and
//! hidden set, and the 28 px header that collapses on click, moves by drag or its right-click menu, and hides.

use gpui::{AnyElement, App, ClickEvent, Entity, FontWeight, Render, SharedString, Window, px};
use serde_json::{Value, json};
use ui::{ContextMenu, prelude::*, right_click_menu};

pub trait SectionKind: Copy + PartialEq + 'static {
    /// Every section, in the default order.
    fn all() -> &'static [Self];
    /// The id saved in the layout (Orca's ids).
    fn id(self) -> &'static str;
    fn label(self) -> &'static str;

    fn parse(id: &str) -> Option<Self> {
        Self::all()
            .iter()
            .copied()
            .find(|section| section.id() == id)
    }
}

/// A saved `{order, hidden}`. Ids this build doesn't know are kept, so saving never drops them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layout {
    pub order: Vec<String>,
    pub hidden: Vec<String>,
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(str::to_owned))
        .collect()
}

impl Layout {
    pub fn from_settings(value: &Value) -> Self {
        Self {
            order: strings(&value["order"]),
            hidden: strings(&value["hidden"]),
        }
    }

    pub fn to_value(&self) -> Value {
        json!({ "order": self.order, "hidden": self.hidden })
    }

    /// Saved ids first; a section the save doesn't name goes right after its default predecessor.
    fn full_order<S: SectionKind>(&self) -> Vec<String> {
        let mut order: Vec<String> = Vec::new();
        for id in &self.order {
            if !order.contains(id) {
                order.push(id.clone());
            }
        }
        let all = S::all();
        for (index, section) in all.iter().enumerate() {
            if order.iter().any(|id| id == section.id()) {
                continue;
            }
            let at = all[..index]
                .iter()
                .rev()
                .find_map(|previous| order.iter().position(|id| id == previous.id()))
                .map_or(0, |position| position + 1);
            order.insert(at, section.id().to_owned());
        }
        order
    }

    pub fn ordered<S: SectionKind>(&self) -> Vec<S> {
        self.full_order::<S>()
            .iter()
            .filter_map(|id| S::parse(id))
            .collect()
    }

    pub fn visible<S: SectionKind>(&self) -> Vec<S> {
        self.ordered::<S>()
            .into_iter()
            .filter(|section| !self.is_hidden(*section))
            .collect()
    }

    pub fn is_hidden<S: SectionKind>(&self, section: S) -> bool {
        self.hidden.iter().any(|id| id == section.id())
    }

    pub fn toggled_hidden<S: SectionKind>(&self, section: S) -> Self {
        let mut layout = self.clone();
        if layout.is_hidden(section) {
            layout.hidden.retain(|id| id != section.id());
        } else {
            layout.hidden.push(section.id().to_owned());
        }
        layout
    }

    /// `section` goes before `before`, or last.
    pub fn moved<S: SectionKind>(&self, section: S, before: Option<S>) -> Self {
        let mut order = self.full_order::<S>();
        order.retain(|id| id != section.id());
        let at = before
            .filter(|before| *before != section)
            .and_then(|before| order.iter().position(|id| id == before.id()))
            .unwrap_or(order.len());
        order.insert(at, section.id().to_owned());
        Self {
            order,
            hidden: self.hidden.clone(),
        }
    }

    /// A header dropped on another takes the target's place: dragged down it lands after it, up before it.
    pub fn dropped<S: SectionKind>(&self, dragged: S, target: S) -> Self {
        let ordered = self.ordered::<S>();
        let (Some(from), Some(to)) = (
            ordered.iter().position(|item| *item == dragged),
            ordered.iter().position(|item| *item == target),
        ) else {
            return self.clone();
        };
        if from == to {
            return self.clone();
        }
        if from < to {
            self.moved(dragged, ordered.get(to + 1).copied())
        } else {
            self.moved(dragged, Some(target))
        }
    }

    /// Swaps `section` with its visible neighbour above (`up`) or below; `None` at an end.
    pub fn moved_by_one<S: SectionKind>(&self, section: S, up: bool) -> Option<Self> {
        let visible = self.visible::<S>();
        let index = visible.iter().position(|item| *item == section)?;
        if up {
            let above = *visible.get(index.checked_sub(1)?)?;
            Some(self.moved(section, Some(above)))
        } else {
            let below = *visible.get(index + 1)?;
            Some(self.moved(below, Some(section)))
        }
    }
}

/// The ghost that follows the pointer while a header is dragged.
pub fn drag_preview(label: &str, cx: &App) -> impl IntoElement {
    div()
        .px_2()
        .py_0p5()
        .rounded_sm()
        .border_1()
        .border_color(cx.theme().colors().border)
        .bg(cx.theme().colors().elevated_surface_background)
        .child(
            Label::new(label.to_uppercase())
                .size(LabelSize::XSmall)
                .weight(FontWeight::SEMIBOLD),
        )
}

/// A section's header: chevron, uppercase label, count, then any trailing content.
pub struct SectionHeader {
    id: SharedString,
    label: SharedString,
    count: Option<SharedString>,
    collapsed: bool,
    trailing: Vec<AnyElement>,
}

impl SectionHeader {
    pub fn new(id: impl Into<SharedString>, label: &str, collapsed: bool) -> Self {
        Self {
            id: id.into(),
            label: label.to_uppercase().into(),
            count: None,
            collapsed,
            trailing: Vec::new(),
        }
    }

    pub fn count(mut self, count: impl Into<SharedString>) -> Self {
        self.count = Some(count.into());
        self
    }

    pub fn trailing(mut self, element: impl IntoElement) -> Self {
        self.trailing.push(element.into_any_element());
        self
    }

    /// Click toggles, dropping another header of the same panel (`D`) on it moves that one here, and
    /// right-click opens `menu`.
    pub fn render<D: Clone + Render + 'static>(
        self,
        dragged: D,
        on_toggle: impl Fn(&mut Window, &mut App) + 'static,
        on_drop: impl Fn(&D, &mut Window, &mut App) + 'static,
        menu: impl Fn(&mut Window, &mut App) -> Entity<ContextMenu> + 'static,
    ) -> AnyElement {
        let header = h_flex()
            .id(self.id.clone())
            .h(px(28.))
            .flex_none()
            .pl(px(6.))
            .pr(px(16.))
            .gap_1()
            .cursor_pointer()
            .on_click(move |_: &ClickEvent, window, cx| on_toggle(window, cx))
            .on_drag(dragged, |dragged: &D, _, _, cx| cx.new(|_| dragged.clone()))
            .drag_over::<D>(|style, _, _, cx| style.bg(cx.theme().colors().drop_target_background))
            .on_drop(move |dragged: &D, window, cx| on_drop(dragged, window, cx))
            .child(
                Icon::new(if self.collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(
                Label::new(self.label)
                    .size(LabelSize::Custom(rems_from_px(11_f32)))
                    .weight(FontWeight::SEMIBOLD)
                    .color(Color::Muted),
            )
            .when_some(self.count, |header, count| {
                header.child(
                    Label::new(count)
                        .size(LabelSize::XSmall)
                        .weight(FontWeight::MEDIUM)
                        .color(Color::Muted),
                )
            })
            .when(!self.trailing.is_empty(), |header| {
                header.child(div().flex_1()).children(self.trailing)
            });
        right_click_menu(SharedString::from(format!("{}-menu", self.id)))
            .trigger(move |_, _, _| header)
            .menu(menu)
            .into_any_element()
    }
}

/// A section header's menu: move or hide `section`, then a checkbox per section, which is how a hidden
/// one comes back. `on_move(section, up)` and `on_toggle(section)` save the change.
pub fn sections_menu<S: SectionKind>(
    section: Option<S>,
    layout: Layout,
    on_move: impl Fn(S, bool, &mut App) + Clone + 'static,
    on_toggle: impl Fn(S, &mut App) + Clone + 'static,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |mut menu, _, _| {
        if let Some(section) = section {
            for (label, up) in [("Move Up", true), ("Move Down", false)] {
                let on_move = on_move.clone();
                menu = menu.item(
                    ui::ContextMenuEntry::new(label)
                        .disabled(layout.moved_by_one(section, up).is_none())
                        .handler(move |_, cx| on_move(section, up, cx)),
                );
            }
            let hide = on_toggle.clone();
            menu = menu
                .entry("Hide Section", None, move |_, cx| hide(section, cx))
                .separator();
        }
        menu = menu.header("Sections");
        for item in layout.ordered::<S>() {
            let on_toggle = on_toggle.clone();
            menu = menu.toggleable_entry(
                item.label(),
                !layout.is_hidden(item),
                IconPosition::Start,
                None,
                move |_, cx| on_toggle(item, cx),
            );
        }
        menu
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Demo {
        A,
        B,
        C,
    }

    impl SectionKind for Demo {
        fn all() -> &'static [Self] {
            &[Demo::A, Demo::B, Demo::C]
        }

        fn id(self) -> &'static str {
            match self {
                Demo::A => "a",
                Demo::B => "b",
                Demo::C => "c",
            }
        }

        fn label(self) -> &'static str {
            self.id()
        }
    }

    #[test]
    fn missing_sections_follow_their_default_predecessor_and_unknown_ids_stay() {
        let layout =
            Layout::from_settings(&json!({ "order": ["c", "future", "a"], "hidden": ["c"] }));
        assert_eq!(layout.ordered::<Demo>(), [Demo::C, Demo::A, Demo::B]);
        assert_eq!(layout.visible::<Demo>(), [Demo::A, Demo::B]);
        assert_eq!(
            layout.moved(Demo::B, Some(Demo::C)).order,
            ["b", "c", "future", "a"]
        );
        assert_eq!(
            Layout::default()
                .dropped(Demo::A, Demo::C)
                .ordered::<Demo>(),
            [Demo::B, Demo::C, Demo::A]
        );
        assert_eq!(
            layout
                .moved_by_one(Demo::B, true)
                .map(|l| l.visible::<Demo>()),
            Some(vec![Demo::B, Demo::A])
        );
        assert!(layout.toggled_hidden(Demo::C).hidden.is_empty());
    }
}
