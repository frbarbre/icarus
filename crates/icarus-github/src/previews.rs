use crate::client::GitHub;
use crate::error::{GitHubError, Result};
use crate::raw::Nodes;
use crate::runner::{Gh, GhRequest};
use icarus_rules::facts::RepoRef;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

pub const DEFAULT_ENVIRONMENT_PATTERN: &str =
    r"(?i)preview|review|(^|[^a-z])pr([-_]?\d+)?($|[^a-z])|staging-pr";
pub const DEFAULT_EXCLUDE_PATTERN: &str = r"(?i)^prod(uction)?$";
const REF_DEPLOYMENTS: usize = 30;
const PRIMARY_NAMES: &[&str] = &["frontend", "web", "webapp", "app", "site", "www"];

const QUERY: &str = "query PrPreviews($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) { pullRequest(number: $number) {
    headRefOid headRefName isCrossRepository
    comments(last: 50) { nodes { body createdAt author { __typename login } } }
    commits(last: 1) { nodes { commit { oid
      deployments(first: 50, orderBy: {field: CREATED_AT, direction: DESC}) { nodes {
        environment state createdAt commitOid latestStatus { state environmentUrl logUrl createdAt } } }
      status { contexts { context state targetUrl createdAt } }
      checkSuites(first: 50) { nodes { app { name slug }
        checkRuns(last: 10) { nodes { name status conclusion detailsUrl startedAt completedAt } } } }
    } } } } }
}";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewSource {
    Deployment,
    Status,
    Comment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    pub app: String,
    pub environment: Option<String>,
    pub url: Option<String>,
    pub state: String,
    pub sha: Option<String>,
    pub is_head: bool,
    pub created_at: Option<String>,
    pub source: PreviewSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrPreviews {
    pub previews: Vec<Preview>,
    pub primary: Option<Preview>,
}

#[derive(Debug, Clone)]
pub struct PreviewConfig {
    pub environment: Option<Regex>,
    pub exclude: Option<Regex>,
    pub comment: Option<Regex>,
    pub primary_app: Option<String>,
}

fn compile(key: &str, pattern: &str) -> Result<Regex> {
    Regex::new(pattern).map_err(|error| {
        GitHubError::Invalid(format!("previews.{key} is not a valid regex: {error}"))
    })
}

impl PreviewConfig {
    /// Reads the `previews` settings object; absent keys take the defaults.
    pub fn from_settings(settings: &Value) -> Result<Self> {
        let text = |key: &str| {
            settings
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        };
        let environment = Some(compile(
            "environment_pattern",
            text("environment_pattern").unwrap_or(DEFAULT_ENVIRONMENT_PATTERN),
        )?);
        let exclude = match settings.get("exclude_pattern") {
            Some(Value::String(s)) if s.trim().is_empty() => None,
            _ => Some(compile(
                "exclude_pattern",
                text("exclude_pattern").unwrap_or(DEFAULT_EXCLUDE_PATTERN),
            )?),
        };
        let comment = text("comment_pattern")
            .map(|pattern| compile("comment_pattern", pattern))
            .transpose()?;
        if let Some(regex) = &comment
            && !regex.capture_names().any(|name| name == Some("url"))
        {
            return Err(GitHubError::Invalid(
                "previews.comment_pattern needs a named group `url`".to_owned(),
            ));
        }
        Ok(Self {
            environment,
            exclude,
            comment,
            primary_app: text("primary_app").map(str::to_owned),
        })
    }

    pub fn is_preview_environment(&self, environment: &str) -> bool {
        self.environment
            .as_ref()
            .is_some_and(|pattern| pattern.is_match(environment))
            && !self
                .exclude
                .as_ref()
                .is_some_and(|exclude| exclude.is_match(environment))
    }
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            environment: Regex::new(DEFAULT_ENVIRONMENT_PATTERN).ok(),
            exclude: Regex::new(DEFAULT_EXCLUDE_PATTERN).ok(),
            comment: None,
            primary_app: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAuthor {
    #[serde(default, rename = "__typename")]
    typename: Option<String>,
    #[serde(default)]
    login: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawComment {
    #[serde(default)]
    body: String,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    author: Option<RawAuthor>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDeploymentStatus {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    environment_url: Option<String>,
    #[serde(default)]
    log_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDeployment {
    #[serde(default)]
    environment: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    commit_oid: Option<String>,
    #[serde(default)]
    latest_status: Option<RawDeploymentStatus>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawStatusContext {
    #[serde(default)]
    context: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    target_url: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawStatus {
    #[serde(default)]
    contexts: Vec<RawStatusContext>,
}

#[derive(Debug, Deserialize)]
struct RawApp {
    #[serde(default)]
    name: String,
    #[serde(default)]
    slug: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCheckRun {
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    details_url: Option<String>,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    completed_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCheckSuite {
    #[serde(default)]
    app: Option<RawApp>,
    #[serde(default)]
    check_runs: Option<Nodes<RawCheckRun>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCommit {
    #[serde(default)]
    deployments: Option<Nodes<RawDeployment>>,
    #[serde(default)]
    status: Option<RawStatus>,
    #[serde(default)]
    check_suites: Option<Nodes<RawCheckSuite>>,
}

#[derive(Debug, Deserialize)]
struct RawCommitNode {
    commit: RawCommit,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPreviewPr {
    head_ref_oid: String,
    #[serde(default)]
    head_ref_name: String,
    #[serde(default)]
    is_cross_repository: bool,
    #[serde(default)]
    comments: Option<Nodes<RawComment>>,
    #[serde(default)]
    commits: Option<Nodes<RawCommitNode>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRepository {
    pull_request: Option<RawPreviewPr>,
}

#[derive(Debug, Deserialize)]
struct RawData {
    repository: Option<RawRepository>,
}

#[derive(Debug, Deserialize)]
struct RestDeployment {
    id: u64,
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    environment: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RestDeploymentStatus {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    environment_url: Option<String>,
    #[serde(default)]
    log_url: Option<String>,
    #[serde(default)]
    target_url: Option<String>,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.trim().is_empty())
}

fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric() && c != '.')
        .flat_map(|t| t.split('.'))
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
}

fn is_preview_word(token: &str) -> bool {
    matches!(
        token,
        "preview" | "previews" | "review" | "deploy" | "deployment" | "deploys" | "env"
    ) || token.bytes().all(|b| b.is_ascii_digit())
        || token
            .strip_prefix("pr")
            .is_some_and(|rest| rest.bytes().all(|b| b.is_ascii_digit()))
}

fn label(text: &str, drop: impl Fn(&str) -> bool, fallback: &str) -> String {
    let kept: Vec<String> = tokens(text)
        .filter(|t| !is_preview_word(t) && !drop(t))
        .collect();
    if kept.is_empty() {
        fallback.to_lowercase()
    } else {
        kept.join("-")
    }
}

/// `frontend-preview` → `frontend`, `Preview – web` → `web`; an all-affix name keeps itself.
pub fn environment_app(environment: &str) -> String {
    label(environment, |_| false, environment.trim())
}

fn deployment_state(status: Option<&str>, deployment: Option<&str>) -> String {
    match status.or(deployment).map(str::to_lowercase).as_deref() {
        Some("active") => "success".to_owned(),
        Some("waiting") => "queued".to_owned(),
        Some("abandoned" | "destroyed") => "inactive".to_owned(),
        Some(other) => other.to_owned(),
        None => "pending".to_owned(),
    }
}

fn check_state(status: Option<&str>, conclusion: Option<&str>) -> String {
    match (
        status.map(str::to_lowercase).as_deref(),
        conclusion.map(str::to_lowercase).as_deref(),
    ) {
        (_, Some("success" | "neutral" | "skipped")) => "success",
        (_, Some("failure" | "timed_out" | "startup_failure" | "action_required")) => "failure",
        (_, Some("cancelled" | "stale")) => "error",
        (Some("in_progress"), _) => "in_progress",
        (Some("queued" | "waiting" | "requested"), _) => "queued",
        _ => "pending",
    }
    .to_owned()
}

fn status_state(state: Option<&str>) -> String {
    match state.map(str::to_lowercase).as_deref() {
        Some("expected") | None => "pending".to_owned(),
        Some(other) => other.to_owned(),
    }
}

const PROVIDER_WORDS: &[&str] = &[
    "vercel",
    "netlify",
    "cloudflare",
    "pages",
    "workers",
    "and",
    "render",
    "railway",
    "fly",
    "io",
    "amplify",
    "aws",
    "console",
    "app",
    "status",
];

/// The preview host a status or check run belongs to, if it is a known one.
pub fn preview_provider(name: &str) -> Option<&'static str> {
    let lower = name.trim().to_lowercase();
    let leads = |word: &str| {
        lower
            .strip_prefix(word)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', ':', '–', '—']))
    };
    if lower.contains("comments") {
        return None;
    }
    if lower.contains("vercel") {
        Some("vercel")
    } else if lower.contains("netlify") {
        Some("netlify")
    } else if lower.contains("cloudflare") && lower.contains("pages") {
        Some("cloudflare-pages")
    } else if leads("render") {
        Some("render")
    } else if lower.contains("railway") {
        Some("railway")
    } else if lower.contains("fly.io") || leads("fly") {
        Some("fly")
    } else if lower.contains("amplify") {
        Some("amplify")
    } else {
        None
    }
}

fn host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..end];
    Some(host.rsplit_once('@').map_or(host, |(_, h)| h))
}

/// Provider dashboards and code hosts link to a build page, not to the running preview.
fn is_dashboard(url: &str) -> bool {
    let Some(host) = host(url).map(str::to_lowercase) else {
        return true;
    };
    matches!(
        host.as_str(),
        "github.com"
            | "vercel.com"
            | "app.netlify.com"
            | "dash.cloudflare.com"
            | "dashboard.render.com"
            | "railway.app"
            | "railway.com"
            | "fly.io"
            | "linear.app"
            | "console.aws.amazon.com"
    ) || host.ends_with(".github.com")
        || host.ends_with("githubusercontent.com")
        || host.ends_with(".console.aws.amazon.com")
}

fn same_sha(head: &str, sha: &str) -> bool {
    sha.len() >= 7 && head.to_lowercase().starts_with(&sha.to_lowercase())
}

static URL: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"https?://[^\s<>()\[\]"'`|*]+"#).ok());
static SHA: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"\b[0-9a-f]{7,40}\b").ok());
static BOLD: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"\*\*([^*]+)\*\*").ok());

const APP_STOPWORDS: &[&str] = &[
    "visit", "view", "open", "the", "a", "an", "your", "latest", "this", "for", "deploy", "see",
    "ready", "new",
];

fn trim_url(url: &str) -> &str {
    url.trim_end_matches(['.', ',', ';', ':', '!', '?'])
}

fn url_app(url: &str) -> String {
    host(url)
        .and_then(|h| h.split('.').next())
        .map_or_else(|| "preview".to_owned(), str::to_lowercase)
}

fn comment_line_app(line: &str, url: &str) -> String {
    let words: Vec<String> = tokens(line).collect();
    if let Some(index) = words.iter().position(|w| w == "preview" || w == "previews")
        && let Some(before) = index.checked_sub(1).and_then(|i| words.get(i))
        && !APP_STOPWORDS.contains(&before.as_str())
        && !is_preview_word(before)
    {
        return before.clone();
    }
    if let Some(bold) = BOLD
        .as_ref()
        .and_then(|re| re.captures(line))
        .and_then(|c| c.get(1))
    {
        let app = label(bold.as_str(), |_| false, "");
        if !app.is_empty() && !APP_STOPWORDS.contains(&app.as_str()) {
            return app;
        }
    }
    url_app(url)
}

fn line_sha(line: &str) -> Option<String> {
    SHA.as_ref()?
        .find_iter(line)
        .map(|m| m.as_str())
        .find(|s| {
            s.bytes().any(|b| b.is_ascii_digit()) && s.bytes().any(|b| b.is_ascii_alphabetic())
        })
        .map(str::to_owned)
}

fn is_bot(author: Option<&RawAuthor>) -> bool {
    author.is_some_and(|a| a.typename.as_deref() == Some("Bot") || a.login.ends_with("[bot]"))
}

fn comment_previews(comment: &RawComment, head: &str, config: &PreviewConfig) -> Vec<Preview> {
    let make = |app: String, url: &str, sha: Option<String>| Preview {
        app,
        environment: None,
        url: Some(url.to_owned()),
        state: "success".to_owned(),
        is_head: sha.as_deref().is_some_and(|s| same_sha(head, s)),
        sha,
        created_at: comment.created_at.clone(),
        source: PreviewSource::Comment,
        log_url: None,
    };
    if let Some(pattern) = &config.comment {
        return pattern
            .captures_iter(&comment.body)
            .filter_map(|caps| {
                let url = trim_url(caps.name("url")?.as_str());
                let app = caps
                    .name("app")
                    .map(|m| label(m.as_str(), |_| false, m.as_str()))
                    .unwrap_or_else(|| url_app(url));
                let sha = caps.name("sha").map(|m| m.as_str().to_owned());
                Some(make(app, url, sha))
            })
            .collect();
    }
    let Some(url_re) = URL.as_ref() else {
        return Vec::new();
    };
    if !is_bot(comment.author.as_ref()) {
        return Vec::new();
    }
    let mut found = Vec::new();
    for line in comment.body.lines() {
        if !line.to_lowercase().contains("preview") {
            continue;
        }
        let Some(url) = url_re
            .find_iter(line)
            .map(|m| trim_url(m.as_str()))
            .find(|url| !is_dashboard(url))
        else {
            continue;
        };
        found.push(make(comment_line_app(line, url), url, line_sha(line)));
    }
    found
}

fn status_preview(
    name: &str,
    provider: &str,
    url: Option<String>,
    state: String,
    head: &str,
    created_at: Option<String>,
) -> Option<Preview> {
    let url = non_empty(url)?;
    let (url, log_url) = if is_dashboard(&url) {
        (None, Some(url))
    } else {
        (Some(url), None)
    };
    Some(Preview {
        app: label(name, |t| PROVIDER_WORDS.contains(&t), provider),
        environment: Some(name.to_owned()),
        url,
        state,
        sha: Some(head.to_owned()),
        is_head: true,
        created_at,
        source: PreviewSource::Status,
        log_url,
    })
}

fn commit_previews(commit: RawCommit, head: &str, config: &PreviewConfig) -> Vec<Preview> {
    let mut found = Vec::new();
    for deployment in commit.deployments.into_iter().flat_map(Nodes::items) {
        let Some(environment) = non_empty(deployment.environment) else {
            continue;
        };
        if !config.is_preview_environment(&environment) {
            continue;
        }
        let status = deployment.latest_status;
        let sha = non_empty(deployment.commit_oid).unwrap_or_else(|| head.to_owned());
        found.push(Preview {
            app: environment_app(&environment),
            url: status
                .as_ref()
                .and_then(|s| non_empty(s.environment_url.clone())),
            state: deployment_state(
                status.as_ref().and_then(|s| s.state.as_deref()),
                deployment.state.as_deref(),
            ),
            is_head: same_sha(head, &sha),
            sha: Some(sha),
            created_at: deployment.created_at,
            source: PreviewSource::Deployment,
            log_url: status.and_then(|s| non_empty(s.log_url)),
            environment: Some(environment),
        });
    }
    for context in commit.status.into_iter().flat_map(|s| s.contexts) {
        if let Some(provider) = preview_provider(&context.context)
            && let Some(preview) = status_preview(
                &context.context,
                provider,
                context.target_url,
                status_state(context.state.as_deref()),
                head,
                context.created_at,
            )
        {
            found.push(preview);
        }
    }
    for suite in commit.check_suites.into_iter().flat_map(Nodes::items) {
        let app = suite.app.map(|a| format!("{} {}", a.name, a.slug));
        for run in suite.check_runs.into_iter().flat_map(Nodes::items) {
            let identity = format!("{} {}", app.as_deref().unwrap_or_default(), run.name);
            let Some(provider) = preview_provider(&identity) else {
                continue;
            };
            let name = if preview_provider(&run.name).is_some() || app.is_none() {
                run.name.clone()
            } else {
                format!("{provider} {}", run.name)
            };
            if let Some(preview) = status_preview(
                &name,
                provider,
                run.details_url,
                check_state(run.status.as_deref(), run.conclusion.as_deref()),
                head,
                run.completed_at.or(run.started_at),
            ) {
                found.push(preview);
            }
        }
    }
    found
}

fn url_key(url: &str) -> String {
    url.trim_end_matches('/').to_lowercase()
}

/// One preview per app: the higher-priority source wins, then the head commit, then a live deployment, then the newest.
pub fn merge_previews(mut candidates: Vec<Preview>) -> Vec<Preview> {
    candidates.sort_by_key(|p| {
        (
            p.source,
            !p.is_head,
            p.state == "inactive",
            p.url.is_none(),
            Reverse(p.created_at.clone()),
        )
    });
    let mut apps = HashSet::new();
    let mut urls = HashSet::new();
    let mut kept = Vec::new();
    for preview in candidates {
        let url = preview.url.as_deref().map(url_key);
        if url.as_ref().is_some_and(|u| urls.contains(u)) {
            continue;
        }
        if !apps.insert(preview.app.to_lowercase()) {
            continue;
        }
        urls.extend(url);
        kept.push(preview);
    }
    kept.sort_by(|a, b| (a.source, &a.app).cmp(&(b.source, &b.app)));
    kept
}

fn live(preview: &Preview) -> bool {
    preview.url.is_some() && preview.state == "success"
}

/// `primary_app` if set, else a frontend-like app, else the first successful preview.
pub fn primary_preview(previews: &[Preview], primary_app: Option<&str>) -> Option<Preview> {
    if let Some(wanted) = primary_app {
        return previews
            .iter()
            .find(|p| p.app.eq_ignore_ascii_case(wanted))
            .cloned();
    }
    for name in PRIMARY_NAMES {
        let mut named = previews
            .iter()
            .filter(|p| p.url.is_some() && tokens(&p.app).any(|t| t == *name));
        let first = named.next();
        if let Some(best) = first.into_iter().chain(named).find(|p| live(p)).or(first) {
            return Some(best.clone());
        }
    }
    previews.iter().find(|p| live(p)).cloned()
}

fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

impl<G: Gh> GitHub<G> {
    /// Preview deployments of a pull request: GitHub Deployments, then preview-host statuses, then bot comments.
    pub async fn pr_previews(
        &self,
        repo: &RepoRef,
        number: u64,
        config: &PreviewConfig,
    ) -> Result<PrPreviews> {
        let data: RawData = self
            .graphql_data(
                QUERY,
                json!({ "owner": repo.owner, "name": repo.name, "number": number }),
            )
            .await?;
        let pr = data
            .repository
            .and_then(|r| r.pull_request)
            .ok_or_else(|| GitHubError::NotFound(format!("{}#{number} not found", repo.slug())))?;
        let head = pr.head_ref_oid.clone();
        let mut candidates = Vec::new();
        if let Some(commit) = pr.commits.into_iter().flat_map(Nodes::items).next() {
            candidates.extend(commit_previews(commit.commit, &head, config));
        }
        if !pr.is_cross_repository && !pr.head_ref_name.is_empty() {
            let covered: HashSet<String> = candidates
                .iter()
                .filter(|p| p.source == PreviewSource::Deployment)
                .filter_map(|p| p.environment.clone())
                .collect();
            // Why best-effort: older deployments only fill gaps, so a failed lookup must not hide the head's previews.
            if let Ok(older) = self
                .ref_deployments(repo, &pr.head_ref_name, &head, &covered, config)
                .await
            {
                candidates.extend(older);
            }
        }
        for comment in pr.comments.into_iter().flat_map(Nodes::items) {
            candidates.extend(comment_previews(&comment, &head, config));
        }
        let previews = merge_previews(candidates);
        let primary = primary_preview(&previews, config.primary_app.as_deref());
        Ok(PrPreviews { previews, primary })
    }

    async fn ref_deployments(
        &self,
        repo: &RepoRef,
        branch: &str,
        head: &str,
        covered: &HashSet<String>,
        config: &PreviewConfig,
    ) -> Result<Vec<Preview>> {
        let deployments: Vec<RestDeployment> = self
            .rest_json(GhRequest::get(format!(
                "repos/{}/{}/deployments?ref={}&per_page={REF_DEPLOYMENTS}",
                repo.owner,
                repo.name,
                encode(branch)
            )))
            .await?;
        let mut newest: BTreeMap<String, RestDeployment> = BTreeMap::new();
        for deployment in deployments {
            let Some(environment) = deployment.environment.clone() else {
                continue;
            };
            if covered.contains(&environment) || !config.is_preview_environment(&environment) {
                continue;
            }
            match newest.get(&environment) {
                Some(seen) if seen.created_at >= deployment.created_at => {}
                _ => {
                    newest.insert(environment, deployment);
                }
            }
        }
        let mut found = Vec::new();
        for (environment, deployment) in newest {
            let statuses: Vec<RestDeploymentStatus> = self
                .rest_json(GhRequest::get(format!(
                    "repos/{}/{}/deployments/{}/statuses?per_page=1",
                    repo.owner, repo.name, deployment.id
                )))
                .await
                .unwrap_or_default();
            let status = statuses.into_iter().next();
            let sha = deployment.sha.unwrap_or_default();
            found.push(Preview {
                app: environment_app(&environment),
                url: status.as_ref().and_then(|s| {
                    non_empty(s.environment_url.clone()).or_else(|| non_empty(s.target_url.clone()))
                }),
                state: deployment_state(status.as_ref().and_then(|s| s.state.as_deref()), None),
                is_head: same_sha(head, &sha),
                sha: non_empty(Some(sha)),
                created_at: deployment.created_at,
                source: PreviewSource::Deployment,
                log_url: status.and_then(|s| non_empty(s.log_url)),
                environment: Some(environment),
            });
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_names_lose_preview_affixes() {
        assert_eq!(environment_app("frontend-preview"), "frontend");
        assert_eq!(environment_app("insights-preview"), "insights");
        assert_eq!(environment_app("Preview – web"), "web");
        assert_eq!(environment_app("pr-123-api"), "api");
        assert_eq!(environment_app("Preview"), "preview");
    }

    #[test]
    fn default_patterns_keep_previews_and_drop_production() {
        let config = PreviewConfig::default();
        for name in [
            "frontend-preview",
            "Preview",
            "review-app",
            "pr-12",
            "api_pr",
            "staging-pr",
        ] {
            assert!(config.is_preview_environment(name), "{name}");
        }
        for name in [
            "production",
            "Prod",
            "staging",
            "github-pages",
            "api-production",
        ] {
            assert!(!config.is_preview_environment(name), "{name}");
        }
    }

    #[test]
    fn providers_are_recognised_by_context_or_app() {
        assert_eq!(preview_provider("Vercel – web"), Some("vercel"));
        assert_eq!(
            preview_provider("netlify/site/deploy-preview"),
            Some("netlify")
        );
        assert_eq!(
            preview_provider("Cloudflare Workers and Pages cloudflare-workers-and-pages"),
            Some("cloudflare-pages")
        );
        assert_eq!(preview_provider("Render – api"), Some("render"));
        assert_eq!(preview_provider("render-tests"), None);
        assert_eq!(preview_provider("Vercel Preview Comments"), None);
        assert_eq!(preview_provider("ci / build"), None);
    }

    #[test]
    fn dashboards_are_not_preview_urls() {
        assert!(is_dashboard("https://vercel.com/acme/web/abc"));
        assert!(is_dashboard("https://github.com/acme/web/pull/1"));
        assert!(!is_dashboard("https://web-git-feat.vercel.app"));
    }
}
