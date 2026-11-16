//! Shared behaviour of every comment box (github-review.md §11): @-mention and `:emoji:` completion,
//! and image/video uploads from a file pick, a paste or a drop, shown as "Uploading…" until GitHub answers.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use base64::Engine as _;
use editor::{CompletionProvider, Editor};
use gpui::{App, ClipboardEntry, Context, Entity, Task, Window};
use icarus_client::Client;
use language::{Buffer, CodeLabel, ToOffset as _};
use project::{Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource};
use serde::Deserialize;
use serde_json::json;
use util::ResultExt as _;

/// Someone who can be mentioned (`review.mentionables`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Mentionable {
    pub login: String,
    pub name: Option<String>,
}

/// The repository's people, shared by every comment box of a diff tab and filled on first use.
#[derive(Clone, Default)]
pub struct MentionSource(pub Arc<Mutex<Vec<Mentionable>>>);

impl MentionSource {
    pub fn set(&self, people: Vec<Mentionable>) {
        *self.0.lock().unwrap_or_else(|poison| poison.into_inner()) = people;
    }

    fn snapshot(&self) -> Vec<Mentionable> {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}

/// GitHub shortcodes offered after `:`; GitHub renders the inserted characters the same as the codes.
pub const EMOJI: &[(&str, &str)] = &[
    ("+1", "👍"),
    ("-1", "👎"),
    ("100", "💯"),
    ("art", "🎨"),
    ("beer", "🍺"),
    ("boom", "💥"),
    ("bug", "🐛"),
    ("bulb", "💡"),
    ("clap", "👏"),
    ("coffee", "☕"),
    ("confused", "😕"),
    ("construction", "🚧"),
    ("cry", "😢"),
    ("eyes", "👀"),
    ("fire", "🔥"),
    ("gear", "⚙️"),
    ("grin", "😁"),
    ("hammer", "🔨"),
    ("heart", "❤️"),
    ("heavy_check_mark", "✔️"),
    ("hooray", "🎉"),
    ("hourglass", "⌛"),
    ("joy", "😂"),
    ("laughing", "😆"),
    ("link", "🔗"),
    ("lock", "🔒"),
    ("mag", "🔍"),
    ("memo", "📝"),
    ("muscle", "💪"),
    ("ok_hand", "👌"),
    ("package", "📦"),
    ("pencil2", "✏️"),
    ("point_right", "👉"),
    ("pray", "🙏"),
    ("question", "❓"),
    ("rage", "😡"),
    ("rainbow", "🌈"),
    ("recycle", "♻️"),
    ("rocket", "🚀"),
    ("rotating_light", "🚨"),
    ("see_no_evil", "🙈"),
    ("shrug", "🤷"),
    ("slightly_smiling_face", "🙂"),
    ("smile", "😄"),
    ("smiley", "😃"),
    ("sob", "😭"),
    ("sparkles", "✨"),
    ("speech_balloon", "💬"),
    ("star", "⭐"),
    ("sunglasses", "😎"),
    ("sweat_smile", "😅"),
    ("tada", "🎉"),
    ("thinking", "🤔"),
    ("thumbsdown", "👎"),
    ("thumbsup", "👍"),
    ("warning", "⚠️"),
    ("wave", "👋"),
    ("white_check_mark", "✅"),
    ("wink", "😉"),
    ("wrench", "🔧"),
    ("x", "❌"),
    ("zap", "⚡"),
];

const MAX_SUGGESTIONS: usize = 20;

/// What the text before the cursor asks to complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// `@query`, or a bare word in the reviewer box; `back` counts the characters to replace.
    Mention { query: String, back: usize },
    /// `:query` with at least one character after the colon.
    Emoji { query: String, back: usize },
}

fn word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '+' || c == '/'
}

/// Reads the trigger from the text that ends at the cursor; `bare_mentions` treats a plain word as one.
pub fn trigger_before(text_before_cursor: &str, bare_mentions: bool) -> Option<Trigger> {
    let word: String = text_before_cursor
        .chars()
        .rev()
        .take_while(|c| word_char(*c))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let rest = &text_before_cursor[..text_before_cursor.len() - word.len()];
    let before = rest.chars().last();
    let at_boundary = |c: Option<char>| c.is_none_or(|c| c.is_whitespace() || "([{".contains(c));
    match before {
        Some('@') if at_boundary(rest[..rest.len() - 1].chars().last()) => Some(Trigger::Mention {
            back: word.chars().count() + 1,
            query: word,
        }),
        Some(':') if !word.is_empty() && at_boundary(rest[..rest.len() - 1].chars().last()) => {
            Some(Trigger::Emoji {
                back: word.chars().count() + 1,
                query: word,
            })
        }
        _ if bare_mentions && !word.is_empty() && at_boundary(before) => Some(Trigger::Mention {
            back: word.chars().count(),
            query: word,
        }),
        _ => None,
    }
}

/// Logins matching `query`: prefix matches first, then other matches, each in list order.
pub fn mention_matches(people: &[Mentionable], query: &str) -> Vec<Mentionable> {
    let query = query.to_lowercase();
    let matches = |person: &Mentionable| {
        person.login.to_lowercase().contains(&query)
            || person
                .name
                .as_deref()
                .is_some_and(|name| name.to_lowercase().contains(&query))
    };
    let (mut prefixed, mut others): (Vec<_>, Vec<_>) = people
        .iter()
        .filter(|person| matches(person))
        .cloned()
        .partition(|person| person.login.to_lowercase().starts_with(&query));
    prefixed.append(&mut others);
    prefixed.truncate(MAX_SUGGESTIONS);
    prefixed
}

pub fn emoji_matches(query: &str) -> Vec<(&'static str, &'static str)> {
    let query = query.to_lowercase();
    let (mut prefixed, mut others): (Vec<_>, Vec<_>) = EMOJI
        .iter()
        .copied()
        .filter(|(code, _)| code.contains(&query))
        .partition(|(code, _)| code.starts_with(&query));
    prefixed.append(&mut others);
    prefixed.truncate(MAX_SUGGESTIONS);
    prefixed
}

/// Completes @-mentions and `:emoji:` in a comment box; in the reviewer box any word is a login.
pub struct CommentCompletions {
    pub people: MentionSource,
    /// The reviewer box: plain logins, no `@`, and no emoji.
    pub bare_logins: bool,
}

impl CompletionProvider for CommentCompletions {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: language::Anchor,
        _trigger: editor::CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let buffer = buffer.read(cx);
        let offset = buffer_position.to_offset(buffer);
        let line_start = buffer
            .reversed_chars_at(buffer_position)
            .take_while(|c| *c != '\n')
            .map(char::len_utf8)
            .sum::<usize>();
        let before: String = buffer.text_for_range(offset - line_start..offset).collect();
        let Some(trigger) = trigger_before(&before, self.bare_logins) else {
            return Task::ready(Ok(Vec::new()));
        };
        let back = match &trigger {
            Trigger::Mention { back, .. } | Trigger::Emoji { back, .. } => *back,
        };
        let back_bytes: usize = before.chars().rev().take(back).map(char::len_utf8).sum();
        let start = buffer.anchor_before(offset - back_bytes);
        let replace_range = start..buffer_position;
        let completion = |label: String, new_text: String| Completion {
            replace_range: replace_range.clone(),
            label: CodeLabel::plain(label, None),
            new_text,
            documentation: None,
            source: CompletionSource::Custom,
            icon_path: None,
            icon_color: None,
            match_start: Some(start),
            snippet_deduplication_key: None,
            insert_text_mode: None,
            confirm: None,
            group: None,
        };
        let completions: Vec<Completion> = match trigger {
            Trigger::Mention { query, .. } => mention_matches(&self.people.snapshot(), &query)
                .into_iter()
                .map(|person| {
                    let label = match &person.name {
                        Some(name) if !name.is_empty() => format!("@{} {name}", person.login),
                        _ => format!("@{}", person.login),
                    };
                    let new_text = if self.bare_logins {
                        person.login
                    } else {
                        format!("@{} ", person.login)
                    };
                    completion(label, new_text)
                })
                .collect(),
            Trigger::Emoji { query, .. } if !self.bare_logins => emoji_matches(&query)
                .into_iter()
                .map(|(code, emoji)| completion(format!("{emoji} :{code}:"), emoji.to_owned()))
                .collect(),
            Trigger::Emoji { .. } => Vec::new(),
        };
        Task::ready(Ok(vec![CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions::default(),
            is_incomplete: true,
        }]))
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        text: &str,
        _trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        if text.contains('@') || (text.contains(':') && !self.bare_logins) {
            return true;
        }
        if !text.chars().all(word_char) || text.is_empty() {
            return false;
        }
        let before: String = buffer
            .read(cx)
            .reversed_chars_at(position)
            .take_while(|c| *c != '\n')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        trigger_before(&before, self.bare_logins).is_some()
    }

    fn sort_completions(&self) -> bool {
        false
    }

    fn filter_completions(&self) -> bool {
        false
    }
}

/// Puts the completion provider on a comment box's editor.
pub fn enable_completions(
    editor: &mut Editor,
    people: MentionSource,
    bare_logins: bool,
    _cx: &mut Context<Editor>,
) {
    editor.set_completion_provider(Some(Rc::new(CommentCompletions {
        people,
        bare_logins,
    })));
}

/// Where an upload's bytes come from.
#[derive(Debug, Clone)]
pub enum UploadSource {
    Path(PathBuf),
    Data { name: String, bytes: Vec<u8> },
}

impl UploadSource {
    fn name(&self) -> String {
        match self {
            Self::Path(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_owned()),
            Self::Data { name, .. } => name.clone(),
        }
    }

    fn params(&self, workspace: &str) -> serde_json::Value {
        match self {
            Self::Path(path) => json!({ "workspace": workspace, "path": path }),
            Self::Data { name, bytes } => json!({
                "workspace": workspace,
                "name": name,
                "data_base64": base64::engine::general_purpose::STANDARD.encode(bytes),
            }),
        }
    }
}

/// Images on the clipboard, named so the server can tell their type.
pub fn clipboard_images(cx: &App) -> Vec<UploadSource> {
    let Some(item) = cx.read_from_clipboard() else {
        return Vec::new();
    };
    item.entries()
        .iter()
        .filter_map(|entry| match entry {
            ClipboardEntry::Image(image) => Some(UploadSource::Data {
                name: format!("pasted-image.{}", image.format.extension()),
                bytes: image.bytes.clone(),
            }),
            ClipboardEntry::ExternalPaths(paths) => {
                paths.paths().first().cloned().map(UploadSource::Path)
            }
            ClipboardEntry::String(_) => None,
        })
        .collect()
}

fn placeholder(name: &str, id: u64) -> String {
    format!("![Uploading {name}… #{id}]()")
}

thread_local! {
    static NEXT_UPLOAD: RefCell<u64> = const { RefCell::new(1) };
}

/// Inserts an "Uploading…" placeholder at the cursor, uploads through `review.upload`, then swaps in
/// GitHub's markdown; on failure the placeholder goes and `on_error` gets GitHub's words.
pub fn upload_into(
    editor: Entity<Editor>,
    source: UploadSource,
    workspace: String,
    client: Option<Arc<Client>>,
    on_error: Rc<dyn Fn(String, &mut Window, &mut App)>,
    window: &mut Window,
    cx: &mut App,
) {
    let id = NEXT_UPLOAD.with(|next| {
        let mut next = next.borrow_mut();
        let id = *next;
        *next += 1;
        id
    });
    let marker = placeholder(&source.name(), id);
    editor.update(cx, |editor, cx| editor.insert(&marker, window, cx));
    let Some(call) = crate::review::request(client, "review.upload", source.params(&workspace), cx)
    else {
        return;
    };
    window
        .spawn(cx, async move |cx| {
            let result = call.await;
            cx.update(|window, cx| {
                let markdown = match &result {
                    Ok(value) => value["markdown"].as_str().unwrap_or_default().to_owned(),
                    Err(_) => String::new(),
                };
                replace_text(&editor, &marker, &markdown, cx);
                if let Err(error) = result {
                    on_error(format!("{error:#}"), window, cx);
                }
            })
            .log_err();
        })
        .detach();
}

/// Replaces the first `from` in the editor's single buffer with `to`.
fn replace_text(editor: &Entity<Editor>, from: &str, to: &str, cx: &mut App) {
    let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else {
        return;
    };
    buffer.update(cx, |buffer, cx| {
        let text = buffer.text();
        if let Some(start) = text.find(from) {
            buffer.edit([(start..start + from.len(), to)], None, cx);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(login: &str, name: Option<&str>) -> Mentionable {
        Mentionable {
            login: login.into(),
            name: name.map(str::to_owned),
        }
    }

    #[test]
    fn triggers_need_a_boundary_before_the_sigil() {
        assert_eq!(
            trigger_before("hi @al", false),
            Some(Trigger::Mention {
                query: "al".into(),
                back: 3
            })
        );
        assert_eq!(
            trigger_before("@", false),
            Some(Trigger::Mention {
                query: String::new(),
                back: 1
            })
        );
        assert_eq!(
            trigger_before("mail@al", false),
            None,
            "an email isn't a mention"
        );
        assert_eq!(
            trigger_before("nice :tad", false),
            Some(Trigger::Emoji {
                query: "tad".into(),
                back: 4
            })
        );
        assert_eq!(
            trigger_before("10:30", false),
            None,
            "a time isn't an emoji"
        );
        assert_eq!(trigger_before("a :", false), None, "a bare colon waits");
        assert_eq!(
            trigger_before("alice, bo", true),
            Some(Trigger::Mention {
                query: "bo".into(),
                back: 2
            }),
            "the reviewer box completes plain words"
        );
        assert_eq!(trigger_before("hi bo", false), None);
    }

    #[test]
    fn mentions_prefer_prefix_matches_and_search_names() {
        let people = [
            person("bob", None),
            person("alice", Some("Alice Bobson")),
            person("robert", None),
        ];
        let logins: Vec<String> = mention_matches(&people, "b")
            .into_iter()
            .map(|p| p.login)
            .collect();
        assert_eq!(logins, ["bob", "alice", "robert"]);
        assert!(mention_matches(&people, "zzz").is_empty());
    }

    #[test]
    fn emoji_prefer_prefix_matches() {
        let codes: Vec<&str> = emoji_matches("ta").into_iter().map(|(c, _)| c).collect();
        assert_eq!(codes.first(), Some(&"tada"));
        assert!(codes.contains(&"star"), "{codes:?}");
    }
}
