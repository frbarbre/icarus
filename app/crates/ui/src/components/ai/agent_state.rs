use crate::{CommonAnimationExt, prelude::*, rems_from_px};
use gpui::{Hsla, Pixels, px, rgb};

const WORKING_ICON: &str = "icons/icarus/agent_working.svg";
const QUESTION_ICON: &str = "icons/icarus/message_circle_question.svg";
const DONE_ICON: &str = "icons/icarus/circle_check.svg";
const SLEEPING_ICON: &str = "icons/icarus/moon.svg";

const YELLOW_500: u32 = 0xeab308;
const ORANGE_500: u32 = 0xf97316;
const ORANGE_600: u32 = 0xea580c;
const EMERALD_500: u32 = 0x10b981;
const RED_500: u32 = 0xef4444;
const NEUTRAL_500: u32 = 0x737373;

/// What an agent is doing, as Orca's agent state glyphs show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentDotState {
    Working,
    /// Waiting on the user, e.g. to approve a tool call.
    Permission,
    Failed,
    Interrupted,
    Done,
    Idle,
    Sleeping,
}

impl AgentDotState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "Working",
            Self::Permission => "Needs attention",
            Self::Failed => "Failed",
            Self::Interrupted => "Interrupted",
            Self::Done => "Done",
            Self::Idle => "Idle",
            Self::Sleeping => "Sleeping",
        }
    }

    /// The lowercase word agent rows and summaries show beside an agent.
    pub fn summary_label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Permission => "needs attention",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Done => "done",
            Self::Idle => "idle",
            Self::Sleeping => "sleeping",
        }
    }

    /// Lower ranks lead a summary of several agents.
    pub fn summary_rank(self) -> u8 {
        match self {
            Self::Permission => 0,
            Self::Failed => 1,
            Self::Working => 2,
            Self::Interrupted => 3,
            Self::Done => 4,
            Self::Idle => 5,
            Self::Sleeping => 6,
        }
    }
}

/// The state glyph shown beside an agent's identity icon: a spinner while it works.
#[derive(IntoElement)]
pub struct AgentStateIndicator {
    state: AgentDotState,
    size: Pixels,
}

impl AgentStateIndicator {
    pub fn new(state: AgentDotState) -> Self {
        Self {
            state,
            size: px(12.),
        }
    }

    pub fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }
}

impl RenderOnce for AgentStateIndicator {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let light = cx.theme().appearance().is_light();
        let icon_size = IconSize::Custom(rems_from_px(f32::from(self.size)));
        let icon =
            |path: &'static str, color: Color| Icon::from_path(path).size(icon_size).color(color);
        let custom = |hex: u32| Color::Custom(Hsla::from(rgb(hex)));
        let dot = |color: Hsla| {
            div()
                .size(self.size * (2. / 3.))
                .rounded_full()
                .bg(color)
                .into_any_element()
        };
        let glyph = match self.state {
            AgentDotState::Working => icon(WORKING_ICON, custom(YELLOW_500))
                .with_rotate_animation(1)
                .into_any_element(),
            AgentDotState::Permission => icon(
                QUESTION_ICON,
                custom(if light { ORANGE_600 } else { ORANGE_500 }),
            )
            .into_any_element(),
            AgentDotState::Done => icon(DONE_ICON, custom(EMERALD_500)).into_any_element(),
            AgentDotState::Sleeping => icon(SLEEPING_ICON, Color::Muted).into_any_element(),
            AgentDotState::Failed => dot(rgb(RED_500).into()),
            AgentDotState::Interrupted => dot(cx.theme().colors().text_muted),
            AgentDotState::Idle => dot(Hsla::from(rgb(NEUTRAL_500)).opacity(0.4)),
        };
        h_flex()
            .flex_none()
            .size(self.size)
            .justify_center()
            .child(glyph)
    }
}
