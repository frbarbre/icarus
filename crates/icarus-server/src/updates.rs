//! Update checks (plan D9): the newest GitHub release of Icarus against the running version,
//! through the user's `gh` login when there is one, else plain HTTPS.

use std::cmp::Ordering;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use icarus_protocol::RpcError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::next_action::setting;
use crate::notifications::{NewNotification, UPDATE_AVAILABLE};
use crate::server::{Server, rpc, to_value};

pub const REPO: &str = "frbarbre/icarus";

/// The release workflow stamps `ICARUS_VERSION` from the tag; local builds report the crate version.
pub const VERSION: &str = match option_env!("ICARUS_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

/// Dev builds never check in the background: their version means nothing, and tests must stay offline.
pub const RELEASE_BUILD: bool = option_env!("ICARUS_VERSION").is_some();

const DEFAULT_INTERVAL_MS: u64 = 6 * 60 * 60 * 1000;
const MIN_INTERVAL_MS: u64 = 60_000;
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(30);
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const NOTES_MAX_CHARS: usize = 4_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

/// The fields of GitHub's release object that updates use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    pub html_url: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    NotFound(String),
    Failed(String),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(message) | Self::Failed(message) => f.write_str(message),
        }
    }
}

/// Where releases come from. Blocking; the server calls it off the async runtime.
pub trait ReleaseSource: Send + Sync {
    /// The newest published non-prerelease, or `None` when the repository has none.
    fn latest(&self, repo: &str) -> Result<Option<Release>, SourceError>;
    fn name(&self) -> &'static str;
}

pub fn parse_release(body: &[u8]) -> Result<Option<Release>, SourceError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| SourceError::Failed(format!("release JSON: {e}")))?;
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|e| SourceError::Failed(format!("release JSON: {e}")))
}

/// `gh api` first (works while the repository is private), then unauthenticated HTTPS.
pub struct LiveSource;

impl ReleaseSource for LiveSource {
    fn latest(&self, repo: &str) -> Result<Option<Release>, SourceError> {
        match gh_latest(repo) {
            Some(found) => found,
            None => https_latest(repo),
        }
    }

    fn name(&self) -> &'static str {
        "github"
    }
}

/// `None` when `gh` is missing or not signed in, so the caller falls back to HTTPS.
fn gh_latest(repo: &str) -> Option<Result<Option<Release>, SourceError>> {
    let output = Command::new("gh")
        .args([
            "api",
            "-H",
            "Accept: application/vnd.github+json",
            &format!("repos/{repo}/releases/latest"),
        ])
        .output()
        .ok()?;
    if output.status.success() {
        return Some(parse_release(&output.stdout));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("HTTP 404") {
        return Some(Ok(None));
    }
    tracing::debug!("gh release check failed: {}", stderr.trim());
    None
}

fn https_agent() -> Result<ureq::Agent, SourceError> {
    static ROOTS: OnceLock<Result<Vec<ureq::tls::Certificate<'static>>, String>> = OnceLock::new();
    let roots = ROOTS
        .get_or_init(|| {
            let certs: Vec<_> = rustls_native_certs::load_native_certs()
                .certs
                .iter()
                .map(|der| ureq::tls::Certificate::from_der(der.as_ref()).to_owned())
                .collect();
            if certs.is_empty() {
                Err("no root certificates found on this system".to_owned())
            } else {
                Ok(certs)
            }
        })
        .clone()
        .map_err(SourceError::Failed)?;
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::from(roots))
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    Ok(ureq::Agent::config_builder()
        .tls_config(tls)
        .http_status_as_error(false)
        .timeout_global(Some(CHECK_TIMEOUT))
        .user_agent("icarus")
        .build()
        .new_agent())
}

fn https_latest(repo: &str) -> Result<Option<Release>, SourceError> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let mut response = https_agent()?
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| SourceError::Failed(format!("GET {url}: {e}")))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(4 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| SourceError::Failed(e.to_string()))?;
    match status {
        200 => parse_release(&body),
        404 => Err(SourceError::NotFound(format!(
            "{repo} has no published release, or it is private: sign in with `gh auth login` to check a private repository"
        ))),
        other => Err(SourceError::Failed(format!(
            "GitHub answered HTTP {other} for {url}"
        ))),
    }
}

/// A JSON file holding a release object or `null`; for tests and dry runs (`ICARUS_UPDATE_FIXTURE`).
pub struct FixtureSource(pub PathBuf);

impl ReleaseSource for FixtureSource {
    fn latest(&self, _repo: &str) -> Result<Option<Release>, SourceError> {
        let body = std::fs::read(&self.0)
            .map_err(|e| SourceError::Failed(format!("read {}: {e}", self.0.display())))?;
        parse_release(&body)
    }

    fn name(&self) -> &'static str {
        "fixture"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    core: [u64; 3],
    pre: Vec<String>,
}

fn parse_version(text: &str) -> Option<Version> {
    let text = text.trim().trim_start_matches('v');
    let text = text.split_once('+').map_or(text, |(version, _)| version);
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, pre.split('.').map(str::to_owned).collect()),
        None => (text, Vec::new()),
    };
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next().unwrap_or(Some(0))?;
    let patch = parts.next().unwrap_or(Some(0))?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version {
        core: [major, minor, patch],
        pre,
    })
}

fn compare_identifiers(a: &str, b: &str) -> Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Err(_), Err(_)) => a.cmp(b),
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self
                    .pre
                    .iter()
                    .zip(&other.pre)
                    .map(|(a, b)| compare_identifiers(a, b))
                    .find(|order| order.is_ne())
                    .unwrap_or_else(|| self.pre.len().cmp(&other.pre.len())),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Semver order; an unparsable version is never newer.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateCheck {
    pub current: String,
    pub latest: Option<String>,
    pub update_available: bool,
    pub url: Option<String>,
    pub notes: Option<String>,
    pub tag: Option<String>,
    pub published_at: Option<String>,
    pub assets: Vec<Asset>,
    pub repo: String,
    pub source: String,
}

fn clip_notes(notes: &str) -> String {
    let notes = notes.trim();
    match notes.char_indices().nth(NOTES_MAX_CHARS) {
        Some((end, _)) => format!("{}…", &notes[..end]),
        None => notes.to_owned(),
    }
}

pub fn evaluate(current: &str, release: Option<Release>, repo: &str, source: &str) -> UpdateCheck {
    let release = release.filter(|r| !r.draft && !r.prerelease);
    let latest = release
        .as_ref()
        .map(|r| r.tag_name.trim_start_matches('v').to_owned());
    UpdateCheck {
        current: current.to_owned(),
        update_available: latest
            .as_deref()
            .is_some_and(|latest| is_newer(latest, current)),
        latest,
        url: release.as_ref().map(|r| r.html_url.clone()),
        notes: release
            .as_ref()
            .and_then(|r| r.body.as_deref())
            .map(clip_notes)
            .filter(|notes| !notes.is_empty()),
        tag: release.as_ref().map(|r| r.tag_name.clone()),
        published_at: release.as_ref().and_then(|r| r.published_at.clone()),
        assets: release.map(|r| r.assets).unwrap_or_default(),
        repo: repo.to_owned(),
        source: source.to_owned(),
    }
}

pub fn update_notification(check: &UpdateCheck) -> Option<NewNotification> {
    if !check.update_available {
        return None;
    }
    let latest = check.latest.clone()?;
    Some(NewNotification {
        kind: UPDATE_AVAILABLE,
        subject: latest.clone(),
        title: "Update available — restart to update".to_owned(),
        body: format!(
            "Icarus {latest} is out (you have {}). Run `icarus update install`.",
            check.current
        ),
        workspace_id: None,
        data: json!({
            "current": check.current,
            "latest": latest,
            "url": check.url,
            "tag": check.tag,
        }),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    None,
}

/// `updates.channel`: `stable` (default) or `none`, which stops background checks.
pub fn channel(settings: &Value) -> Channel {
    match setting(settings, &["updates", "channel"]).and_then(Value::as_str) {
        Some("none") => Channel::None,
        _ => Channel::Stable,
    }
}

pub fn check_interval(settings: &Value) -> Duration {
    let ms = setting(settings, &["updates", "check_interval_ms"])
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_INTERVAL_MS)
        .max(MIN_INTERVAL_MS);
    Duration::from_millis(ms)
}

pub struct Updates {
    source: Arc<dyn ReleaseSource>,
    repo: String,
}

impl Updates {
    pub fn new(source: Arc<dyn ReleaseSource>, repo: impl Into<String>) -> Self {
        Self {
            source,
            repo: repo.into(),
        }
    }

    pub fn from_env() -> Self {
        let source: Arc<dyn ReleaseSource> = match std::env::var_os("ICARUS_UPDATE_FIXTURE") {
            Some(path) => Arc::new(FixtureSource(PathBuf::from(path))),
            None => Arc::new(LiveSource),
        };
        let repo = std::env::var("ICARUS_UPDATE_REPO").unwrap_or_else(|_| REPO.to_owned());
        Self::new(source, repo)
    }

    pub async fn check(&self, current: &str) -> Result<UpdateCheck, RpcError> {
        let (source, repo) = (Arc::clone(&self.source), self.repo.clone());
        let name = source.name();
        let found = tokio::time::timeout(
            CHECK_TIMEOUT,
            tokio::task::spawn_blocking(move || source.latest(&repo)),
        )
        .await
        .map_err(|_| rpc("timeout", "the release check timed out"))?
        .map_err(|e| rpc("internal", e))?;
        let release = found.map_err(|error| match error {
            SourceError::NotFound(message) => rpc("not_found", message),
            SourceError::Failed(message) => rpc("update_check_failed", message),
        })?;
        Ok(evaluate(current, release, &self.repo, name))
    }
}

pub(crate) fn spawn(server: Arc<Server>) {
    if !RELEASE_BUILD {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        loop {
            let settings = server.resolved_settings(None);
            if channel(&settings) == Channel::Stable
                && let Err(error) = server.update_check().await
            {
                tracing::debug!("update check failed: {}", error.message);
            }
            tokio::time::sleep(check_interval(&settings)).await;
        }
    });
}

impl Server {
    /// Checks for a newer release and notifies when there is one; repeats are dropped per version.
    pub(crate) async fn update_check(&self) -> Result<UpdateCheck, RpcError> {
        let check = self.updates.check(VERSION).await?;
        if let Some(new) = update_notification(&check) {
            self.notify(new);
        }
        Ok(check)
    }

    pub(crate) async fn updates_dispatch(&self, method: &str) -> Result<Value, RpcError> {
        match method {
            "update.check" => to_value(self.update_check().await?),
            other => Err(rpc("unknown_method", other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct FakeSource(Mutex<Vec<Result<Option<Release>, SourceError>>>);

    impl FakeSource {
        fn new(answers: Vec<Result<Option<Release>, SourceError>>) -> Arc<Self> {
            Arc::new(Self(Mutex::new(answers)))
        }
    }

    impl ReleaseSource for FakeSource {
        fn latest(&self, repo: &str) -> Result<Option<Release>, SourceError> {
            assert_eq!(repo, REPO);
            self.0.lock().expect("lock").remove(0)
        }

        fn name(&self) -> &'static str {
            "fake"
        }
    }

    fn release(tag: &str) -> Release {
        Release {
            tag_name: tag.into(),
            name: Some(format!("Icarus {tag}")),
            html_url: format!("https://github.com/{REPO}/releases/tag/{tag}"),
            body: Some("## What's changed\n- things".into()),
            draft: false,
            prerelease: false,
            published_at: Some("2026-11-15T10:00:00Z".into()),
            assets: vec![Asset {
                name: "SHA256SUMS".into(),
                browser_download_url: "https://example.invalid/SHA256SUMS".into(),
                size: 10,
            }],
        }
    }

    #[test]
    fn versions_compare_as_semver() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "1.0.0-rc.1"));
        assert!(is_newer("1.0.0-rc.10", "1.0.0-rc.9"));
        assert!(is_newer("1.0.0-rc.1", "1.0.0-beta"));
        assert!(is_newer("1.0.0-alpha.1", "1.0.0-alpha"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0+build.5", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("nightly", "0.1.0"));
        assert!(!is_newer("1.2.3.4", "0.1.0"));
    }

    #[test]
    fn evaluate_reports_a_newer_release_with_its_url_and_notes() {
        let check = evaluate("0.1.0", Some(release("v0.2.0")), REPO, "fake");
        assert!(check.update_available);
        assert_eq!(check.latest.as_deref(), Some("0.2.0"));
        assert_eq!(check.tag.as_deref(), Some("v0.2.0"));
        assert_eq!(
            check.url.as_deref(),
            Some("https://github.com/frbarbre/icarus/releases/tag/v0.2.0")
        );
        assert_eq!(check.notes.as_deref(), Some("## What's changed\n- things"));
        assert_eq!(check.assets.len(), 1);

        let same = evaluate("0.2.0", Some(release("v0.2.0")), REPO, "fake");
        assert!(!same.update_available);
        assert_eq!(same.latest.as_deref(), Some("0.2.0"));

        let none = evaluate("0.2.0", None, REPO, "fake");
        assert!(!none.update_available);
        assert_eq!(none.latest, None);
    }

    #[test]
    fn prereleases_and_drafts_are_ignored() {
        let mut pre = release("v0.3.0-rc.1");
        pre.prerelease = true;
        assert_eq!(evaluate("0.1.0", Some(pre), REPO, "fake").latest, None);
        let mut draft = release("v0.3.0");
        draft.draft = true;
        assert!(!evaluate("0.1.0", Some(draft), REPO, "fake").update_available);
    }

    #[test]
    fn long_notes_are_clipped() {
        let mut long = release("v0.2.0");
        long.body = Some("x".repeat(NOTES_MAX_CHARS + 5));
        let notes = evaluate("0.1.0", Some(long), REPO, "fake")
            .notes
            .expect("notes");
        assert_eq!(notes.chars().count(), NOTES_MAX_CHARS + 1);
    }

    #[test]
    fn notification_names_both_versions_once_per_version() {
        let check = evaluate("0.1.0", Some(release("v0.2.0")), REPO, "fake");
        let new = update_notification(&check).expect("notification");
        assert_eq!(new.kind, UPDATE_AVAILABLE);
        assert_eq!(new.subject, "0.2.0");
        assert_eq!(new.title, "Update available — restart to update");
        assert!(new.body.contains("0.2.0") && new.body.contains("0.1.0"));
        assert_eq!(new.data["url"], json!(check.url));
        let current = evaluate("0.2.0", Some(release("v0.2.0")), REPO, "fake");
        assert_eq!(update_notification(&current), None);
    }

    #[test]
    fn github_release_json_parses_and_null_means_none() {
        let body = json!({
            "tag_name": "v0.2.0",
            "name": "Icarus 0.2.0",
            "html_url": "https://github.com/frbarbre/icarus/releases/tag/v0.2.0",
            "body": "notes",
            "draft": false,
            "prerelease": false,
            "published_at": "2026-11-15T10:00:00Z",
            "assets": [{ "name": "SHA256SUMS", "browser_download_url": "https://x/SHA256SUMS", "size": 120, "id": 1 }],
            "author": { "login": "frbarbre" }
        });
        let parsed = parse_release(body.to_string().as_bytes())
            .expect("parse")
            .expect("release");
        assert_eq!(parsed.tag_name, "v0.2.0");
        assert_eq!(parsed.assets[0].size, 120);
        assert_eq!(parse_release(b"null"), Ok(None));
        assert!(matches!(
            parse_release(b"{\"message\":\"Not Found\"}"),
            Err(SourceError::Failed(_))
        ));
    }

    #[test]
    fn settings_default_to_stable_every_six_hours_with_a_floor() {
        assert_eq!(channel(&Value::Null), Channel::Stable);
        assert_eq!(
            channel(&json!({ "updates": { "channel": "none" } })),
            Channel::None
        );
        assert_eq!(
            check_interval(&Value::Null),
            Duration::from_millis(DEFAULT_INTERVAL_MS)
        );
        assert_eq!(
            check_interval(&json!({ "updates": { "check_interval_ms": 5 } })),
            Duration::from_millis(MIN_INTERVAL_MS)
        );
    }

    #[tokio::test]
    async fn checks_go_through_the_source_and_map_its_errors() {
        let source = FakeSource::new(vec![
            Ok(Some(release("v0.2.0"))),
            Err(SourceError::NotFound("private".into())),
            Err(SourceError::Failed("offline".into())),
        ]);
        let updates = Updates::new(source, REPO);
        let found = updates.check("0.1.0").await.expect("check");
        assert!(found.update_available);
        assert_eq!(found.source, "fake");
        assert_eq!(
            updates.check("0.1.0").await.expect_err("404").code,
            "not_found"
        );
        let failed = updates.check("0.1.0").await.expect_err("offline");
        assert_eq!(failed.code, "update_check_failed");
        assert_eq!(failed.message, "offline");
    }

    #[test]
    fn fixture_source_reads_a_file() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("release.json");
        std::fs::write(
            &path,
            serde_json::to_string(&release("v1.0.0")).expect("json"),
        )
        .expect("write");
        let found = FixtureSource(path).latest(REPO).expect("read");
        assert_eq!(found.map(|r| r.tag_name).as_deref(), Some("v1.0.0"));
    }
}
