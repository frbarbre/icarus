//! Images and videos in PR comments: finding them in a comment's text, loading GitHub's attachments
//! through the server (which holds the user's login), thumbnails, and the full-window viewer.

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use collections::HashMap;
use gpui::{
    AnyElement, App, ClickEvent, Context, DismissEvent, EventEmitter, FocusHandle, Focusable,
    ImageFormat, ImageSource, KeyDownEvent, ObjectFit, Render, SharedString, Window, img, px,
    relative,
};
use ui::{Tooltip, prelude::*};
use workspace::{ModalView, MultiWorkspace};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRef {
    pub kind: MediaKind,
    pub url: String,
    pub label: String,
}

const IMAGE_EXTENSIONS: [&str; 6] = [".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg"];
const VIDEO_EXTENSIONS: [&str; 4] = [".mp4", ".mov", ".webm", ".m4v"];

fn has_extension(url: &str, extensions: &[&str]) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    extensions.iter().any(|extension| path.ends_with(extension))
}

fn is_user_attachment(url: &str) -> bool {
    url.starts_with("https://github.com/user-attachments/")
}

/// What a bare URL on its own line shows as: GitHub's pasted recordings are attachment links with no
/// extension, so those play as videos.
fn bare_kind(url: &str) -> Option<MediaKind> {
    if has_extension(url, &IMAGE_EXTENSIONS) {
        Some(MediaKind::Image)
    } else if has_extension(url, &VIDEO_EXTENSIONS) || is_user_attachment(url) {
        Some(MediaKind::Video)
    } else {
        None
    }
}

fn file_name(url: &str) -> String {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn label_or(label: &str, url: &str, kind: MediaKind) -> String {
    let label = label.trim();
    if !label.is_empty() {
        return label.to_owned();
    }
    match kind {
        MediaKind::Image => "Image".to_owned(),
        MediaKind::Video if is_user_attachment(url) => "Video".to_owned(),
        MediaKind::Video => file_name(url),
    }
}

/// An HTML attribute's value: `src="…"` or `src='…'`.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find(name) {
        let start = from + found;
        from = start + name.len();
        let before_ok = start == 0
            || lower[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        let rest = lower[from..].trim_start();
        if !before_ok || !rest.starts_with('=') {
            continue;
        }
        let value_start = tag.len() - rest.len() + 1;
        let value = tag[value_start..].trim_start();
        let quote = value.chars().next()?;
        if quote == '"' || quote == '\'' {
            let end = value[1..].find(quote)?;
            return Some(value[1..1 + end].to_owned());
        }
        let end = value
            .find(|character: char| character.is_whitespace() || character == '>')
            .unwrap_or(value.len());
        return Some(value[..end].to_owned());
    }
    None
}

/// Takes a markdown image (`![alt](url "title")`) starting at `text`, returning its alt, url and length.
fn markdown_image(text: &str) -> Option<(String, String, usize)> {
    let rest = text.strip_prefix("![")?;
    let alt_end = rest.find("](")?;
    let alt = &rest[..alt_end];
    if alt.contains('\n') {
        return None;
    }
    let target = &rest[alt_end + 2..];
    let close = target.find(')')?;
    let url = target[..close]
        .split_whitespace()
        .next()?
        .trim_matches(['<', '>']);
    Some((alt.to_owned(), url.to_owned(), 2 + alt_end + 2 + close + 1))
}

/// Takes `<img …>` or `<video …>…</video>` starting at `text`.
fn html_media(text: &str) -> Option<(MediaKind, String, String, usize)> {
    let lower = text.get(..7)?.to_ascii_lowercase();
    let kind = if lower.starts_with("<img") {
        MediaKind::Image
    } else if lower.starts_with("<video") {
        MediaKind::Video
    } else {
        return None;
    };
    let tag_end = text.find('>')? + 1;
    let tag = &text[..tag_end];
    let mut length = tag_end;
    let mut src = attribute(tag, "src");
    if kind == MediaKind::Video {
        let lower = text.to_ascii_lowercase();
        if let Some(close) = lower.find("</video>") {
            if src.is_none() {
                src = attribute(&text[tag_end..close], "src");
            }
            length = close + "</video>".len();
        }
    }
    let alt = attribute(tag, "alt").unwrap_or_default();
    Some((kind, src?, alt, length))
}

/// Splits a comment into its text (media taken out) and its media, in the order they appear.
pub fn split_media(body: &str) -> (String, Vec<MediaRef>) {
    let mut text = String::with_capacity(body.len());
    let mut media = Vec::new();
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with("https://")
            && !trimmed.contains(char::is_whitespace)
            && let Some(kind) = bare_kind(trimmed)
        {
            media.push(MediaRef {
                kind,
                url: trimmed.to_owned(),
                label: label_or("", trimmed, kind),
            });
            continue;
        }
        let mut rest = line;
        while !rest.is_empty() {
            let next = rest.find(['!', '<']).unwrap_or(rest.len());
            text.push_str(&rest[..next]);
            rest = &rest[next..];
            if rest.is_empty() {
                break;
            }
            if let Some((alt, url, length)) = markdown_image(rest) {
                media.push(MediaRef {
                    kind: MediaKind::Image,
                    label: label_or(&alt, &url, MediaKind::Image),
                    url,
                });
                rest = &rest[length..];
            } else if let Some((kind, url, alt, length)) = html_media(rest) {
                media.push(MediaRef {
                    kind,
                    label: label_or(&alt, &url, kind),
                    url,
                });
                rest = &rest[length..];
            } else {
                let mut characters = rest.chars();
                if let Some(character) = characters.next() {
                    text.push(character);
                }
                rest = characters.as_str();
            }
        }
    }
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    (text.trim().to_owned(), media)
}

/// A loaded attachment.
#[derive(Clone)]
pub struct Asset {
    pub content_type: String,
    pub bytes: Arc<Vec<u8>>,
    pub image: Option<Arc<gpui::Image>>,
}

#[derive(Clone)]
pub enum MediaState {
    Loading,
    Ready(Asset),
    Failed(String),
}

/// Attachments by URL, shared by every view; each is asked of the server once.
#[derive(Default)]
struct MediaStore(HashMap<String, MediaState>);

impl gpui::Global for MediaStore {}

/// GitHub's comment media, which private repositories serve only to a signed-in user.
fn needs_server(url: &str) -> bool {
    url.starts_with("https://github.com/user-attachments/")
        || url.starts_with("https://private-user-images.githubusercontent.com/")
        || url.starts_with("https://user-images.githubusercontent.com/")
}

/// The attachment's state, starting its load the first time it is asked for.
pub fn media_state(url: &str, cx: &mut App) -> MediaState {
    if let Some(state) = cx
        .try_global::<MediaStore>()
        .and_then(|store| store.0.get(url))
    {
        return state.clone();
    }
    cx.default_global::<MediaStore>()
        .0
        .insert(url.to_owned(), MediaState::Loading);
    let Some(task) = crate::review::request(
        crate::app_client(cx),
        "github.asset",
        serde_json::json!({ "url": url }),
        cx,
    ) else {
        let failed = MediaState::Failed("Not connected to the Icarus server".into());
        cx.default_global::<MediaStore>()
            .0
            .insert(url.to_owned(), failed.clone());
        return failed;
    };
    let url = url.to_owned();
    cx.spawn(async move |cx| {
        let reply = task.await;
        cx.update(|cx| {
            let state = match reply.and_then(|reply| asset_from_reply(&reply)) {
                Ok(asset) => MediaState::Ready(asset),
                Err(error) => MediaState::Failed(format!("{error:#}")),
            };
            cx.default_global::<MediaStore>().0.insert(url, state);
            cx.refresh_windows();
        })
    })
    .detach();
    MediaState::Loading
}

/// Puts an attachment in the store without asking the server, for previews and visual tests.
pub fn preload_media(url: &str, content_type: &str, bytes: Vec<u8>, cx: &mut App) {
    let image = ImageFormat::from_mime_type(content_type)
        .map(|format| Arc::new(gpui::Image::from_bytes(format, bytes.clone())));
    let asset = Asset {
        content_type: content_type.to_owned(),
        bytes: Arc::new(bytes),
        image,
    };
    cx.default_global::<MediaStore>()
        .0
        .insert(url.to_owned(), MediaState::Ready(asset));
}

fn asset_from_reply(reply: &serde_json::Value) -> anyhow::Result<Asset> {
    let content_type = reply["content_type"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(reply["data"].as_str().unwrap_or_default())?;
    let image = ImageFormat::from_mime_type(&content_type)
        .map(|format| Arc::new(gpui::Image::from_bytes(format, bytes.clone())));
    Ok(Asset {
        content_type,
        bytes: Arc::new(bytes),
        image,
    })
}

/// What an image element shows: the loaded attachment, or the URL itself for a public host.
fn image_source(item: &MediaRef, cx: &mut App) -> Option<Result<ImageSource, MediaState>> {
    if !needs_server(&item.url) {
        return Some(Ok(ImageSource::from(SharedString::from(item.url.clone()))));
    }
    match media_state(&item.url, cx) {
        MediaState::Ready(Asset {
            image: Some(image), ..
        }) => Some(Ok(ImageSource::from(image))),
        state => Some(Err(state)),
    }
}

/// Opens the viewer on `index` of `items` in the window's workspace.
pub fn open_viewer(items: Vec<MediaRef>, index: usize, window: &mut Window, cx: &mut App) {
    let Some(workspace) = window
        .root::<MultiWorkspace>()
        .flatten()
        .map(|multi| multi.read(cx).workspace().clone())
    else {
        return;
    };
    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, |_, cx| MediaViewer::new(items, index, cx));
    });
}

/// A comment's thumbnails; clicking one opens the viewer on it with the rest of `gallery` beside it.
pub fn render_media_strip(
    id: &str,
    items: &[MediaRef],
    gallery: &[MediaRef],
    cx: &mut App,
) -> Option<AnyElement> {
    if items.is_empty() {
        return None;
    }
    let colors = cx.theme().colors();
    let (border, surface) = (colors.border_variant, colors.element_background);
    let mut strip = h_flex().flex_wrap().gap_1p5().py_0p5();
    for (index, item) in items.iter().enumerate() {
        let at = gallery
            .iter()
            .position(|candidate| candidate == item)
            .unwrap_or(0);
        let gallery = gallery.to_vec();
        let thumbnail = match item.kind {
            MediaKind::Image => match image_source(item, cx) {
                Some(Ok(source)) => img(source)
                    .max_h(px(180.))
                    .max_w(relative(1.))
                    .object_fit(ObjectFit::Contain)
                    .rounded_md()
                    .into_any_element(),
                Some(Err(MediaState::Failed(error))) => {
                    placeholder(&item.label, Some(error), false)
                }
                _ => placeholder(&item.label, None, false),
            },
            MediaKind::Video => placeholder(&item.label, None, true),
        };
        strip = strip.child(
            div()
                .id(SharedString::from(format!("{id}-media-{index}")))
                .max_w_full()
                .rounded_md()
                .border_1()
                .border_color(border)
                .bg(surface)
                .overflow_hidden()
                .cursor_pointer()
                .tooltip(Tooltip::text(item.label.clone()))
                .on_click(move |_: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    open_viewer(gallery.clone(), at, window, cx);
                })
                .child(thumbnail),
        );
    }
    Some(strip.into_any_element())
}

fn placeholder(label: &str, error: Option<String>, video: bool) -> AnyElement {
    v_flex()
        .w(px(200.))
        .h(px(112.))
        .gap_1()
        .justify_center()
        .items_center()
        .child(
            Icon::new(if video {
                IconName::PlayFilled
            } else {
                IconName::Image
            })
            .size(IconSize::Medium)
            .color(if error.is_some() {
                Color::Error
            } else {
                Color::Muted
            }),
        )
        .child(
            div().max_w(px(184.)).child(
                Label::new(match (&error, video) {
                    (Some(_), _) => format!("Couldn't load {label}"),
                    (None, true) => label.to_owned(),
                    (None, false) => "Loading…".to_owned(),
                })
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .truncate(),
            ),
        )
        .into_any_element()
}

/// The extension a saved copy of a video gets, so the system player recognises it.
fn extension_for(content_type: &str, url: &str) -> String {
    match content_type {
        "video/mp4" => "mp4".to_owned(),
        "video/quicktime" => "mov".to_owned(),
        "video/webm" => "webm".to_owned(),
        "image/png" => "png".to_owned(),
        "image/jpeg" => "jpg".to_owned(),
        "image/gif" => "gif".to_owned(),
        "image/webp" => "webp".to_owned(),
        _ => file_name(url)
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_owned())
            .unwrap_or_else(|| "bin".to_owned()),
    }
}

/// Writes a loaded attachment to the temp folder, once, for the system player or Finder.
fn saved_copy(item: &MediaRef, asset: &Asset) -> anyhow::Result<PathBuf> {
    let dir = std::env::temp_dir().join("icarus-media");
    std::fs::create_dir_all(&dir)?;
    let stem: String = item
        .url
        .rsplit('/')
        .next()
        .unwrap_or("media")
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(64)
        .collect();
    let path = dir.join(format!(
        "{stem}.{}",
        extension_for(&asset.content_type, &item.url)
    ));
    if !path.exists() {
        std::fs::write(&path, asset.bytes.as_slice())?;
    }
    Ok(path)
}

/// Every image and video of a thread, large, one at a time: ←/→ step, Escape closes (Orca's lightbox).
pub struct MediaViewer {
    items: Vec<MediaRef>,
    index: usize,
    focus_handle: FocusHandle,
    error: Option<String>,
}

impl MediaViewer {
    pub fn new(items: Vec<MediaRef>, index: usize, cx: &mut Context<Self>) -> Self {
        let index = index.min(items.len().saturating_sub(1));
        Self {
            items,
            index,
            focus_handle: cx.focus_handle(),
            error: None,
        }
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn counter(&self) -> Option<String> {
        (self.items.len() > 1).then(|| format!("{} / {}", self.index + 1, self.items.len()))
    }

    /// Moves by `delta`, wrapping around at either end.
    pub fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.items.len() as isize;
        if count == 0 {
            return;
        }
        self.index = (self.index as isize + delta).rem_euclid(count) as usize;
        self.error = None;
        cx.notify();
    }

    fn current(&self) -> Option<&MediaRef> {
        self.items.get(self.index)
    }

    /// Opens the current item in the system's app for it, or reveals its saved copy.
    fn open_saved(&mut self, reveal: bool, cx: &mut Context<Self>) {
        let Some(item) = self.current().cloned() else {
            return;
        };
        if !needs_server(&item.url) {
            cx.open_url(&item.url);
            return;
        }
        match media_state(&item.url, cx) {
            MediaState::Ready(asset) => match saved_copy(&item, &asset) {
                Ok(path) if reveal => cx.reveal_path(&path),
                Ok(path) => cx.open_with_system(&path),
                Err(error) => self.error = Some(format!("{error:#}")),
            },
            MediaState::Loading => self.error = Some("Still loading…".into()),
            MediaState::Failed(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "left" => self.step(-1, cx),
            "right" => self.step(1, cx),
            "escape" => cx.emit(DismissEvent),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn render_item(&self, item: &MediaRef, cx: &mut Context<Self>) -> AnyElement {
        if item.kind == MediaKind::Image {
            match image_source(item, cx) {
                Some(Ok(source)) => {
                    return img(source)
                        .size_full()
                        .object_fit(ObjectFit::Contain)
                        .into_any_element();
                }
                Some(Err(MediaState::Failed(error))) => {
                    return placeholder(&item.label, Some(error), false);
                }
                _ => return placeholder(&item.label, None, false),
            }
        }
        let loading =
            needs_server(&item.url) && matches!(media_state(&item.url, cx), MediaState::Loading);
        v_flex()
            .gap_2()
            .items_center()
            .child(
                IconButton::new("icarus-media-play", IconName::PlayFilled)
                    .icon_size(IconSize::XLarge)
                    .shape(ui::IconButtonShape::Square)
                    .style(ButtonStyle::Filled)
                    .tooltip(Tooltip::text("Play in the default player"))
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.open_saved(false, cx)),
                    ),
            )
            .child(
                Label::new(if loading {
                    "Loading the video…"
                } else {
                    "Plays in your default video player"
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .into_any_element()
    }
}

impl EventEmitter<DismissEvent> for MediaViewer {}
impl ModalView for MediaViewer {
    fn fade_out_background(&self) -> bool {
        true
    }
}

impl Focusable for MediaViewer {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MediaViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let Some(item) = self.current().cloned() else {
            return div().into_any_element();
        };
        let many = self.items.len() > 1;
        let body = self.render_item(&item, cx);
        let colors = cx.theme().colors();
        let url = item.url.clone();
        let saved = needs_server(&item.url);
        v_flex()
            .key_context("IcarusMediaViewer")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .w(viewport.width * 0.9)
            .h(viewport.height * 0.85)
            .elevation_3(cx)
            .overflow_hidden()
            .child(
                h_flex()
                    .flex_none()
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(item.label.clone()).truncate()),
                    )
                    .when_some(self.counter(), |row, counter| {
                        row.child(
                            Label::new(counter)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .when(saved, |row| {
                        row.child(
                            IconButton::new("icarus-media-reveal", IconName::Folder)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Reveal the file"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.open_saved(true, cx)
                                })),
                        )
                    })
                    .child(
                        IconButton::new("icarus-media-github", IconName::ArrowUpRight)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Open on GitHub"))
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    )
                    .child(
                        IconButton::new("icarus-media-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Close"))
                            .on_click(
                                cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(DismissEvent)),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .p_4()
                    .justify_center()
                    .items_center()
                    .bg(colors.editor_background)
                    .child(body)
                    .when(many, |stage| {
                        stage
                            .child(
                                div().absolute().left_3().child(
                                    IconButton::new("icarus-media-previous", IconName::ChevronLeft)
                                        .style(ButtonStyle::Filled)
                                        .tooltip(Tooltip::text("Previous (←)"))
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.step(-1, cx)
                                        })),
                                ),
                            )
                            .child(
                                div().absolute().right_3().child(
                                    IconButton::new("icarus-media-next", IconName::ChevronRight)
                                        .style(ButtonStyle::Filled)
                                        .tooltip(Tooltip::text("Next (→)"))
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.step(1, cx)
                                        })),
                                ),
                            )
                    }),
            )
            .when_some(self.error.clone(), |column, error| {
                column.child(
                    div().px_3().py_1().child(
                        Label::new(error)
                            .size(LabelSize::XSmall)
                            .color(Color::Error),
                    ),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_is_found_in_markdown_html_and_bare_attachment_links() {
        let body = "Before\n\
            ![Screenshot 2026-11-11 at 09.12.38.png](https://github.com/user-attachments/assets/aaaa)\n\
            Inline <img width=\"300\" alt=\"Diagram\" src=\"https://github.com/user-attachments/assets/bbbb\" /> after\n\
            https://github.com/user-attachments/assets/cccc\n\
            <video src='https://example.com/demo.mp4' controls></video>\n\
            https://example.com/page\n\
            Price is ! not an image and a < b.";
        let (text, media) = split_media(body);
        assert_eq!(
            media,
            [
                MediaRef {
                    kind: MediaKind::Image,
                    url: "https://github.com/user-attachments/assets/aaaa".into(),
                    label: "Screenshot 2026-11-11 at 09.12.38.png".into(),
                },
                MediaRef {
                    kind: MediaKind::Image,
                    url: "https://github.com/user-attachments/assets/bbbb".into(),
                    label: "Diagram".into(),
                },
                MediaRef {
                    kind: MediaKind::Video,
                    url: "https://github.com/user-attachments/assets/cccc".into(),
                    label: "Video".into(),
                },
                MediaRef {
                    kind: MediaKind::Video,
                    url: "https://example.com/demo.mp4".into(),
                    label: "demo.mp4".into(),
                },
            ]
        );
        assert_eq!(
            text,
            "Before\n\nInline  after\n\nhttps://example.com/page\nPrice is ! not an image and a < b."
        );
        assert_eq!(
            attribute("<img src=x alt='a b'>", "alt").as_deref(),
            Some("a b")
        );
        assert_eq!(attribute("<img data-src=\"no\">", "src"), None);
    }

    #[gpui::test]
    fn the_viewer_steps_through_the_gallery_and_wraps(cx: &mut gpui::TestAppContext) {
        let items: Vec<MediaRef> = ["a", "b", "c"]
            .into_iter()
            .map(|name| MediaRef {
                kind: MediaKind::Image,
                url: format!("https://example.com/{name}.png"),
                label: name.into(),
            })
            .collect();
        let viewer = cx.new(|cx| MediaViewer::new(items, 1, cx));
        viewer.update(cx, |viewer, cx| {
            assert_eq!(viewer.counter().as_deref(), Some("2 / 3"));
            viewer.step(1, cx);
            assert_eq!(viewer.index(), 2);
            viewer.step(1, cx);
            assert_eq!(viewer.index(), 0, "past the last it wraps to the first");
            viewer.step(-1, cx);
            assert_eq!(viewer.index(), 2);
        });
        let single = cx.new(|cx| {
            MediaViewer::new(
                vec![MediaRef {
                    kind: MediaKind::Video,
                    url: "https://github.com/user-attachments/assets/x".into(),
                    label: "Video".into(),
                }],
                5,
                cx,
            )
        });
        single.read_with(cx, |viewer, _| {
            assert_eq!(viewer.index(), 0);
            assert_eq!(viewer.counter(), None, "one item shows no counter");
        });
    }
}
