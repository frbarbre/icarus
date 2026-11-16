//! Linear, kept minimal (plan D12): issue titles and details for cards, issue lookups, and naming workspaces made from an issue.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use icarus_linear::{
    ApiKey, FixtureLinear, HttpTransport, Issue, KeyCommand, KeyResolver, Linear, LinearError,
    LinearRequest, LinearResponse, Transport, normalize_identifier,
};
use icarus_protocol::RpcError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::{Server, params, rpc, to_value};

const ISSUE_TTL: Duration = Duration::from_secs(60);
const CREATE_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_POLL_MS: u64 = 300_000;
const MIN_POLL_MS: u64 = 1_000;
const SEARCH_LIMIT: usize = 20;

/// The transport the server uses: Linear's API, or a fixture directory when `ICARUS_LINEAR_FIXTURES` is set.
pub enum ServerLinear {
    Http(HttpTransport),
    Fixture(FixtureLinear),
}

impl Transport for ServerLinear {
    async fn post(&self, key: &ApiKey, request: &LinearRequest) -> std::io::Result<LinearResponse> {
        match self {
            Self::Http(http) => http.post(key, request).await,
            Self::Fixture(fixture) => fixture.post(key, request).await,
        }
    }
}

struct Cached {
    fetched: Instant,
    issue: Option<Issue>,
}

pub struct LinearService {
    client: Linear<ServerLinear>,
    cache: Mutex<HashMap<String, Cached>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl LinearService {
    pub fn from_env() -> Self {
        let transport = match std::env::var_os("ICARUS_LINEAR_FIXTURES") {
            Some(dir) => ServerLinear::Fixture(FixtureLinear::new(PathBuf::from(dir))),
            None => ServerLinear::Http(HttpTransport::new()),
        };
        Self {
            client: Linear::new(transport, KeyResolver::from_env()),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn cached(&self, identifier: &str) -> Option<Issue> {
        lock(&self.cache)
            .get(identifier)
            .and_then(|cached| cached.issue.clone())
    }

    fn fresh(&self, identifier: &str) -> Option<Issue> {
        lock(&self.cache)
            .get(identifier)
            .filter(|cached| cached.fetched.elapsed() < ISSUE_TTL)
            .and_then(|cached| cached.issue.clone())
    }

    /// Fetches and caches the issues; returns the identifiers whose card fields changed.
    async fn fetch(
        &self,
        identifiers: &[String],
    ) -> Result<(BTreeMap<String, Option<Issue>>, Vec<String>), LinearError> {
        let found = self.client.issues(identifiers).await?;
        let mut cache = lock(&self.cache);
        let mut changed = Vec::new();
        for (identifier, issue) in &found {
            let before = cache
                .get(identifier)
                .and_then(|c| c.issue.as_ref().map(Issue::card_fingerprint));
            let after = issue.as_ref().map(Issue::card_fingerprint);
            if before != after {
                changed.push(identifier.clone());
            }
            cache.insert(
                identifier.clone(),
                Cached {
                    fetched: Instant::now(),
                    issue: issue.clone(),
                },
            );
        }
        Ok((found, changed))
    }
}

/// The normalized identifier of a stored `{provider: "linear", id}` issue link.
pub(crate) fn linear_issue_id(issue: &Value) -> Option<String> {
    let provider = issue["provider"].as_str()?;
    if !provider.eq_ignore_ascii_case("linear") {
        return None;
    }
    normalize_identifier(issue["id"].as_str()?).ok()
}

fn poll_interval(settings: &Value) -> Duration {
    let ms = settings["linear"]["poll_interval_ms"]
        .as_u64()
        .unwrap_or(DEFAULT_POLL_MS)
        .max(MIN_POLL_MS);
    Duration::from_millis(ms)
}

fn linear_rpc(error: LinearError) -> RpcError {
    rpc(error.code(), error)
}

pub(crate) fn spawn(server: Arc<Server>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        loop {
            if server.client_count() > 0 {
                match server.linear_poll().await {
                    Ok(_) | Err(LinearError::NotConfigured) => {}
                    Err(error) => tracing::debug!("linear poll failed: {error}"),
                }
            }
            let interval = poll_interval(&server.resolved_settings(None));
            tokio::time::sleep(interval).await;
        }
    });
}

impl Server {
    /// Follows `linear.api_key_command` from the user layer only, never from a repo's `icarus.json`.
    async fn linear_sync_settings(&self) {
        let settings = self.resolved_settings(None);
        let command = KeyCommand::from_setting(&settings["linear"]["api_key_command"]);
        self.linear.client.keys().set_command(command).await;
    }

    /// Adds the cached Linear details to a workspace's `issue`; never touches the network.
    pub(crate) fn enrich_issue(&self, issue: &mut Value) {
        let Some(identifier) = linear_issue_id(issue) else {
            return;
        };
        let Some(found) = self.linear.cached(&identifier) else {
            return;
        };
        let Ok(Value::Object(details)) = serde_json::to_value(&found) else {
            return;
        };
        for (key, value) in details {
            // Linear's own `id` is a UUID; the stored `id` stays the identifier.
            if key != "id" {
                issue[key] = value;
            }
        }
    }

    /// The issue behind a new workspace, if Linear answers quickly; failures stay quiet.
    pub(crate) async fn linear_issue_for_create(&self, identifier: &str) -> Option<Issue> {
        if let Some(issue) = self.linear.fresh(identifier) {
            return Some(issue);
        }
        self.linear_sync_settings().await;
        let ids = [identifier.to_owned()];
        match tokio::time::timeout(CREATE_TIMEOUT, self.linear.fetch(&ids)).await {
            Ok(Ok((mut found, _))) => found.remove(identifier).flatten(),
            Ok(Err(LinearError::NotConfigured)) => None,
            Ok(Err(error)) => {
                tracing::debug!("linear lookup for {identifier} failed: {error}");
                None
            }
            Err(_) => {
                tracing::debug!("linear lookup for {identifier} timed out");
                None
            }
        }
    }

    /// Refreshes every linked issue; tells clients which workspaces' cards changed.
    pub(crate) async fn linear_poll(&self) -> Result<Value, LinearError> {
        self.linear_sync_settings().await;
        let workspaces = self
            .store
            .workspaces(None)
            .map_err(|e| LinearError::Malformed(e.to_string()))?;
        let linked: Vec<(String, String)> = workspaces
            .iter()
            .filter_map(|w| Some((w.id.clone(), linear_issue_id(w.issue.as_ref()?)?)))
            .collect();
        if linked.is_empty() {
            return Ok(json!({ "refreshed": 0, "changed": [] }));
        }
        let ids: Vec<String> = linked.iter().map(|(_, id)| id.clone()).collect();
        let (found, changed) = self.linear.fetch(&ids).await?;
        let changed_workspaces: Vec<&String> = linked
            .iter()
            .filter(|(_, id)| changed.contains(id))
            .map(|(workspace, _)| workspace)
            .collect();
        if !changed_workspaces.is_empty() {
            self.broadcast(
                "workspace.changed",
                json!({ "workspaces": changed_workspaces, "source": "linear" }),
            );
        }
        Ok(json!({ "refreshed": found.len(), "changed": changed_workspaces }))
    }

    pub(crate) async fn linear_dispatch(&self, method: &str, p: Value) -> Result<Value, RpcError> {
        if method == "issue.search" {
            return self.issue_search(p).await;
        }
        #[derive(Deserialize)]
        struct P {
            id: Option<String>,
            #[serde(default)]
            refresh: bool,
        }
        let p: P = params(p)?;
        let identifier =
            p.id.as_deref()
                .map(normalize_identifier)
                .transpose()
                .map_err(|e| rpc("invalid_params", e))?;
        match (method, identifier) {
            ("issue.get", Some(identifier)) => self.issue_get(&identifier, p.refresh).await,
            ("issue.get", None) => Err(rpc("invalid_params", "id is required")),
            ("issue.refresh", Some(identifier)) => {
                self.linear.client.keys().reset_backoff().await;
                self.issue_get(&identifier, true).await
            }
            ("issue.refresh", None) => {
                self.linear.client.keys().reset_backoff().await;
                self.linear_poll().await.map_err(linear_rpc)
            }
            ("issue.status", _) => {
                self.linear_sync_settings().await;
                let status = self.linear.client.keys().status().await;
                Ok(json!({
                    "configured": status.configured(),
                    "key": format!("{status:?}"),
                    "cached": lock(&self.linear.cache).len(),
                }))
            }
            (other, _) => Err(rpc("unknown_method", other)),
        }
    }

    async fn issue_search(&self, p: Value) -> Result<Value, RpcError> {
        #[derive(Deserialize)]
        struct P {
            #[serde(default)]
            query: String,
            limit: Option<usize>,
        }
        let p: P = params(p)?;
        self.linear_sync_settings().await;
        let limit = p.limit.unwrap_or(SEARCH_LIMIT);
        let found = self
            .linear
            .client
            .search(&p.query, limit)
            .await
            .map_err(linear_rpc)?;
        // Why: creating a workspace from a picked issue then needs no second Linear round trip.
        let mut cache = lock(&self.linear.cache);
        for issue in &found {
            cache.insert(
                issue.identifier.clone(),
                Cached {
                    fetched: Instant::now(),
                    issue: Some(issue.clone()),
                },
            );
        }
        to_value(&found)
    }

    async fn issue_get(&self, identifier: &str, refresh: bool) -> Result<Value, RpcError> {
        if !refresh && let Some(issue) = self.linear.fresh(identifier) {
            return issue_json(&issue, false);
        }
        self.linear_sync_settings().await;
        let ids = [identifier.to_owned()];
        match self.linear.fetch(&ids).await {
            Ok((mut found, _)) => match found.remove(identifier).flatten() {
                Some(issue) => issue_json(&issue, false),
                None => Err(linear_rpc(LinearError::NotFound(identifier.to_owned()))),
            },
            Err(error) => match self.linear.cached(identifier) {
                Some(issue) => {
                    let mut value = issue_json(&issue, true)?;
                    value["error"] = json!(error.to_string());
                    Ok(value)
                }
                None => Err(linear_rpc(error)),
            },
        }
    }
}

fn issue_json(issue: &Issue, stale: bool) -> Result<Value, RpcError> {
    let mut value = to_value(issue)?;
    value["stale"] = json!(stale);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_linear_links_with_valid_identifiers_count() {
        assert_eq!(
            linear_issue_id(&json!({ "provider": "linear", "id": "abc-12" })).as_deref(),
            Some("ABC-12")
        );
        assert_eq!(
            linear_issue_id(&json!({ "provider": "Linear", "id": "ABC-1" })).as_deref(),
            Some("ABC-1")
        );
        assert_eq!(
            linear_issue_id(&json!({ "provider": "jira", "id": "ABC-1" })),
            None
        );
        assert_eq!(
            linear_issue_id(&json!({ "provider": "linear", "id": "nope" })),
            None
        );
        assert_eq!(linear_issue_id(&json!(null)), None);
    }

    #[test]
    fn the_poll_interval_has_a_floor() {
        assert_eq!(
            poll_interval(&json!(null)),
            Duration::from_millis(DEFAULT_POLL_MS)
        );
        assert_eq!(
            poll_interval(&json!({ "linear": { "poll_interval_ms": 5 } })),
            Duration::from_millis(MIN_POLL_MS)
        );
        assert_eq!(
            poll_interval(&json!({ "linear": { "poll_interval_ms": 120000 } })),
            Duration::from_secs(120)
        );
    }
}
