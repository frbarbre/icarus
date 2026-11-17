use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Bounds, DebugElement, Keystroke, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, PlatformInput, Point,
    TaskExt as _, Window, point, px,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
pub(crate) struct DriveRequest {
    op: String,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    keys: Option<String>,
    #[serde(default)]
    repeat: Option<u32>,
    #[serde(default)]
    interval_ms: Option<u64>,
    #[serde(default)]
    wait: Option<WaitFor>,
    result: PathBuf,
}

#[derive(Deserialize, Clone, Debug)]
pub(crate) struct WaitFor {
    pub(crate) target: String,
    #[serde(default)]
    pub(crate) gone: bool,
    #[serde(default = "default_timeout")]
    pub(crate) timeout_ms: u64,
}

fn default_timeout() -> u64 {
    10_000
}

/// How long a changed window may wait for the platform to draw it before the driver draws it itself,
/// as windows that are hidden or off a locked screen never get a display-link frame.
const UNDRAWN_GRACE: Duration = Duration::from_millis(50);
const POLL: Duration = Duration::from_millis(4);

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Selector {
    Id(String),
    IdPrefix(String),
    Text(String),
    Point(f32, f32),
}

impl Selector {
    pub(crate) fn parse(source: &str) -> Result<Self> {
        let source = source.trim();
        if source.is_empty() {
            return Err(anyhow!("an empty selector matches nothing"));
        }
        if let Some(text) = source.strip_prefix("text:") {
            let text = text.trim();
            let text = text
                .strip_prefix('"')
                .and_then(|text| text.strip_suffix('"'))
                .unwrap_or(text);
            return Ok(Self::Text(text.to_owned()));
        }
        if let Some((x, y)) = source.split_once(',')
            && let (Ok(x), Ok(y)) = (x.trim().parse::<f32>(), y.trim().parse::<f32>())
        {
            return Ok(Self::Point(x, y));
        }
        Ok(match source.strip_suffix('*') {
            Some(prefix) => Self::IdPrefix(prefix.to_owned()),
            None => Self::Id(source.to_owned()),
        })
    }

    /// Where the selector is on screen: the first visible match's bounds.
    pub(crate) fn find(&self, elements: &[DebugElement]) -> Option<Bounds<Pixels>> {
        let mut shown = elements.iter().filter(|element| shows(element));
        match self {
            Self::Point(x, y) => Some(Bounds::new(point(px(*x), px(*y)), Default::default())),
            Self::Id(id) => shown
                .find(|element| element.selector.as_deref() == Some(id.as_str()))
                .map(|element| element.visible),
            Self::IdPrefix(prefix) => shown
                .find(|element| {
                    element
                        .selector
                        .as_deref()
                        .is_some_and(|selector| selector.starts_with(prefix.as_str()))
                })
                .map(|element| element.visible),
            Self::Text(text) => {
                let texts: Vec<&DebugElement> = shown.filter(|e| e.text.is_some()).collect();
                texts
                    .iter()
                    .find(|element| element.text.as_deref().map(str::trim) == Some(text.as_str()))
                    .or_else(|| {
                        texts.iter().find(|element| {
                            element
                                .text
                                .as_deref()
                                .is_some_and(|shown| shown.contains(text.as_str()))
                        })
                    })
                    .map(|element| element.visible)
            }
        }
    }
}

fn shows(element: &DebugElement) -> bool {
    element.visible.size.width > Pixels::ZERO && element.visible.size.height > Pixels::ZERO
}

fn bounds_json(bounds: Bounds<Pixels>) -> Value {
    let round = |value: Pixels| (f32::from(value) * 10.).round() / 10.;
    json!([
        round(bounds.origin.x),
        round(bounds.origin.y),
        round(bounds.size.width),
        round(bounds.size.height)
    ])
}

/// By origin rather than whole bounds, since text often overflows the box it sits in.
fn contains(outer: &Bounds<Pixels>, inner: &Bounds<Pixels>) -> bool {
    outer.contains(&inner.origin)
}

/// The visible selectors and text, nested by paint order and containment: elements paint their
/// own box before their children, so the innermost painted box that contains one is its parent.
pub(crate) fn tree(elements: &[DebugElement]) -> Vec<Value> {
    struct Node {
        value: Value,
        bounds: Bounds<Pixels>,
        children: Vec<usize>,
    }
    let mut nodes: Vec<Node> = Vec::new();
    let mut roots = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    for element in elements.iter().filter(|element| shows(element)) {
        let value = match (&element.selector, &element.text) {
            (Some(selector), _) => {
                json!({ "id": selector, "bounds": bounds_json(element.visible) })
            }
            (None, Some(text)) if !text.trim().is_empty() => {
                json!({ "text": text, "bounds": bounds_json(element.visible) })
            }
            _ => continue,
        };
        while open
            .last()
            .is_some_and(|&parent| !contains(&nodes[parent].bounds, &element.visible))
        {
            open.pop();
        }
        let index = nodes.len();
        nodes.push(Node {
            value,
            bounds: element.visible,
            children: Vec::new(),
        });
        match open.last() {
            Some(&parent) => nodes[parent].children.push(index),
            None => roots.push(index),
        }
        if element.selector.is_some() {
            open.push(index);
        }
    }
    fn build(index: usize, nodes: &[Node]) -> Value {
        let node = &nodes[index];
        let mut value = node.value.clone();
        if !node.children.is_empty() {
            value["children"] = node
                .children
                .iter()
                .map(|&child| build(child, nodes))
                .collect();
        }
        value
    }
    roots.into_iter().map(|root| build(root, &nodes)).collect()
}

fn front_window(cx: &App) -> Result<AnyWindowHandle> {
    cx.active_window()
        .or_else(|| cx.windows().into_iter().next())
        .context("no app window is open")
}

/// The window's elements as last drawn, drawing first when recording just started or the window changed.
fn current_elements(window: &mut Window, cx: &mut App) -> Vec<DebugElement> {
    let started = !gpui::element_recording();
    if started {
        gpui::set_element_recording(true);
        // Cached views would otherwise reuse frames painted without their selectors.
        window.refresh();
    }
    if started || window.needs_draw() {
        window.draw(cx).clear(cx);
        window.refresh();
    }
    window.debug_elements().to_vec()
}

pub(crate) fn click(window: &mut Window, position: Point<Pixels>, cx: &mut App) {
    let modifiers = Modifiers::default();
    window.dispatch_event(
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers,
        }),
        cx,
    );
    window.dispatch_event(
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers,
            click_count: 1,
            first_mouse: false,
        }),
        cx,
    );
    window.dispatch_event(
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position,
            modifiers,
            click_count: 1,
        }),
        cx,
    );
}

pub(crate) fn type_text(window: &mut Window, text: &str, cx: &mut App) {
    for character in text.chars() {
        let keystroke = match character {
            '\n' => Keystroke {
                modifiers: Modifiers::default(),
                key: "enter".into(),
                key_char: None,
            },
            ' ' => Keystroke {
                modifiers: Modifiers::default(),
                key: "space".into(),
                key_char: Some(" ".into()),
            },
            character => Keystroke {
                modifiers: Modifiers::default(),
                key: character.to_string(),
                key_char: Some(character.to_string()),
            },
        };
        window.dispatch_keystroke(keystroke, cx);
    }
}

pub(crate) fn press(window: &mut Window, keys: &str, cx: &mut App) -> Result<()> {
    let os = cx.platform_info().os;
    for source in keys.split_whitespace() {
        let keystroke = Keystroke::parse_with_os(source, os)
            .map_err(|error| anyhow!("{source} is not a keystroke: {error}"))?;
        window.dispatch_keystroke(keystroke, cx);
    }
    Ok(())
}

/// Waits until `wait.target` shows (or is gone) in a drawn frame; `since` is when the input went in.
pub(crate) async fn wait_for(
    handle: AnyWindowHandle,
    wait: &WaitFor,
    since: std::time::Instant,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let selector = Selector::parse(&wait.target)?;
    let executor = cx.background_executor().clone();
    let timeout = Duration::from_millis(wait.timeout_ms);
    let mut undrawn_since = None;
    loop {
        let now = executor.now();
        let found = cx.update_window(handle, |_, window, cx| {
            if window.needs_draw() {
                let undrawn = *undrawn_since.get_or_insert(now);
                if now.saturating_duration_since(undrawn) >= UNDRAWN_GRACE {
                    window.draw(cx).clear(cx);
                    window.refresh();
                    undrawn_since = None;
                }
            } else {
                undrawn_since = None;
            }
            selector.find(window.debug_elements()).is_some()
        })?;
        let elapsed = now.saturating_duration_since(since);
        if found != wait.gone {
            return Ok(json!({
                "target": wait.target,
                "gone": wait.gone,
                "elapsed_ms": elapsed.as_millis() as u64,
            }));
        }
        if elapsed > timeout {
            return Err(anyhow!(
                "{} did not {} within {} ms",
                wait.target,
                if wait.gone { "go away" } else { "show" },
                wait.timeout_ms
            ));
        }
        executor.timer(POLL).await;
    }
}

pub(crate) fn handle_request(request: DriveRequest, cx: &mut App) {
    let result = request.result.clone();
    let task = cx.spawn(async move |cx| {
        let outcome = run(request, cx).await;
        let report = outcome.unwrap_or_else(|error| json!({ "error": format!("{error:#}") }));
        cx.background_spawn(async move { std::fs::write(result, report.to_string()) })
            .await
    });
    task.detach_and_log_err(cx);
}

async fn run(request: DriveRequest, cx: &mut AsyncApp) -> Result<Value> {
    let handle = cx.update(|cx| front_window(cx))?;
    let executor = cx.background_executor().clone();
    match request.op.as_str() {
        "tree" => {
            let elements =
                cx.update_window(handle, |_, window, cx| current_elements(window, cx))?;
            Ok(json!({ "window": handle.window_id().as_u64(), "nodes": tree(&elements) }))
        }
        "click" => {
            let target = request.target.context("click needs a target")?;
            let selector = Selector::parse(&target)?;
            let since = executor.now();
            let position = cx.update_window(handle, |_, window, cx| {
                let elements = current_elements(window, cx);
                let bounds = selector
                    .find(&elements)
                    .with_context(|| format!("nothing on screen matches {target}"))?;
                let position = bounds.center();
                click(window, position, cx);
                anyhow::Ok(position)
            })??;
            let mut report = json!({ "clicked": {
                "target": target,
                "x": f32::from(position.x),
                "y": f32::from(position.y),
            }});
            if let Some(wait) = &request.wait {
                report["waited"] = wait_for(handle, wait, since, cx).await?;
            }
            Ok(report)
        }
        "type" => {
            let text = request.text.context("type needs text")?;
            cx.update_window(handle, |_, window, cx| type_text(window, &text, cx))?;
            Ok(json!({ "typed": text }))
        }
        "key" => {
            let keys = request.keys.context("key needs keystrokes")?;
            let repeat = request.repeat.unwrap_or(1).max(1);
            let interval = Duration::from_millis(request.interval_ms.unwrap_or(30));
            let first = executor.now();
            let mut last = first;
            for press_index in 0..repeat {
                if press_index > 0 {
                    executor.timer(interval).await;
                }
                last = executor.now();
                cx.update_window(handle, |_, window, cx| press(window, &keys, cx))??;
            }
            let mut report = json!({ "pressed": keys, "repeat": repeat });
            if let Some(wait) = &request.wait {
                let mut waited = wait_for(handle, wait, last, cx).await?;
                waited["since_first_ms"] =
                    json!(executor.now().saturating_duration_since(first).as_millis() as u64);
                report["waited"] = waited;
            }
            Ok(report)
        }
        "wait" => {
            let wait = request.wait.context("wait needs a target")?;
            let since = executor.now();
            let waited = wait_for(handle, &wait, since, cx).await?;
            Ok(json!({ "waited": waited }))
        }
        other => Err(anyhow!("unknown app driver op {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(selector: Option<&str>, text: Option<&str>, bounds: [f32; 4]) -> DebugElement {
        let bounds = Bounds::new(
            point(px(bounds[0]), px(bounds[1])),
            gpui::size(px(bounds[2]), px(bounds[3])),
        );
        DebugElement {
            selector: selector.map(str::to_owned),
            text: text.map(Into::into),
            bounds,
            visible: bounds,
        }
    }

    #[test]
    fn selectors_parse_ids_prefixes_text_and_points() {
        assert_eq!(
            Selector::parse("card:e-6849").ok(),
            Some(Selector::Id("card:e-6849".into()))
        );
        assert_eq!(
            Selector::parse("scm-row:*").ok(),
            Some(Selector::IdPrefix("scm-row:".into()))
        );
        assert_eq!(
            Selector::parse("text:\"Changes\"").ok(),
            Some(Selector::Text("Changes".into()))
        );
        assert_eq!(
            Selector::parse("text:Changes").ok(),
            Some(Selector::Text("Changes".into()))
        );
        assert_eq!(
            Selector::parse("120, 48.5").ok(),
            Some(Selector::Point(120., 48.5))
        );
        assert_eq!(
            Selector::parse("scm-row:a,b.rs").ok(),
            Some(Selector::Id("scm-row:a,b.rs".into()))
        );
        assert!(Selector::parse("  ").is_err());
    }

    #[test]
    fn text_prefers_an_exact_match_and_hidden_elements_never_match() {
        let mut hidden = element(Some("card:a"), None, [0., 0., 10., 10.]);
        hidden.visible = Bounds::default();
        let elements = [
            hidden,
            element(None, Some("Changes (3)"), [0., 0., 50., 10.]),
            element(None, Some("Changes"), [0., 20., 50., 10.]),
        ];
        assert_eq!(Selector::Id("card:a".into()).find(&elements), None);
        let exact = Selector::Text("Changes".into()).find(&elements);
        assert_eq!(exact.map(|b| f32::from(b.origin.y)), Some(20.));
        let partial = Selector::Text("(3)".into()).find(&elements);
        assert_eq!(partial.map(|b| f32::from(b.origin.y)), Some(0.));
    }

    #[test]
    fn the_tree_nests_by_paint_order_and_containment() {
        let elements = [
            element(Some("panel"), None, [0., 0., 100., 100.]),
            element(Some("card:a"), None, [0., 0., 100., 20.]),
            element(None, Some("Alpha"), [5., 5., 40., 10.]),
            element(Some("card:b"), None, [0., 20., 100., 20.]),
            element(None, Some("   "), [5., 25., 40., 10.]),
            element(Some("composer"), None, [0., 200., 100., 20.]),
        ];
        let tree = tree(&elements);
        assert_eq!(
            serde_json::to_value(&tree).ok(),
            Some(json!([
                { "id": "panel", "bounds": [0.0, 0.0, 100.0, 100.0], "children": [
                    { "id": "card:a", "bounds": [0.0, 0.0, 100.0, 20.0], "children": [
                        { "text": "Alpha", "bounds": [5.0, 5.0, 40.0, 10.0] },
                    ]},
                    { "id": "card:b", "bounds": [0.0, 20.0, 100.0, 20.0] },
                ]},
                { "id": "composer", "bounds": [0.0, 200.0, 100.0, 20.0] },
            ]))
        );
    }

    struct Clickable {
        clicks: usize,
    }

    impl gpui::Render for Clickable {
        fn render(
            &mut self,
            _: &mut Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            use gpui::{
                InteractiveElement as _, ParentElement as _, StatefulInteractiveElement as _,
                Styled as _,
            };
            gpui::div().size_full().child(
                gpui::div()
                    .id("button")
                    .debug_selector(|| "button".into())
                    .mt(px(40.))
                    .w(px(80.))
                    .h(px(20.))
                    .child(format!("Clicked {}", self.clicks))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.clicks += 1;
                        cx.notify();
                    })),
            )
        }
    }

    #[gpui::test]
    async fn clicks_find_their_target_and_waits_see_the_redrawn_frame(
        cx: &mut gpui::TestAppContext,
    ) {
        let window = cx.add_window(|_, _| Clickable { clicks: 0 });
        let handle: AnyWindowHandle = window.into();
        cx.run_until_parked();
        let elements = cx
            .update_window(handle, |_, window, cx| current_elements(window, cx))
            .expect("window");
        let tree = tree(&elements);
        assert_eq!(tree.len(), 1, "{tree:#?}");
        assert_eq!(tree[0]["id"], "button");
        assert_eq!(tree[0]["children"][0]["text"], "Clicked 0");

        let position = Selector::parse("button")
            .ok()
            .and_then(|selector| selector.find(&elements))
            .map(|bounds| bounds.center())
            .expect("the label");
        cx.update_window(handle, |_, window, cx| click(window, position, cx))
            .expect("window");
        let waited = cx.spawn(|mut cx| async move {
            let since = cx.background_executor().now();
            let wait = WaitFor {
                target: "text:Clicked 1".into(),
                gone: false,
                timeout_ms: 1000,
            };
            wait_for(handle, &wait, since, &mut cx).await
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        let waited = waited.await.expect("the click redraws the label");
        assert_eq!(waited["target"], "text:Clicked 1");
        assert!(waited["elapsed_ms"].as_u64().is_some_and(|ms| ms <= 100));
        window
            .read_with(cx, |view, _| assert_eq!(view.clicks, 1))
            .expect("window");
    }
}
