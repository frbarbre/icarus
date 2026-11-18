use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use anyhow::{Context, Result, anyhow, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::turns::{self, Turn};
use crate::{ChangeRow, ChangeStatus, Changes, ChangesQuery, CompareStatus};

pub mod imports;

pub use imports::Semantics;

const MAX_ATTR_PATHS: usize = 2000;
const MAX_TEXT_QUERY: usize = 2048;
const ANALYZER_WORKERS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FacetValue {
    pub id: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub value: String,
    pub when: Predicate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Patterns {
    One(String),
    Many(Vec<String>),
}

impl Patterns {
    fn list(&self) -> Vec<&str> {
        match self {
            Self::One(one) => vec![one.as_str()],
            Self::Many(many) => many.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Predicate {
    Path(Patterns),
    Regex(Patterns),
    Attr(AttrTest),
    Change(ChangeTest),
    Content(String),
    Builtin(String),
    All(Vec<Predicate>),
    Any(Vec<Predicate>),
    Not(Box<Predicate>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttrTest {
    pub name: String,
    pub state: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ChangeTest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Vec<ChangeStatus>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<LineRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<ChangeSource>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LineRange {
    #[serde(default)]
    pub min: Option<u64>,
    #[serde(default)]
    pub max: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeSource {
    Committed,
    Staged,
    Unstaged,
    Untracked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Facet {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub values: Vec<FacetValue>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ImportsOnlyConfig {
    pub max_changed_lines: u64,
    pub max_files: usize,
    pub max_bytes: usize,
    pub semantics: Semantics,
}

impl Default for ImportsOnlyConfig {
    fn default() -> Self {
        Self {
            max_changed_lines: 60,
            max_files: 200,
            max_bytes: 256_000,
            semantics: Semantics::OrcaV1,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Analyzers {
    #[serde(default)]
    pub imports_only: ImportsOnlyConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FilterConfig {
    #[serde(default)]
    pub facets: Vec<Facet>,
    #[serde(default)]
    pub analyzers: Analyzers,
}

#[derive(Debug, Deserialize)]
struct FacetOverlay {
    id: String,
    label: Option<String>,
    default: Option<String>,
    #[serde(default)]
    values: Vec<FacetValue>,
    #[serde(default)]
    rules: Vec<Rule>,
    #[serde(default)]
    replace: bool,
}

#[derive(Debug, Default, Deserialize)]
struct ImportsOnlyOverlay {
    max_changed_lines: Option<u64>,
    max_files: Option<usize>,
    max_bytes: Option<usize>,
    semantics: Option<Semantics>,
}

#[derive(Debug, Default, Deserialize)]
struct AnalyzersOverlay {
    #[serde(default)]
    imports_only: ImportsOnlyOverlay,
}

#[derive(Debug, Default, Deserialize)]
struct Overlay {
    #[serde(default)]
    facets: Vec<FacetOverlay>,
    #[serde(default)]
    analyzers: AnalyzersOverlay,
}

impl FilterConfig {
    pub fn preset() -> Self {
        serde_json::from_str(include_str!("filters/preset.json")).unwrap_or_default()
    }

    pub fn with_overlay(mut self, changes: &Value) -> Result<Self> {
        if changes.is_null() {
            return Ok(self);
        }
        let overlay: Overlay =
            serde_json::from_value(changes.clone()).context("invalid `changes` settings")?;
        for facet in overlay.facets {
            let existing = self.facets.iter_mut().find(|f| f.id == facet.id);
            match existing {
                Some(existing) if !facet.replace => merge_facet(existing, facet)?,
                Some(existing) => *existing = facet.into_facet(),
                None => self.facets.push(facet.into_facet()),
            }
        }
        let imports = overlay.analyzers.imports_only;
        let target = &mut self.analyzers.imports_only;
        if let Some(v) = imports.max_changed_lines {
            target.max_changed_lines = v;
        }
        if let Some(v) = imports.max_files {
            target.max_files = v;
        }
        if let Some(v) = imports.max_bytes {
            target.max_bytes = v;
        }
        if let Some(v) = imports.semantics {
            target.semantics = v;
        }
        Ok(self)
    }
}

impl FacetOverlay {
    fn into_facet(self) -> Facet {
        Facet {
            label: self.label.unwrap_or_else(|| self.id.clone()),
            id: self.id,
            default: self.default,
            values: self.values,
            rules: self.rules,
        }
    }
}

fn merge_facet(existing: &mut Facet, overlay: FacetOverlay) -> Result<()> {
    if let Some(label) = overlay.label {
        existing.label = label;
    }
    if overlay.default.is_some() {
        existing.default = overlay.default;
    }
    for value in overlay.values {
        match existing.values.iter_mut().find(|v| v.id == value.id) {
            Some(known) => *known = value,
            None => existing.values.push(value),
        }
    }
    let mut front = 0;
    for rule in overlay.rules {
        let anchor = match rule.before.as_deref() {
            None => None,
            Some(before) => Some(
                existing
                    .rules
                    .iter()
                    .position(|r| r.id.as_deref() == Some(before))
                    .ok_or_else(|| anyhow!("facet {}: no rule with id `{before}`", existing.id))?,
            ),
        };
        match anchor {
            Some(position) => {
                if position < front {
                    front += 1;
                }
                existing.rules.insert(position, rule);
            }
            None => {
                existing.rules.insert(front, rule);
                front += 1;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttrState {
    Set,
    Unset,
    Unspecified,
    Value(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Builtin {
    GeneratedHeuristic,
    TestHeuristic,
    LinguistGenerated,
    LinguistDocumentation,
}

#[derive(Debug, Clone)]
enum Test {
    Glob(GlobSet),
    Regex(Vec<Regex>),
    Attr { name: String, state: AttrState },
    Change(ChangeTest),
    ImportsOnly,
    Builtin(Builtin),
    All(Vec<Test>),
    Any(Vec<Test>),
    Not(Box<Test>),
}

fn regex(pattern: &str, case_insensitive: bool) -> Regex {
    RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
        .unwrap_or_else(|_| imports::never())
}

// source-control.md §3.4, verbatim from Orca's generated-code-path.ts and test-code-path.ts.
static GENERATED_DIRECTORY: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?:^|[/\\])(?:__generated__|__pycache__|\.next|\.nuxt|coverage|dist|generated|node_modules|vendor)[/\\]",
        true,
    )
});
static LOCKFILE: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?:^|[/\\])(?:bun\.lockb?|cargo\.lock|composer\.lock|flake\.lock|gemfile\.lock|go\.sum|gradle\.lockfile|mix\.lock|npm-shrinkwrap\.json|package-lock\.json|packages\.lock\.json|pipfile\.lock|pnpm-lock\.yaml|poetry\.lock|pubspec\.lock|uv\.lock|yarn\.lock)$",
        true,
    )
});
static GENERATED_BASENAME: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?:\.(?:generated|designer)\.[^./\\]+|[._](?:pb|pb2|pb2_grpc)\.(?:[^./\\]+\.)*[^./\\]+|[._]gen\.[^./\\]+|_generated\.[^./\\]+|\.(?:g|freezed)\.dart|\.min\.(?:js|css|mjs)|\.(?:js|css|mjs)\.map|\.snap)$",
        true,
    )
});
static TEST_DIRECTORY: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?:^|[/\\])(?:__mocks__|__snapshots__|__tests__|cypress|e2e|spec|tests?|testdata)[/\\]",
        true,
    )
});
static TEST_BASENAME: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?:\.(?:test|spec)\.[^./\\]+|[-_](?:test|spec)s?\.[^./\\]+|(?:^|[/\\])test_[^./\\]*\.py|(?:^|[/\\])conftest\.py|\.snap)$",
        true,
    )
});
static TEST_TYPE_NAME_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?:Test|Tests|Spec)\.(?:java|kt|kts|scala|groovy|cs|swift|php|rb)$",
        false,
    )
});

pub fn is_generated_path(path: &str) -> bool {
    GENERATED_BASENAME.is_match(path)
        || LOCKFILE.is_match(path)
        || GENERATED_DIRECTORY.is_match(path)
}

pub fn is_test_path(path: &str) -> bool {
    TEST_DIRECTORY.is_match(path)
        || TEST_BASENAME.is_match(path)
        || TEST_TYPE_NAME_SUFFIX.is_match(path)
}

fn glob_set(patterns: &[&str]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let anchored = pattern.starts_with('/');
        let body = pattern.trim_start_matches('/');
        let directory_only = body.ends_with('/');
        let body = body.trim_end_matches('/');
        let base = if !anchored && !body.contains('/') {
            format!("**/{body}")
        } else {
            body.to_string()
        };
        let mut variants = vec![format!("{base}/**")];
        if !directory_only {
            variants.push(base);
        }
        for variant in variants {
            let glob = GlobBuilder::new(&variant)
                .literal_separator(true)
                .build()
                .with_context(|| format!("invalid path glob `{pattern}`"))?;
            builder.add(glob);
        }
    }
    Ok(builder.build()?)
}

fn compile(
    predicate: &Predicate,
    attrs: &mut BTreeSet<String>,
    imports: &mut bool,
) -> Result<Test> {
    Ok(match predicate {
        Predicate::Path(patterns) => Test::Glob(glob_set(&patterns.list())?),
        Predicate::Regex(patterns) => Test::Regex(
            patterns
                .list()
                .into_iter()
                .map(|p| Regex::new(p).with_context(|| format!("invalid path regex `{p}`")))
                .collect::<Result<_>>()?,
        ),
        Predicate::Attr(test) => {
            let state = match test.state.as_str() {
                "set" | "true" => AttrState::Set,
                "unset" | "false" => AttrState::Unset,
                "unspecified" => AttrState::Unspecified,
                other => match other.strip_prefix("value=") {
                    Some(value) => AttrState::Value(value.to_string()),
                    None => bail!(
                        "attribute state must be set, unset, unspecified or value=<v>, not `{other}`"
                    ),
                },
            };
            attrs.insert(test.name.clone());
            Test::Attr {
                name: test.name.clone(),
                state,
            }
        }
        Predicate::Change(test) => Test::Change(test.clone()),
        Predicate::Content(name) => match name.as_str() {
            "imports_only" => {
                *imports = true;
                Test::ImportsOnly
            }
            other => bail!("unknown content analyzer `{other}`"),
        },
        Predicate::Builtin(name) => {
            let builtin = match name.as_str() {
                "generated_heuristic" => Builtin::GeneratedHeuristic,
                "test_heuristic" => Builtin::TestHeuristic,
                "linguist_generated" => Builtin::LinguistGenerated,
                "linguist_documentation" => Builtin::LinguistDocumentation,
                other => bail!("unknown builtin predicate `{other}`"),
            };
            match builtin {
                Builtin::LinguistGenerated => {
                    attrs.insert("linguist-generated".into());
                }
                Builtin::LinguistDocumentation => {
                    attrs.insert("linguist-documentation".into());
                }
                _ => {}
            }
            Test::Builtin(builtin)
        }
        Predicate::All(items) => Test::All(
            items
                .iter()
                .map(|p| compile(p, attrs, imports))
                .collect::<Result<_>>()?,
        ),
        Predicate::Any(items) => Test::Any(
            items
                .iter()
                .map(|p| compile(p, attrs, imports))
                .collect::<Result<_>>()?,
        ),
        Predicate::Not(inner) => Test::Not(Box::new(compile(inner, attrs, imports)?)),
    })
}

#[derive(Debug, Clone, Copy)]
pub struct Facts<'a> {
    pub path: &'a str,
    pub row: Option<&'a ChangeRow>,
    pub attrs: Option<&'a BTreeMap<String, String>>,
    pub imports_only: bool,
}

fn attr_matches(info: Option<&str>, state: &AttrState) -> bool {
    match state {
        AttrState::Set => matches!(info, Some("set" | "true")),
        AttrState::Unset => matches!(info, Some("unset" | "false")),
        AttrState::Unspecified => matches!(info, None | Some("unspecified")),
        AttrState::Value(value) => info == Some(value.as_str()),
    }
}

fn change_matches(test: &ChangeTest, row: Option<&ChangeRow>) -> bool {
    let Some(row) = row else { return false };
    if let Some(kinds) = &test.kind
        && !kinds.contains(&row.status)
    {
        return false;
    }
    if let Some(binary) = test.binary
        && row.binary != binary
    {
        return false;
    }
    if let Some(range) = &test.lines {
        let Some(lines) = row.added.zip(row.removed).map(|(a, r)| a + r) else {
            return false;
        };
        if range.min.is_some_and(|min| lines < min) || range.max.is_some_and(|max| lines > max) {
            return false;
        }
    }
    if let Some(sources) = &test.sources {
        let has = |source: &ChangeSource| match source {
            ChangeSource::Committed => row.committed,
            ChangeSource::Staged => row.staged,
            ChangeSource::Unstaged => row.unstaged,
            ChangeSource::Untracked => row.untracked,
        };
        if !sources.iter().any(has) {
            return false;
        }
    }
    true
}

impl Test {
    fn matches(&self, facts: &Facts<'_>) -> bool {
        let attr = |name: &str| facts.attrs.and_then(|a| a.get(name)).map(String::as_str);
        match self {
            Self::Glob(set) => set.is_match(facts.path),
            Self::Regex(patterns) => patterns.iter().any(|r| r.is_match(facts.path)),
            Self::Attr { name, state } => attr_matches(attr(name), state),
            Self::Change(test) => change_matches(test, facts.row),
            Self::ImportsOnly => facts.imports_only,
            Self::Builtin(Builtin::GeneratedHeuristic) => is_generated_path(facts.path),
            Self::Builtin(Builtin::TestHeuristic) => is_test_path(facts.path),
            Self::Builtin(Builtin::LinguistGenerated) => {
                attr_matches(attr("linguist-generated"), &AttrState::Set)
            }
            Self::Builtin(Builtin::LinguistDocumentation) => {
                attr_matches(attr("linguist-documentation"), &AttrState::Set)
            }
            Self::All(items) => items.iter().all(|t| t.matches(facts)),
            Self::Any(items) => items.iter().any(|t| t.matches(facts)),
            Self::Not(inner) => !inner.matches(facts),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Classifier {
    config: FilterConfig,
    rules: Vec<Vec<Test>>,
    attrs: Vec<String>,
    needs_imports: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FacetMatch {
    pub facet: String,
    pub value: Option<String>,
    pub rule: Option<usize>,
    pub rule_id: Option<String>,
    pub when: Option<Predicate>,
}

pub type FacetValues = BTreeMap<String, Option<String>>;

impl Classifier {
    pub fn new(config: FilterConfig) -> Result<Self> {
        let mut attrs = BTreeSet::new();
        let mut needs_imports = false;
        let mut rules = Vec::new();
        let mut seen = BTreeSet::new();
        for facet in &config.facets {
            if !seen.insert(facet.id.as_str()) {
                bail!("facet `{}` is defined twice", facet.id);
            }
            let known: BTreeSet<&str> = facet.values.iter().map(|v| v.id.as_str()).collect();
            if let Some(default) = &facet.default
                && !known.contains(default.as_str())
            {
                bail!(
                    "facet {}: default `{default}` is not one of its values",
                    facet.id
                );
            }
            let mut compiled = Vec::new();
            for rule in &facet.rules {
                if !known.contains(rule.value.as_str()) {
                    bail!(
                        "facet {}: rule value `{}` is not one of its values",
                        facet.id,
                        rule.value
                    );
                }
                compiled.push(compile(&rule.when, &mut attrs, &mut needs_imports)?);
            }
            rules.push(compiled);
        }
        Ok(Self {
            config,
            rules,
            attrs: attrs.into_iter().collect(),
            needs_imports,
        })
    }

    pub fn config(&self) -> &FilterConfig {
        &self.config
    }

    pub fn attributes(&self) -> &[String] {
        &self.attrs
    }

    pub fn explain_facts(&self, facts: &Facts<'_>) -> Vec<FacetMatch> {
        self.config
            .facets
            .iter()
            .zip(&self.rules)
            .map(
                |(facet, tests)| match tests.iter().position(|t| t.matches(facts)) {
                    Some(index) => {
                        let rule = &facet.rules[index];
                        FacetMatch {
                            facet: facet.id.clone(),
                            value: Some(rule.value.clone()),
                            rule: Some(index),
                            rule_id: rule.id.clone(),
                            when: Some(rule.when.clone()),
                        }
                    }
                    None => FacetMatch {
                        facet: facet.id.clone(),
                        value: facet.default.clone(),
                        rule: None,
                        rule_id: None,
                        when: None,
                    },
                },
            )
            .collect()
    }

    pub fn classify(&self, facts: &Facts<'_>) -> FacetValues {
        self.explain_facts(facts)
            .into_iter()
            .map(|m| (m.facet, m.value))
            .collect()
    }

    pub fn gather(&self, worktree: &Path, base: &Changes, paths: &BTreeSet<String>) -> Gathered {
        let paths: Vec<String> = paths.iter().take(MAX_ATTR_PATHS).cloned().collect();
        let attrs = if self.attrs.is_empty() {
            BTreeMap::new()
        } else {
            check_attributes(worktree, &self.attrs, &paths)
        };
        let imports_only = match &base.compare {
            CompareStatus::Ready { merge_base, .. } if self.needs_imports => imports_only_paths(
                worktree,
                merge_base,
                &base.rows,
                &self.config.analyzers.imports_only,
            ),
            _ => BTreeSet::new(),
        };
        Gathered {
            attrs,
            imports_only,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Gathered {
    pub attrs: BTreeMap<String, BTreeMap<String, String>>,
    pub imports_only: BTreeSet<String>,
}

impl Gathered {
    pub fn facts<'a>(&'a self, path: &'a str, row: Option<&'a ChangeRow>) -> Facts<'a> {
        Facts {
            path,
            row,
            attrs: self.attrs.get(path),
            imports_only: self.imports_only.contains(path),
        }
    }
}

// Any failure means no attributes, so older hosts fall back to path rules only.
pub fn check_attributes(
    worktree: &Path,
    names: &[String],
    paths: &[String],
) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    if names.is_empty() || paths.is_empty() {
        return out;
    }
    let child = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["-c", "core.quotePath=false", "check-attr", "-z", "--stdin"])
        .args(names)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return out };
    if let Some(mut stdin) = child.stdin.take() {
        let mut input = Vec::new();
        for path in paths {
            input.extend_from_slice(path.as_bytes());
            input.push(0);
        }
        // A writer thread avoids a pipe deadlock when git's output fills before we finish writing.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let Ok(output) = child.wait_with_output() else {
        return out;
    };
    if !output.status.success() {
        return out;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<&str> = text.split('\0').collect();
    for triple in fields.chunks(3) {
        if let [path, name, info] = triple
            && !path.is_empty()
            && *info != "unspecified"
        {
            out.entry(path.to_string())
                .or_insert_with(BTreeMap::new)
                .insert(name.to_string(), info.to_string());
        }
    }
    out
}

pub fn imports_only_paths(
    worktree: &Path,
    merge_base: &str,
    rows: &[ChangeRow],
    config: &ImportsOnlyConfig,
) -> BTreeSet<String> {
    let mut candidates: Vec<&str> = rows
        .iter()
        .filter(|r| imports::is_supported(&r.path))
        .filter(|r| {
            let lines = r.added.unwrap_or(0) + r.removed.unwrap_or(0);
            lines > 0 && lines <= config.max_changed_lines
        })
        .map(|r| r.path.as_str())
        .collect();
    candidates.sort_unstable();
    candidates.truncate(config.max_files);
    let check = |path: &str| -> bool {
        let Ok(before) = crate::git_read(worktree, &["show", &format!("{merge_base}:{path}")])
        else {
            return false;
        };
        let Ok(after) = std::fs::read_to_string(worktree.join(path)) else {
            return false;
        };
        if before.chars().count() > config.max_bytes || after.chars().count() > config.max_bytes {
            return false;
        }
        imports::is_import_only_change(path, &before, &after, config.semantics)
    };
    let chunk = candidates.len().div_ceil(ANALYZER_WORKERS).max(1);
    std::thread::scope(|scope| {
        let workers: Vec<_> = candidates
            .chunks(chunk)
            .map(|paths| {
                scope.spawn(move || {
                    paths
                        .iter()
                        .filter(|p| check(p))
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .filter_map(|w| w.join().ok())
            .flatten()
            .collect()
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Scope {
    #[default]
    Base,
    Uncommitted,
    Turn(String),
    Commit(String),
}

impl Scope {
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        Ok(match text {
            "" | "base" | "all" => Self::Base,
            "uncommitted" => Self::Uncommitted,
            "turn" => Self::Turn("latest".into()),
            _ => match text.split_once(':') {
                Some(("turn", name)) if !name.is_empty() => Self::Turn(name.into()),
                Some(("commit", sha)) if !sha.is_empty() => Self::Commit(sha.into()),
                _ => bail!(
                    "scope must be base, uncommitted, turn:<ref> or commit:<sha>, not `{text}`"
                ),
            },
        })
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Base => write!(f, "base"),
            Self::Uncommitted => write!(f, "uncommitted"),
            Self::Turn(name) => write!(f, "turn:{name}"),
            Self::Commit(sha) => write!(f, "commit:{sha}"),
        }
    }
}

pub type Hidden = BTreeMap<String, BTreeSet<String>>;

#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub base: Option<String>,
    pub scope: Scope,
    pub hidden: Hidden,
    pub text: Option<String>,
    pub all_rows: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ClassifiedRow {
    #[serde(flatten)]
    pub row: ChangeRow,
    pub facets: FacetValues,
    pub discardable: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq)]
pub struct Totals {
    pub files: usize,
    pub added: u64,
    pub removed: u64,
}

impl Totals {
    fn of<'a>(rows: impl IntoIterator<Item = &'a ChangeRow>) -> Self {
        let mut totals = Self::default();
        for row in rows {
            totals.files += 1;
            totals.added += row.added.unwrap_or(0);
            totals.removed += row.removed.unwrap_or(0);
        }
        totals
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ValueSummary {
    pub id: String,
    pub label: String,
    pub present: bool,
    pub hidden: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FacetSummary {
    pub id: String,
    pub label: String,
    pub values: Vec<ValueSummary>,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CommitInfo {
    pub oid: String,
    pub parent: Option<String>,
    pub subject: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Listing {
    pub scope: String,
    pub compare: CompareStatus,
    pub turn: Option<Turn>,
    pub commit: Option<CommitInfo>,
    pub rows: Vec<ClassifiedRow>,
    /// Every row in scope, shown or not, so a client can re-filter without asking again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all_rows: Option<Vec<ClassifiedRow>>,
    pub totals: Totals,
    pub unfiltered: Totals,
    pub facets: Vec<FacetSummary>,
    pub hidden: Hidden,
    pub text: Option<String>,
    pub added: u64,
    pub removed: u64,
}

fn text_matches(query: Option<&str>, path: &str) -> bool {
    let Some(query) = query.map(str::trim).filter(|q| !q.is_empty()) else {
        return true;
    };
    if query.len() > MAX_TEXT_QUERY {
        return false;
    }
    path.to_lowercase().contains(&query.to_lowercase())
}

fn is_shown(values: &FacetValues, hidden: &Hidden) -> bool {
    values.iter().all(|(facet, value)| match value {
        Some(value) => !hidden.get(facet).is_some_and(|h| h.contains(value)),
        None => true,
    })
}

fn base_changes(worktree: &Path, base: &Option<String>) -> Result<Changes> {
    crate::changes(
        worktree,
        &ChangesQuery {
            base: base.clone(),
            uncommitted_only: false,
        },
    )
}

// Facets and text apply in every scope (source-control.md §8.1).
pub fn list(worktree: &Path, classifier: &Classifier, query: &ListQuery) -> Result<Listing> {
    let base = base_changes(worktree, &query.base)?;
    let mut turn = None;
    let mut commit = None;
    let scope_rows = match &query.scope {
        Scope::Base => base.rows.clone(),
        Scope::Uncommitted => {
            crate::changes(
                worktree,
                &ChangesQuery {
                    base: query.base.clone(),
                    uncommitted_only: true,
                },
            )?
            .rows
        }
        Scope::Turn(name) => {
            let found = turns::resolve_turn(worktree, name)?;
            let rows = crate::commit_rows(worktree, Some(&found.parent), &found.oid)?;
            turn = Some(found);
            rows
        }
        Scope::Commit(sha) => {
            let oid =
                crate::resolve_commit(worktree, sha).ok_or_else(|| anyhow!("no commit {sha}"))?;
            let parent = crate::first_parent(worktree, &oid);
            let rows = crate::commit_rows(worktree, parent.as_deref(), &oid)?;
            let subject = crate::git_read(worktree, &["show", "-s", "--format=%s", &oid])?
                .trim()
                .to_string();
            commit = Some(CommitInfo {
                oid,
                parent,
                subject,
            });
            rows
        }
    };
    let paths: BTreeSet<String> = base
        .rows
        .iter()
        .chain(&scope_rows)
        .map(|r| r.path.clone())
        .collect();
    let gathered = classifier.gather(worktree, &base, &paths);
    let live = matches!(query.scope, Scope::Base | Scope::Uncommitted);
    let mut present: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut note = |values: &FacetValues| {
        for (facet, value) in values {
            if let Some(value) = value {
                present
                    .entry(facet.clone())
                    .or_default()
                    .insert(value.clone());
            }
        }
    };
    for row in &base.rows {
        note(&classifier.classify(&gathered.facts(&row.path, Some(row))));
    }
    let mut classified = Vec::new();
    for row in &scope_rows {
        let values = classifier.classify(&gathered.facts(&row.path, Some(row)));
        note(&values);
        classified.push((row, values));
    }
    let unfiltered = Totals::of(scope_rows.iter());
    let every: Vec<ClassifiedRow> = classified
        .into_iter()
        .map(|(row, facets)| ClassifiedRow {
            discardable: live && (row.unstaged || row.untracked) && !row.conflicted,
            row: row.clone(),
            facets,
        })
        .collect();
    let rows: Vec<ClassifiedRow> = every
        .iter()
        .filter(|row| {
            is_shown(&row.facets, &query.hidden)
                && text_matches(query.text.as_deref(), &row.row.path)
        })
        .cloned()
        .collect();
    let all_rows = query.all_rows.then_some(every);
    let totals = Totals::of(rows.iter().map(|r| &r.row));
    let facets = classifier
        .config
        .facets
        .iter()
        .map(|facet| {
            let present = present.get(&facet.id);
            let hidden = query.hidden.get(&facet.id);
            let values: Vec<ValueSummary> = facet
                .values
                .iter()
                .map(|v| ValueSummary {
                    id: v.id.clone(),
                    label: v.label.clone(),
                    present: present.is_some_and(|p| p.contains(&v.id)),
                    hidden: hidden.is_some_and(|h| h.contains(&v.id)),
                })
                .collect();
            FacetSummary {
                id: facet.id.clone(),
                label: facet.label.clone(),
                active: values.iter().any(|v| v.present && v.hidden),
                values,
            }
        })
        .collect();
    Ok(Listing {
        scope: query.scope.to_string(),
        compare: base.compare,
        turn,
        commit,
        added: totals.added,
        removed: totals.removed,
        rows,
        all_rows,
        totals,
        unfiltered,
        facets,
        hidden: query.hidden.clone(),
        text: query.text.clone(),
    })
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Explanation {
    pub path: String,
    pub row: Option<ChangeRow>,
    pub attributes: BTreeMap<String, String>,
    pub imports_only: bool,
    pub generated_heuristic: bool,
    pub test_heuristic: bool,
    pub facets: Vec<FacetMatch>,
}

pub fn explain(
    worktree: &Path,
    classifier: &Classifier,
    base: Option<String>,
    path: &str,
) -> Result<Explanation> {
    let base = base_changes(worktree, &base)?;
    let row = base.rows.iter().find(|r| r.path == path).cloned();
    let gathered = classifier.gather(worktree, &base, &BTreeSet::from([path.to_string()]));
    let facts = gathered.facts(path, row.as_ref());
    Ok(Explanation {
        path: path.to_string(),
        attributes: facts.attrs.cloned().unwrap_or_default(),
        imports_only: facts.imports_only,
        generated_heuristic: is_generated_path(path),
        test_heuristic: is_test_path(path),
        facets: classifier.explain_facts(&facts),
        row,
    })
}

#[cfg(test)]
mod tests;
