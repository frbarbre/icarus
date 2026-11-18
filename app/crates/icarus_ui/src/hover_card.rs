use std::time::Duration;

use gpui::{
    Anchor, BoxShadow, Entity, EntityId, FontWeight, Global, Hsla, Stateful, Task, anchored,
    deferred, hsla, point, rgb,
};
use ui::prelude::*;

use crate::WorkspacesPanel;
use crate::cards;
use crate::model::WorkspaceSummary;

const OPEN_DELAY: Duration = Duration::from_millis(250);
const CLOSE_DELAY: Duration = Duration::from_millis(120);
const SIDE_OFFSET: f32 = 8.;
const CIRCLE_DOT_ICON: &str = "icons/icarus/circle_dot.svg";

fn font_px(value: f32) -> Rems {
    rems_from_px(value)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Details {
    pub title: Option<String>,
    pub branch: Option<String>,
    pub reviews: Vec<Reference>,
    pub issues: Vec<Reference>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reference {
    pub title: String,
    pub label: String,
    pub provider: Option<&'static str>,
    pub source: Option<String>,
    pub source_url: Option<String>,
    pub url: Option<String>,
    pub pr_state: Option<String>,
    pub issue_state: Option<IssueState>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IssueState {
    pub name: String,
    pub kind: String,
}

impl Details {
    pub fn of(workspace: &WorkspaceSummary) -> Self {
        let branch = workspace.branch.clone().filter(|branch| !branch.is_empty());
        let title = Some(workspace.display_title()).filter(|title| Some(title) != branch.as_ref());
        let reviews = workspace
            .pr
            .iter()
            .map(|pr| {
                let repo = github_repo(&pr.url);
                Reference {
                    title: if pr.title.is_empty() {
                        format!("PR #{}", pr.number)
                    } else {
                        pr.title.clone()
                    },
                    label: format!("PR #{}", pr.number),
                    provider: None,
                    source_url: repo
                        .as_ref()
                        .map(|repo| format!("https://github.com/{repo}")),
                    source: repo,
                    url: Some(pr.url.clone()).filter(|url| !url.is_empty()),
                    pr_state: Some(pr.state.clone()),
                    issue_state: None,
                }
            })
            .collect();
        let issues = workspace
            .issue
            .iter()
            .filter_map(|issue| {
                let id = issue["id"].as_str()?.to_string();
                let url = workspace.issue_url();
                Some(Reference {
                    title: issue["title"]
                        .as_str()
                        .filter(|title| !title.is_empty())
                        .map_or_else(|| id.clone(), str::to_string),
                    label: id,
                    provider: Some("Linear"),
                    source: url.as_deref().and_then(linear_workspace),
                    source_url: None,
                    url,
                    pr_state: None,
                    issue_state: issue["state"]["name"].as_str().map(|name| IssueState {
                        name: name.to_string(),
                        kind: issue["state"]["type"].as_str().unwrap_or_default().into(),
                    }),
                })
            })
            .collect();
        Self {
            title,
            branch,
            reviews,
            issues,
        }
    }
}

fn github_repo(url: &str) -> Option<String> {
    let path = url.strip_prefix("https://github.com/")?;
    let mut parts = path.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    (!owner.is_empty() && !repo.is_empty()).then(|| format!("{owner}/{repo}"))
}

fn linear_workspace(url: &str) -> Option<String> {
    let org = url.strip_prefix("https://linear.app/")?.split('/').next()?;
    (!org.is_empty()).then(|| org.to_string())
}

/// Linear's desktop app takes the web path under its own scheme.
pub fn issue_link(url: &str, linear_app: bool) -> String {
    match url.strip_prefix("https://linear.app/") {
        Some(path) if linear_app => format!("linear://{path}"),
        _ => url.to_string(),
    }
}

fn linear_app_installed() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    std::iter::once(std::path::PathBuf::from("/Applications"))
        .chain(home.map(|home| home.join("Applications")))
        .any(|dir| dir.join("Linear.app").exists())
}

pub fn open_issue(url: &str, cx: &mut App) {
    cx.open_url(&issue_link(url, linear_app_installed()));
}

#[derive(Default)]
struct HoverCards {
    target: Option<(EntityId, String)>,
    open: bool,
    on_card: bool,
    on_popover: bool,
    timer: Option<Task<()>>,
}

impl Global for HoverCards {}

impl HoverCards {
    fn shows(&self, panel: EntityId, id: &str) -> bool {
        self.open
            && self
                .target
                .as_ref()
                .is_some_and(|(p, target)| *p == panel && target == id)
    }
}

#[derive(Clone, Copy)]
enum Part {
    Card,
    Popover,
}

fn pointer(
    panel: EntityId,
    id: &str,
    part: Part,
    hovered: bool,
    cx: &mut Context<WorkspacesPanel>,
) {
    let key = (panel, id.to_string());
    let state = cx.default_global::<HoverCards>();
    if state.target.as_ref() != Some(&key) {
        if !hovered {
            return;
        }
        let was_open = state.open;
        *state = HoverCards {
            target: Some(key.clone()),
            ..HoverCards::default()
        };
        if was_open {
            cx.notify();
        }
    }
    let state = cx.default_global::<HoverCards>();
    match part {
        Part::Card => state.on_card = hovered,
        Part::Popover => state.on_popover = hovered,
    }
    let wanted = state.on_card || state.on_popover;
    if wanted == state.open {
        state.timer = None;
        return;
    }
    let delay = if wanted { OPEN_DELAY } else { CLOSE_DELAY };
    let timer = cx.spawn(async move |panel, cx| {
        cx.background_executor().timer(delay).await;
        panel
            .update(cx, |_, cx| {
                let state = cx.default_global::<HoverCards>();
                if state.target.as_ref() != Some(&key) {
                    return;
                }
                state.open = state.on_card || state.on_popover;
                if !state.open {
                    state.target = None;
                }
                cx.notify();
            })
            .ok();
    });
    cx.default_global::<HoverCards>().timer = Some(timer);
}

/// Opens a card's details at once, for previews.
pub fn show(panel: &Entity<WorkspacesPanel>, id: &str, cx: &mut App) {
    let target = Some((panel.entity_id(), id.to_string()));
    cx.set_global(HoverCards {
        target,
        open: true,
        on_card: true,
        ..HoverCards::default()
    });
    panel.update(cx, |_, cx| cx.notify());
}

/// Orca's worktree hover card: the card's details beside it after a short hover.
pub(crate) fn attach(
    card: Stateful<Div>,
    workspace: &WorkspaceSummary,
    cx: &mut Context<WorkspacesPanel>,
) -> Stateful<Div> {
    let panel = cx.entity_id();
    let id = workspace.id.clone();
    let open = cx
        .try_global::<HoverCards>()
        .is_some_and(|state| state.shows(panel, &id));
    let popover = open.then(|| render(&Details::of(workspace), panel, &id, cx));
    card.on_hover(
        cx.listener(move |_, hovered: &bool, _, cx| pointer(panel, &id, Part::Card, *hovered, cx)),
    )
    .when_some(popover, |card, popover| {
        card.relative().child(
            div().absolute().top_0().right_0().child(
                deferred(
                    anchored()
                        .anchor(Anchor::TopLeft)
                        .offset(point(px(SIDE_OFFSET + 1.), px(-1.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(popover),
                )
                .with_priority(1),
            ),
        )
    })
}

fn surface(cx: &App) -> (Hsla, Hsla, BoxShadow) {
    let base = cx.theme().colors().elevated_surface_background;
    let shadow = |alpha: f32, y: f32, blur: f32| BoxShadow {
        color: hsla(0., 0., 0., alpha),
        offset: point(px(0.), px(y)),
        blur_radius: px(blur),
        spread_radius: px(0.),
        inset: false,
    };
    if cx.theme().appearance().is_light() {
        (
            base.blend(hsla(0., 0., 1., 0.82)),
            hsla(0., 0., 0., 0.14),
            shadow(0.24, 16., 36.),
        )
    } else {
        (
            base.blend(hsla(0., 0., 0., 0.72)),
            hsla(0., 0., 1., 0.14),
            shadow(0.42, 20., 44.),
        )
    }
}

fn render(
    details: &Details,
    panel: EntityId,
    id: &str,
    cx: &mut Context<WorkspacesPanel>,
) -> AnyElement {
    let colors = cx.theme().colors();
    let (background, border, shadow) = surface(cx);
    let hover_id = id.to_string();
    let groups = [
        ("Reviews", &details.reviews),
        ("Issues & tasks", &details.issues),
    ];
    v_flex()
        .id(SharedString::from(format!("icarus-hover-card-{id}")))
        .debug_selector(|| "icarus-hover-card".into())
        .occlude()
        .w(px(360.))
        .max_h(px(448.))
        .overflow_y_scroll()
        .p_3()
        .gap_3()
        .rounded_md()
        .border_1()
        .border_color(border)
        .bg(background)
        .shadow(vec![shadow])
        .text_size(font_px(12.))
        .text_color(colors.text)
        .on_hover(cx.listener(move |_, hovered: &bool, _, cx| {
            pointer(panel, &hover_id, Part::Popover, *hovered, cx)
        }))
        .on_click(|_, _, cx| cx.stop_propagation())
        .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .when(
            details.title.is_some() || details.branch.is_some(),
            |card| {
                card.child(
                    v_flex()
                        .min_w_0()
                        .gap_1()
                        .when_some(details.title.clone(), |header, title| {
                            header.child(
                                div()
                                    .text_size(font_px(13.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .line_height(font_px(13. * 1.375))
                                    .child(title),
                            )
                        })
                        .when_some(details.branch.clone(), |header, branch| {
                            header.child(
                                div()
                                    .font(theme::theme_settings(cx).buffer_font(cx).clone())
                                    .text_size(font_px(11.))
                                    .line_height(font_px(11. * 1.375))
                                    .text_color(colors.text_muted)
                                    .child(branch),
                            )
                        }),
                )
            },
        )
        .when(
            !details.reviews.is_empty() || !details.issues.is_empty(),
            |card| {
                card.child(
                    v_flex().gap_1p5().children(
                        groups
                            .into_iter()
                            .filter(|(_, rows)| !rows.is_empty())
                            .map(|(label, rows)| render_group(label, rows, id, cx)),
                    ),
                )
            },
        )
        .into_any_element()
}

fn render_group(label: &str, rows: &[Reference], id: &str, cx: &App) -> impl IntoElement {
    let colors = cx.theme().colors();
    v_flex()
        .gap_1p5()
        .child(
            h_flex()
                .gap_1p5()
                .text_size(font_px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.text_muted)
                .child(
                    Icon::new(IconName::Link)
                        .size(IconSize::Custom(font_px(12.)))
                        .color(Color::Muted),
                )
                .child(format!("{} · {}", label.to_uppercase(), rows.len())),
        )
        .child(
            v_flex().children(rows.iter().enumerate().map(|(index, row)| {
                div()
                    .py_1p5()
                    .when(index > 0, |row| {
                        row.border_t_1().border_color(colors.border)
                    })
                    .child(render_reference(row, &format!("{id}-{label}-{index}"), cx))
            })),
        )
}

fn state_tone(kind: &str, light: bool) -> Option<(u32, u32, IconName)> {
    match kind {
        "completed" => Some((
            0xa855f7,
            if light { 0x9333ea } else { 0xd8b4fe },
            IconName::CircleCheck,
        )),
        "canceled" => Some((
            0xf43f5e,
            if light { 0xe11d48 } else { 0xfda4af },
            IconName::XCircle,
        )),
        "started" => Some((
            0xf59e0b,
            if light { 0xd97706 } else { 0xfcd34d },
            IconName::Clock,
        )),
        _ => None,
    }
}

fn render_state(state: &IssueState, cx: &App) -> impl IntoElement {
    let colors = cx.theme().colors();
    let tone = state_tone(&state.kind, cx.theme().appearance().is_light());
    let (border, background, text, icon) = match tone {
        Some((accent, text, icon)) => {
            let accent = Hsla::from(rgb(accent));
            (
                accent.opacity(0.25),
                accent.opacity(0.05),
                Hsla::from(rgb(text)),
                Icon::new(icon),
            )
        }
        None => (
            colors.border,
            colors.element_background.opacity(0.3),
            colors.text_muted,
            Icon::from_path(CIRCLE_DOT_ICON),
        ),
    };
    h_flex()
        .h(px(16.))
        .px(px(6.))
        .gap_1()
        .rounded(px(4.))
        .border_1()
        .border_color(border)
        .bg(background)
        .text_size(font_px(9.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(text)
        .child(
            icon.size(IconSize::Custom(font_px(10.)))
                .color(Color::Custom(text)),
        )
        .child(format!("State: {}", state.name))
}

fn render_reference(row: &Reference, id: &str, cx: &App) -> impl IntoElement {
    let colors = cx.theme().colors();
    let icon = match &row.pr_state {
        Some(state) => Icon::new(IconName::PullRequest).color(cards::pr_state_color(state)),
        None => Icon::from_path(CIRCLE_DOT_ICON).color(Color::Muted),
    };
    let is_issue = row.provider == Some("Linear");
    let title = div()
        .id(SharedString::from(format!("icarus-hover-ref-{id}")))
        .debug_selector({
            let label = row.label.clone();
            move || format!("icarus-hover-link:{label}")
        })
        .w_full()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .font_weight(FontWeight::MEDIUM)
        .line_height(font_px(12. * 1.375))
        .child(row.title.clone())
        .when_some(row.url.clone(), |title, url| {
            let decoration = colors.text_muted.opacity(0.5);
            title
                .cursor_pointer()
                .text_decoration_1()
                .text_decoration_solid()
                .text_decoration_color(decoration)
                .hover(|style| style.text_decoration_color(colors.text))
                .tooltip(ui::Tooltip::text(url.clone()))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    if is_issue {
                        open_issue(&url, cx);
                    } else {
                        cx.open_url(&url);
                    }
                })
        });
    let source = row.source.clone().map(|source| {
        let text = match row.provider {
            Some(provider) => format!("· {provider} · {source}"),
            None => format!("· {source}"),
        };
        match row.source_url.clone() {
            Some(url) => div()
                .id(SharedString::from(format!("icarus-hover-src-{id}")))
                .min_w_0()
                .truncate()
                .cursor_pointer()
                .hover(|style| style.text_color(colors.text))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    cx.open_url(&url);
                })
                .child(text)
                .into_any_element(),
            None => div().min_w_0().truncate().child(text).into_any_element(),
        }
    });
    v_flex()
        .gap_1()
        .child(
            h_flex()
                .min_w_0()
                .items_start()
                .gap_2()
                .child(
                    div()
                        .mt_0p5()
                        .w(px(14.))
                        .flex_none()
                        .child(icon.size(IconSize::Custom(font_px(14.)))),
                )
                .child(
                    v_flex().min_w_0().flex_1().child(title).child(
                        h_flex()
                            .mt_0p5()
                            .min_w_0()
                            .gap_1p5()
                            .text_size(font_px(11.))
                            .text_color(colors.text_muted)
                            .when(row.provider.is_none(), |line| {
                                line.child(
                                    Icon::new(IconName::Github)
                                        .size(IconSize::Custom(font_px(12.)))
                                        .color(Color::Muted),
                                )
                            })
                            .child(div().flex_none().child(row.label.clone()))
                            .when_some(
                                row.provider.filter(|_| row.source.is_none()),
                                |line, provider| {
                                    line.child(div().flex_none().child(format!("· {provider}")))
                                },
                            )
                            .children(source),
                    ),
                ),
        )
        .when_some(row.issue_state.as_ref(), |row, state| {
            row.child(h_flex().pl_5().child(render_state(state, cx)))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PrSummary;

    fn linked() -> WorkspaceSummary {
        let mut workspace = crate::tests::summary("e-6965", "/w/e-6965", None);
        workspace.branch = Some("feature/e-6965-editor-lazy-load-frames".into());
        workspace.pr = Some(PrSummary {
            number: 4016,
            title: "perf(frontend): lazy-load frames".into(),
            url: "https://github.com/acme/app/pull/4016".into(),
            state: "open".into(),
            ..PrSummary::default()
        });
        workspace.issue = Some(serde_json::json!({
            "provider": "linear",
            "id": "E-6965",
            "title": "Editor: lazy-load frames",
            "url": "https://linear.app/acme/issue/E-6965/editor-lazy-load-frames",
            "state": { "name": "In Review", "type": "started", "color": "#f2c94c" },
        }));
        workspace
    }

    #[test]
    fn details_read_like_orcas_card() {
        let details = Details::of(&linked());
        assert_eq!(details.title.as_deref(), Some("E-6965"));
        let pr = &details.reviews[0];
        assert_eq!(
            (
                pr.label.as_str(),
                pr.source.as_deref(),
                pr.source_url.as_deref()
            ),
            (
                "PR #4016",
                Some("acme/app"),
                Some("https://github.com/acme/app")
            )
        );
        let issue = &details.issues[0];
        assert_eq!(
            (
                issue.label.as_str(),
                issue.provider,
                issue.source.as_deref()
            ),
            ("E-6965", Some("Linear"), Some("acme"))
        );
        assert_eq!(issue.issue_state.as_ref().unwrap().kind, "started");
    }

    #[test]
    fn issues_open_in_the_linear_app_when_it_is_installed() {
        let url = "https://linear.app/acme/issue/E-1/x";
        assert_eq!(issue_link(url, true), "linear://acme/issue/E-1/x");
        assert_eq!(issue_link(url, false), url);
    }

    #[gpui::test]
    async fn hovering_a_card_shows_its_pr_and_issue_after_a_delay(cx: &mut gpui::TestAppContext) {
        let (_, _, mut cx) = crate::tests::panel_window(vec![linked()], cx).await;
        let cx = &mut cx;
        let card = cx
            .debug_bounds("card:ws-e-6965")
            .expect("the card is drawn");
        cx.simulate_mouse_move(card.center(), None, gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("icarus-hover-card").is_none());
        cx.executor().advance_clock(OPEN_DELAY);
        cx.run_until_parked();
        let popover = cx
            .debug_bounds("icarus-hover-card")
            .expect("the hover card opens");
        assert!(popover.left() >= card.right());

        let pr = cx.debug_bounds("icarus-hover-link:PR #4016").unwrap();
        cx.simulate_click(pr.center(), gpui::Modifiers::default());
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://github.com/acme/app/pull/4016")
        );
        let issue = cx.debug_bounds("icarus-hover-link:E-6965").unwrap();
        cx.simulate_click(issue.center(), gpui::Modifiers::default());
        let url = "https://linear.app/acme/issue/E-6965/editor-lazy-load-frames";
        assert_eq!(
            cx.opened_url(),
            Some(issue_link(url, linear_app_installed()))
        );

        cx.simulate_mouse_move(point(px(1.), px(1.)), None, gpui::Modifiers::default());
        cx.executor().advance_clock(CLOSE_DELAY);
        cx.run_until_parked();
        assert!(cx.debug_bounds("icarus-hover-card").is_none());
    }
}
