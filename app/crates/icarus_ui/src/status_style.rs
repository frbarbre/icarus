//! Status colours and icons, with the same ids and tones as Orca so imported statuses look identical.

use gpui::{App, Hsla, Rgba, SharedString};
use ui::{ActiveTheme as _, Color, Icon, IconSize};

pub struct StatusColor {
    pub id: &'static str,
    pub label: &'static str,
    /// Text tone in light themes and dark themes (Tailwind -600/-700 and -200/-300, like Orca).
    light: u32,
    dark: u32,
    /// Solid swatch used in pickers.
    swatch: u32,
}

pub const STATUS_COLORS: &[StatusColor] = &[
    StatusColor {
        id: "neutral",
        label: "Neutral",
        light: 0,
        dark: 0,
        swatch: 0x71717a,
    },
    StatusColor {
        id: "blue",
        label: "Blue",
        light: 0x2563eb,
        dark: 0x93c5fd,
        swatch: 0x3b82f6,
    },
    StatusColor {
        id: "sky",
        label: "Sky",
        light: 0x0284c7,
        dark: 0x7dd3fc,
        swatch: 0x0ea5e9,
    },
    StatusColor {
        id: "violet",
        label: "Violet",
        light: 0x7c3aed,
        dark: 0xc4b5fd,
        swatch: 0x8b5cf6,
    },
    StatusColor {
        id: "amber",
        label: "Amber",
        light: 0xb45309,
        dark: 0xfde68a,
        swatch: 0xf59e0b,
    },
    StatusColor {
        id: "emerald",
        label: "Emerald",
        light: 0x047857,
        dark: 0xa7f3d0,
        swatch: 0x10b981,
    },
    StatusColor {
        id: "rose",
        label: "Rose",
        light: 0xe11d48,
        dark: 0xfda4af,
        swatch: 0xf43f5e,
    },
    StatusColor {
        id: "zinc",
        label: "Zinc",
        light: 0x52525b,
        dark: 0xd4d4d8,
        swatch: 0x71717a,
    },
    StatusColor {
        id: "conductor-done",
        label: "Done",
        light: 0xc7a594,
        dark: 0xc7a594,
        swatch: 0xc7a594,
    },
    StatusColor {
        id: "conductor-review",
        label: "Review",
        light: 0x16a34a,
        dark: 0x16a34a,
        swatch: 0x16a34a,
    },
    StatusColor {
        id: "conductor-progress",
        label: "Progress",
        light: 0xd4a300,
        dark: 0xd4a300,
        swatch: 0xd4a300,
    },
];

pub struct StatusIcon {
    pub id: &'static str,
    pub label: &'static str,
    path: &'static str,
}

pub const STATUS_ICONS: &[StatusIcon] = &[
    StatusIcon {
        id: "circle",
        label: "Circle",
        path: "icons/icarus/circle.svg",
    },
    StatusIcon {
        id: "circle-dot",
        label: "Dot",
        path: "icons/icarus/circle_dot.svg",
    },
    StatusIcon {
        id: "circle-progress",
        label: "Progress",
        path: "icons/icarus/conductor_progress.svg",
    },
    StatusIcon {
        id: "circle-dashed",
        label: "Dashed",
        path: "icons/icarus/circle_dashed.svg",
    },
    StatusIcon {
        id: "circle-ellipsis",
        label: "Ellipsis",
        path: "icons/icarus/circle_ellipsis.svg",
    },
    StatusIcon {
        id: "git-pull-request",
        label: "Pull request",
        path: "icons/icarus/conductor_review.svg",
    },
    StatusIcon {
        id: "timer",
        label: "Timer",
        path: "icons/icarus/timer.svg",
    },
    StatusIcon {
        id: "flag",
        label: "Flag",
        path: "icons/icarus/flag.svg",
    },
    StatusIcon {
        id: "circle-alert",
        label: "Alert",
        path: "icons/icarus/circle_alert.svg",
    },
    StatusIcon {
        id: "circle-pause",
        label: "Pause",
        path: "icons/icarus/circle_pause.svg",
    },
    StatusIcon {
        id: "circle-play",
        label: "Play",
        path: "icons/icarus/circle_play.svg",
    },
    StatusIcon {
        id: "circle-check",
        label: "Check",
        path: "icons/icarus/conductor_done.svg",
    },
    StatusIcon {
        id: "ban",
        label: "Ban",
        path: "icons/icarus/ban.svg",
    },
    StatusIcon {
        id: "conductor-done",
        label: "Done",
        path: "icons/icarus/conductor_done.svg",
    },
    StatusIcon {
        id: "conductor-review",
        label: "Review",
        path: "icons/icarus/conductor_review.svg",
    },
    StatusIcon {
        id: "conductor-progress",
        label: "In progress",
        path: "icons/icarus/conductor_progress.svg",
    },
];

/// Orca's defaults for its built-in status ids when a status has no colour or icon of its own.
fn default_visual(status_id: &str) -> (&'static str, &'static str) {
    match status_id {
        "todo" => ("neutral", "circle"),
        "in-progress" => ("conductor-progress", "conductor-progress"),
        "in-review" => ("conductor-review", "conductor-review"),
        "completed" => ("conductor-done", "conductor-done"),
        _ => ("neutral", "circle-dot"),
    }
}

fn hsla(rgb: u32) -> Hsla {
    Rgba {
        r: ((rgb >> 16) & 0xff) as f32 / 255.0,
        g: ((rgb >> 8) & 0xff) as f32 / 255.0,
        b: (rgb & 0xff) as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

pub fn color_option(id: &str) -> Option<&'static StatusColor> {
    STATUS_COLORS.iter().find(|color| color.id == id)
}

pub fn icon_option(id: &str) -> Option<&'static StatusIcon> {
    STATUS_ICONS.iter().find(|icon| icon.id == id)
}

/// The text colour for a status in the current theme's appearance.
pub fn status_color(status_id: &str, color: Option<&str>, cx: &App) -> Color {
    let id = color.unwrap_or_else(|| default_visual(status_id).0);
    match color_option(id) {
        Some(option) if option.id != "neutral" => {
            let rgb = if cx.theme().appearance().is_light() {
                option.light
            } else {
                option.dark
            };
            Color::Custom(hsla(rgb))
        }
        _ => Color::Muted,
    }
}

pub fn swatch_color(color_id: &str) -> Color {
    Color::Custom(hsla(
        color_option(color_id).map_or(0x71717a, |option| option.swatch),
    ))
}

pub fn icon_path(status_id: &str, icon: Option<&str>) -> SharedString {
    let id = icon.unwrap_or_else(|| default_visual(status_id).1);
    let path = icon_option(id)
        .or_else(|| icon_option("circle-dot"))
        .map_or("icons/icarus/circle_dot.svg", |option| option.path);
    SharedString::new_static(path)
}

pub fn status_icon(status_id: &str, color: Option<&str>, icon: Option<&str>, cx: &App) -> Icon {
    Icon::from_path(icon_path(status_id, icon))
        .size(IconSize::Small)
        .color(status_color(status_id, color, cx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_orca_color_and_icon_id_is_known() {
        for id in [
            "neutral",
            "blue",
            "sky",
            "violet",
            "amber",
            "emerald",
            "rose",
            "zinc",
            "conductor-done",
            "conductor-review",
            "conductor-progress",
        ] {
            assert!(color_option(id).is_some(), "{id}");
        }
        for id in [
            "circle",
            "circle-dot",
            "circle-progress",
            "circle-dashed",
            "circle-ellipsis",
            "git-pull-request",
            "timer",
            "flag",
            "circle-alert",
            "circle-pause",
            "circle-play",
            "circle-check",
            "ban",
            "conductor-done",
            "conductor-review",
            "conductor-progress",
        ] {
            assert!(icon_option(id).is_some(), "{id}");
        }
    }

    #[test]
    fn icon_files_exist() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        for icon in STATUS_ICONS {
            assert!(root.join(icon.path).exists(), "{}", icon.path);
        }
    }

    #[test]
    fn unknown_ids_fall_back_like_orca() {
        assert_eq!(
            icon_path("custom", Some("nope")).as_ref(),
            "icons/icarus/circle_dot.svg"
        );
        assert_eq!(
            icon_path("in-review", None).as_ref(),
            "icons/icarus/conductor_review.svg"
        );
    }
}
