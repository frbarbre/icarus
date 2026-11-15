use serde_json::Value;

use super::classify::Kind;
use super::urls::{ParsedUrl, extract_urls, parse_url, strip_ansi};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelEntry {
    pub label: String,
    pub url: ParsedUrl,
    pub kind: Option<Kind>,
}

pub fn parse_labels(output: &str) -> Vec<LabelEntry> {
    let text = strip_ansi(output);
    let trimmed = text.trim();
    let mut entries = if trimmed.starts_with('[') || trimmed.starts_with('{') {
        serde_json::from_str::<Value>(trimmed)
            .map(|value| from_json(&value))
            .unwrap_or_else(|_| from_lines(&text))
    } else {
        from_lines(&text)
    };
    let mut seen = std::collections::HashSet::new();
    entries.retain(|entry| seen.insert(entry.url.url.clone()));
    entries
}

fn from_json(value: &Value) -> Vec<LabelEntry> {
    let entry = |label: &str, url: &str, kind: Option<&str>| {
        let label = label.trim();
        let url = parse_url(url.trim())?;
        (!label.is_empty()).then(|| LabelEntry {
            label: label.to_string(),
            url,
            kind: kind.and_then(Kind::parse),
        })
    };
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                entry(
                    item.get("label")
                        .or_else(|| item.get("name"))
                        .and_then(Value::as_str)?,
                    item.get("url").and_then(Value::as_str)?,
                    item.get("kind").and_then(Value::as_str),
                )
            })
            .collect(),
        Value::Object(map) => map
            .iter()
            .filter_map(|(label, item)| match item {
                Value::String(url) => entry(label, url, None),
                Value::Object(_) => entry(
                    label,
                    item.get("url").and_then(Value::as_str)?,
                    item.get("kind").and_then(Value::as_str),
                ),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn from_lines(text: &str) -> Vec<LabelEntry> {
    text.lines()
        .filter_map(|line| {
            let url = extract_urls(line).into_iter().next()?;
            let before = &line[..line.find(&url.url)?];
            let label = before
                .trim()
                .trim_end_matches([':', '-', '=', '>', '→', '|'])
                .trim()
                .trim_start_matches(['-', '*', '•'])
                .trim();
            (!label.is_empty()).then(|| LabelEntry {
                label: label.to_string(),
                url,
                kind: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(output: &str) -> Vec<(String, String, u16, Option<Kind>)> {
        parse_labels(output)
            .into_iter()
            .map(|e| (e.label, e.url.url, e.url.port, e.kind))
            .collect()
    }

    #[test]
    fn parses_label_url_lines() {
        let output = "Frontend   http://feat-x.localhost:1355\nAPI: http://localhost:8000/docs\n\u{1b}[1mStorybook\u{1b}[0m -> http://localhost:6006\nnoise without url\n  - Admin = https://admin.localhost\n";
        assert_eq!(
            summary(output),
            [
                (
                    "Frontend".into(),
                    "http://feat-x.localhost:1355".into(),
                    1355,
                    None
                ),
                (
                    "API".into(),
                    "http://localhost:8000/docs".into(),
                    8000,
                    None
                ),
                (
                    "Storybook".into(),
                    "http://localhost:6006".into(),
                    6006,
                    None
                ),
                ("Admin".into(), "https://admin.localhost".into(), 443, None),
            ]
        );
    }

    #[test]
    fn skips_lines_without_a_label() {
        assert!(summary("http://localhost:3000\n").is_empty());
    }

    #[test]
    fn parses_json_arrays_and_objects() {
        let array =
            r#"[{"label":"Web","url":"http://localhost:3000","kind":"frontend"},{"label":"Bad"}]"#;
        assert_eq!(
            summary(array),
            [(
                "Web".into(),
                "http://localhost:3000".into(),
                3000,
                Some(Kind::Frontend)
            )]
        );
        let object = r#"{"Frontend":"http://localhost:5173","API":{"url":"http://localhost:8000","kind":"api"}}"#;
        let mut parsed = summary(object);
        parsed.sort_by_key(|entry| entry.2);
        assert_eq!(
            parsed,
            [
                (
                    "Frontend".into(),
                    "http://localhost:5173".into(),
                    5173,
                    None
                ),
                (
                    "API".into(),
                    "http://localhost:8000".into(),
                    8000,
                    Some(Kind::Api)
                ),
            ]
        );
    }

    #[test]
    fn duplicate_urls_keep_the_first_label() {
        let output = "Frontend http://localhost:3000\nAlso http://localhost:3000\n";
        assert_eq!(summary(output).len(), 1);
    }
}
