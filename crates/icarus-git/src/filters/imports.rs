use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum Semantics {
    #[default]
    #[serde(rename = "orca-v1")]
    OrcaV1,
    #[serde(rename = "v2")]
    V2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    Script,
    Python,
}

static MODULE_SOURCE_END: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:from\s*|^\s*import\s*)['"][^'"]+['"]\s*;?\s*$"#).unwrap_or_else(|_| never())
});
static SCRIPT_IMPORT_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*import(?:\s|\{|\*|['"])"#).unwrap_or_else(|_| never()));
static SCRIPT_REEXPORT_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*export\s+(?:type\s+)?(?:\*|\{)").unwrap_or_else(|_| never()));
static PYTHON_IMPORT_START: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:import\s+\S|from\s+\S+\s+import\b)").unwrap_or_else(|_| never())
});
static FROM_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bfrom\b").unwrap_or_else(|_| never()));
static WHITESPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").unwrap_or_else(|_| never()));

// The patterns are constants, so this fallback only keeps panics out of library code.
pub(crate) fn never() -> Regex {
    Regex::new(r"[^\s\S]").unwrap_or_else(|_| unreachable!())
}

fn language_of(path: &str) -> Option<Language> {
    let extension = path.rsplit('.').next()?.to_ascii_lowercase();
    match extension.as_str() {
        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => Some(Language::Script),
        "py" | "pyi" => Some(Language::Python),
        _ => None,
    }
}

pub fn is_supported(path: &str) -> bool {
    path.contains('.') && language_of(path).is_some()
}

fn script_import_end(lines: &[&str], start: usize) -> Option<usize> {
    let first = lines[start];
    let reexport = SCRIPT_REEXPORT_START.is_match(first);
    if !reexport && !SCRIPT_IMPORT_START.is_match(first) {
        return None;
    }
    for (index, line) in lines.iter().enumerate().skip(start) {
        if MODULE_SOURCE_END.is_match(line) {
            return Some(index);
        }
        // `export { a }` without `from` exports local names, which is code.
        if reexport && line.contains('}') && !FROM_WORD.is_match(line) {
            return None;
        }
    }
    None
}

fn python_import_end(lines: &[&str], start: usize) -> Option<usize> {
    if !PYTHON_IMPORT_START.is_match(lines[start]) {
        return None;
    }
    let mut index = start;
    if lines[index].contains('(') && !lines[index].contains(')') {
        while index < lines.len() - 1 && !lines[index].contains(')') {
            index += 1;
        }
        return Some(index);
    }
    while index < lines.len() - 1 && lines[index].trim_end().ends_with('\\') {
        index += 1;
    }
    Some(index)
}

struct Split {
    imports: Vec<String>,
    code: Vec<String>,
}

fn split(text: &str, language: Language) -> Split {
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let end_of = match language {
        Language::Script => script_import_end,
        Language::Python => python_import_end,
    };
    let mut imports = Vec::new();
    let mut code = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        match end_of(&lines, index) {
            None => {
                if !lines[index].trim().is_empty() {
                    code.push(lines[index].to_string());
                }
                index += 1;
            }
            Some(end) => {
                let statement = lines[index..=end].join(" ");
                imports.push(WHITESPACE.replace_all(&statement, " ").trim().to_string());
                index = end + 1;
            }
        }
    }
    Split { imports, code }
}

pub fn is_import_only_change(path: &str, before: &str, after: &str, semantics: Semantics) -> bool {
    let Some(language) = language_of(path) else {
        return false;
    };
    let old = split(before, language);
    let new = split(after, language);
    if old.code != new.code {
        return false;
    }
    match semantics {
        Semantics::OrcaV1 => {
            let mut a = old.imports;
            let mut b = new.imports;
            a.sort();
            b.sort();
            a != b
        }
        Semantics::V2 => {
            let raw = |text: &str| -> Vec<String> {
                text.split('\n')
                    .map(|l| l.trim_end_matches('\r').to_string())
                    .filter(|l| !l.trim().is_empty())
                    .collect()
            };
            raw(before) != raw(after)
        }
    }
}
