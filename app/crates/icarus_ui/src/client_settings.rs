//! The app's own settings layer: `client.json` in the app's config folder. Its values override the
//! server's for this app only, so a laptop and a big screen can show different cards (plan §3a).
//! The cards' View menu, the card hotkeys and `icarus app config` change it.

use std::path::PathBuf;

use anyhow::{Result, bail};
use gpui::{App, AppContext as _, Global, TaskExt as _};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use util::ResultExt as _;

pub const CARD_FIELDS: &str = "workspace.cardFields";
/// Per grouping (`sidebar.groupBy` value), the titles of the sections folded shut.
pub const COLLAPSED_GROUPS: &str = "sidebar.collapsedGroups";

/// The line comment box's tab used last: `agent` (a note) or `review` (a review draft).
pub const COMMENT_DESTINATION: &str = "review.commentDestination";

/// How Source Control lists changed files: `list` or `tree`.
pub const CHANGES_VIEW: &str = "sourceControl.changesView";

/// The keys this layer may hold.
pub const KEYS: &[&str] = &[
    CARD_FIELDS,
    COLLAPSED_GROUPS,
    COMMENT_DESTINATION,
    CHANGES_VIEW,
];

#[derive(Default)]
pub struct ClientSettings {
    /// None in tests and previews: changes stay in memory.
    path: Option<PathBuf>,
    value: Value,
}

impl Global for ClientSettings {}

pub fn file_path() -> PathBuf {
    paths::config_dir().join("client.json")
}

/// Loads `client.json`; a missing or broken file means nothing is overridden.
pub fn init(cx: &mut App) {
    let path = file_path();
    let value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))
            .log_err()
            .unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    cx.set_global(ClientSettings {
        path: Some(path),
        value,
    });
}

fn lookup<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.')
        .try_fold(value, |value, part| value.get(part))
        .filter(|value| !value.is_null())
}

/// The value at `key`, or the whole layer for `None`.
pub fn get(key: Option<&str>, cx: &App) -> Value {
    let Some(settings) = cx.try_global::<ClientSettings>() else {
        return Value::Null;
    };
    match key {
        Some(key) => lookup(&settings.value, key).cloned().unwrap_or(Value::Null),
        None => settings.value.clone(),
    }
}

/// This app's card fields, when it overrides the server's `workspace.cardFields`.
pub fn card_fields(cx: &App) -> Option<Vec<String>> {
    get(Some(CARD_FIELDS), cx).as_array().map(|items| {
        items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect()
    })
}

pub fn group_collapsed(group_by: &str, title: &str, cx: &App) -> bool {
    get(Some(COLLAPSED_GROUPS), cx)[group_by]
        .as_array()
        .is_some_and(|titles| titles.iter().any(|item| item.as_str() == Some(title)))
}

/// Folds the section `title` of grouping `group_by` shut, or opens it again.
pub fn toggle_group_collapsed(group_by: &str, title: &str, cx: &mut App) -> Result<()> {
    let mut groups = get(Some(COLLAPSED_GROUPS), cx);
    if !groups.is_object() {
        groups = json!({});
    }
    let mut titles: Vec<Value> = groups[group_by].as_array().cloned().unwrap_or_default();
    match titles.iter().position(|item| item.as_str() == Some(title)) {
        Some(index) => {
            titles.remove(index);
        }
        None => titles.push(json!(title)),
    }
    groups[group_by] = Value::Array(titles);
    set(COLLAPSED_GROUPS, Some(groups), cx)
}

fn validate(key: &str, value: &Value) -> Result<()> {
    if !KEYS.contains(&key) {
        bail!(
            "{key} can't be set for one app; it can: {}",
            KEYS.join(", ")
        );
    }
    if key == CARD_FIELDS
        && !value
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string))
    {
        bail!("{key} takes a list of field ids, e.g. [\"pr\", \"unread\"]");
    }
    if key == CHANGES_VIEW && !matches!(value.as_str(), Some("list" | "tree")) {
        bail!("{key} is \"list\" or \"tree\"");
    }
    if key == COMMENT_DESTINATION && !matches!(value.as_str(), Some("agent" | "review")) {
        bail!("{key} is \"agent\" or \"review\"");
    }
    if key == COLLAPSED_GROUPS
        && !value.as_object().is_some_and(|groups| {
            groups.values().all(|titles| {
                titles
                    .as_array()
                    .is_some_and(|items| items.iter().all(Value::is_string))
            })
        })
    {
        bail!("{key} maps a grouping to section titles, e.g. {{\"stack\": [\"Stack #7\"]}}");
    }
    Ok(())
}

/// Sets `key` (or removes it with `None`) and saves the file.
pub fn set(key: &str, value: Option<Value>, cx: &mut App) -> Result<()> {
    if let Some(value) = &value {
        validate(key, value)?;
    } else if !KEYS.contains(&key) {
        bail!(
            "{key} can't be set for one app; it can: {}",
            KEYS.join(", ")
        );
    }
    let settings = cx.default_global::<ClientSettings>();
    let parts: Vec<&str> = key.split('.').collect();
    let Some((last, parents)) = parts.split_last() else {
        bail!("empty key");
    };
    if !settings.value.is_object() {
        settings.value = Value::Object(Map::new());
    }
    let mut node = &mut settings.value;
    for part in parents {
        if !node[*part].is_object() {
            node[*part] = Value::Object(Map::new());
        }
        node = &mut node[*part];
    }
    if let Some(object) = node.as_object_mut() {
        match value {
            Some(value) => {
                object.insert((*last).to_owned(), value);
            }
            None => {
                object.remove(*last);
            }
        }
    }
    if let Some(path) = settings.path.clone() {
        let text = serde_json::to_string_pretty(&settings.value)? + "\n";
        cx.background_spawn(async move {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, text)?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }
    Ok(())
}

/// `icarus app config get|set|unset`, relayed by the server.
#[derive(Debug, Deserialize)]
pub struct ConfigRequest {
    pub op: String,
    pub key: Option<String>,
    #[serde(default)]
    pub value: Option<Value>,
    pub result: Option<PathBuf>,
}

pub fn handle_request(request: ConfigRequest, cx: &mut App) {
    let key = request.key.as_deref();
    let outcome = match (request.op.as_str(), key) {
        ("get", _) => Ok(()),
        ("set", Some(key)) => set(key, request.value.clone(), cx),
        ("unset", Some(key)) => set(key, None, cx),
        (op, _) => Err(anyhow::anyhow!("can't {op} without a key")),
    };
    let report = match &outcome {
        Ok(()) => json!({ "key": key, "value": get(key, cx) }),
        Err(error) => json!({ "key": key, "error": format!("{error:#}") }),
    };
    if let Some(path) = request.result {
        cx.background_spawn(async move { std::fs::write(path, report.to_string()) })
            .detach_and_log_err(cx);
    }
    outcome.log_err();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn the_layer_holds_only_known_keys_and_valid_values(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert_eq!(card_fields(cx), None, "nothing overridden by default");
            set(CARD_FIELDS, Some(json!(["pr", "unread"])), cx).expect("set");
            assert_eq!(
                card_fields(cx),
                Some(vec!["pr".to_owned(), "unread".to_owned()])
            );
            assert_eq!(
                get(None, cx),
                json!({ "workspace": { "cardFields": ["pr", "unread"] } })
            );
            assert!(set(CARD_FIELDS, Some(json!("pr")), cx).is_err());
            assert!(set("sidebar.sortBy", Some(json!("name")), cx).is_err());
            set(CARD_FIELDS, None, cx).expect("unset");
            assert_eq!(card_fields(cx), None);
        });
    }

    #[gpui::test]
    async fn requests_from_the_cli_answer_in_their_result_file(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("dir");
        let result = dir.path().join("answer.json");
        let read = |path: &std::path::Path| -> Value {
            serde_json::from_str(&std::fs::read_to_string(path).expect("answer")).expect("json")
        };
        cx.update(|cx| {
            handle_request(
                ConfigRequest {
                    op: "set".into(),
                    key: Some(CARD_FIELDS.into()),
                    value: Some(json!(["ports"])),
                    result: Some(result.clone()),
                },
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(read(&result)["value"], json!(["ports"]));
        cx.update(|cx| {
            handle_request(
                ConfigRequest {
                    op: "set".into(),
                    key: Some("rules.armed".into()),
                    value: Some(json!(true)),
                    result: Some(result.clone()),
                },
                cx,
            )
        });
        cx.run_until_parked();
        assert!(
            read(&result)["error"]
                .as_str()
                .is_some_and(|error| error.contains("can't be set for one app"))
        );
    }
}
