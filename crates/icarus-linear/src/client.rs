use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::error::{LinearError, Result};
use crate::issue::{
    BATCH_MAX, Issue, batch_query, my_issues_query, normalize_identifier, parse_issue, search_query,
};
use crate::key::KeyResolver;
use crate::transport::{LinearRequest, LinearResponse, Transport};

const DEFAULT_RATE_WAIT: Duration = Duration::from_secs(60);
const MAX_RATE_WAIT: Duration = Duration::from_secs(3600);

pub struct Linear<T> {
    transport: T,
    keys: KeyResolver,
    blocked_until: Mutex<Option<Instant>>,
}

fn messages(body: &Value) -> Vec<String> {
    body["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|error| error["message"].as_str().map(str::to_owned))
        .collect()
}

/// Batch positions (`iN` aliases) that a GraphQL error points at.
fn failed_aliases(body: &Value) -> Vec<usize> {
    body["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|error| error["path"][0].as_str()?.strip_prefix('i')?.parse().ok())
        .collect()
}

fn has_code(body: &Value, codes: &[&str]) -> bool {
    body["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|error| {
            let code = error["extensions"]["code"].as_str().unwrap_or_default();
            codes.iter().any(|c| code.eq_ignore_ascii_case(c))
        })
}

fn retry_after(response: &LinearResponse) -> Duration {
    if let Some(seconds) = response
        .header("retry-after")
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return Duration::from_secs(seconds).min(MAX_RATE_WAIT);
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    response
        .header("x-ratelimit-requests-reset")
        .or_else(|| response.header("x-ratelimit-complexity-reset"))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|reset_ms| Duration::from_millis(reset_ms.saturating_sub(now_ms)))
        .unwrap_or(DEFAULT_RATE_WAIT)
        .clamp(Duration::from_secs(1), MAX_RATE_WAIT)
}

impl<T: Transport> Linear<T> {
    pub fn new(transport: T, keys: KeyResolver) -> Self {
        Self {
            transport,
            keys,
            blocked_until: Mutex::new(None),
        }
    }

    pub fn keys(&self) -> &KeyResolver {
        &self.keys
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    fn blocked(&self) -> Option<Duration> {
        let mut blocked = self
            .blocked_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match *blocked {
            Some(until) if until > Instant::now() => Some(until - Instant::now()),
            Some(_) => {
                *blocked = None;
                None
            }
            None => None,
        }
    }

    fn block_for(&self, wait: Duration) {
        *self
            .blocked_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now() + wait);
    }

    /// Runs a read-only query and returns its `data`, or `{data, errors}` when `partial` allows errors next to data.
    async fn execute(&self, request: &LinearRequest, partial: bool) -> Result<Value> {
        if !request.query.trim_start().starts_with("query") {
            return Err(LinearError::Malformed(
                "only queries are sent to Linear".into(),
            ));
        }
        let mut retried = false;
        loop {
            if let Some(wait) = self.blocked() {
                return Err(LinearError::RateLimited {
                    retry_after: Some(wait),
                });
            }
            let (key, source) = self.keys.key().await?;
            let response = self.transport.post(&key, request).await?;
            let body: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
            let unauthorized = response.status == 401
                || has_code(&body, &["AUTHENTICATION_ERROR", "UNAUTHENTICATED"]);
            if unauthorized {
                if self.keys.rejected(source, retried).await {
                    retried = true;
                    continue;
                }
                return Err(LinearError::Unauthorized);
            }
            if response.status == 429 || has_code(&body, &["RATELIMITED"]) {
                let wait = retry_after(&response);
                self.block_for(wait);
                return Err(LinearError::RateLimited {
                    retry_after: Some(wait),
                });
            }
            if response.status >= 400 {
                let mut message = messages(&body).join("; ");
                if message.is_empty() {
                    message = String::from_utf8_lossy(&response.body)
                        .chars()
                        .take(200)
                        .collect();
                }
                return Err(LinearError::Http {
                    status: response.status,
                    message,
                });
            }
            let data = &body["data"];
            if partial && !data.is_object() && !failed_aliases(&body).is_empty() {
                return Ok(body);
            }
            if !data.is_object() {
                let errors = messages(&body);
                return Err(if errors.is_empty() {
                    LinearError::Malformed("no data".into())
                } else {
                    LinearError::GraphQl(errors)
                });
            }
            if !partial && body["errors"].is_array() {
                return Err(LinearError::GraphQl(messages(&body)));
            }
            return Ok(if partial { body } else { data.clone() });
        }
    }

    /// A read-only GraphQL query (e.g. `viewer`); mutations are refused.
    pub async fn query(&self, query: &str, variables: Value) -> Result<Value> {
        self.execute(&LinearRequest::new(query, variables), false)
            .await
    }

    pub async fn issue(&self, identifier: &str) -> Result<Issue> {
        let identifier = normalize_identifier(identifier)?;
        self.issues(std::slice::from_ref(&identifier))
            .await?
            .remove(&identifier)
            .flatten()
            .ok_or(LinearError::NotFound(identifier))
    }

    pub async fn search(&self, term: &str, limit: usize) -> Result<Vec<Issue>> {
        let term = term.trim();
        // Why: `searchIssues` is full-text; an identifier deserves an exact lookup.
        if let Ok(identifier) = normalize_identifier(term) {
            let found = self.issues(std::slice::from_ref(&identifier)).await?;
            return Ok(found.into_values().flatten().collect());
        }
        let ((query, variables), field) = if term.is_empty() {
            (my_issues_query(limit), "issues")
        } else {
            (search_query(term, limit), "searchIssues")
        };
        let data = self
            .execute(&LinearRequest::new(query, variables), false)
            .await?;
        data[field]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|node| parse_issue(node.clone()))
            .collect()
    }

    /// Several issues in as few queries as possible; `None` marks an identifier Linear doesn't know.
    pub async fn issues(&self, identifiers: &[String]) -> Result<BTreeMap<String, Option<Issue>>> {
        let mut wanted: Vec<String> = identifiers
            .iter()
            .map(|id| normalize_identifier(id))
            .collect::<Result<_>>()?;
        wanted.sort();
        wanted.dedup();
        let mut found = BTreeMap::new();
        for chunk in wanted.chunks(BATCH_MAX) {
            let mut pending: Vec<String> = chunk.to_vec();
            while !pending.is_empty() {
                let (query, variables) = batch_query(&pending);
                let body = self
                    .execute(&LinearRequest::new(query, variables), true)
                    .await?;
                if !body["data"].is_object() {
                    // `issue` is non-null in Linear's schema, so one unknown identifier nulls the whole
                    // response: drop the failed ones and ask again for the rest.
                    let failed = failed_aliases(&body);
                    let before = pending.len();
                    let mut index = 0;
                    pending.retain(|identifier| {
                        let keep = !failed.contains(&index);
                        if !keep {
                            found.insert(identifier.clone(), None);
                        }
                        index += 1;
                        keep
                    });
                    if pending.len() == before {
                        return Err(LinearError::GraphQl(messages(&body)));
                    }
                    continue;
                }
                for (index, identifier) in pending.iter().enumerate() {
                    let node = body["data"][format!("i{index}")].clone();
                    let issue = if node.is_null() {
                        None
                    } else {
                        Some(parse_issue(node)?)
                    };
                    found.insert(identifier.clone(), issue);
                }
                pending.clear();
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeLinear;
    use crate::key::KeyCommand;
    use serde_json::json;
    use std::sync::Arc;

    fn node(identifier: &str, title: &str) -> Value {
        json!({
            "id": format!("uuid-{identifier}"),
            "identifier": identifier,
            "title": title,
            "description": "Steps to reproduce",
            "priority": 2,
            "priorityLabel": "High",
            "url": format!("https://linear.app/acme/issue/{}", identifier.to_lowercase()),
            "branchName": format!("frederik/{}-fix-login", identifier.to_lowercase()),
            "updatedAt": "2026-11-14T10:00:00.000Z",
            "state": { "name": "In Progress", "type": "started", "color": "#f2c94c" },
            "assignee": { "name": "Frederik B", "displayName": "frederik", "avatarUrl": "https://a/f.png" },
            "labels": { "nodes": [{ "name": "Bug", "color": "#eb5757" }] },
            "team": { "key": identifier.split('-').next() },
        })
    }

    fn client(fake: FakeLinear) -> Linear<Arc<FakeLinear>> {
        Linear::new(Arc::new(fake), KeyResolver::new(Some("lin_test".into())))
    }

    #[tokio::test]
    async fn get_issue_maps_every_field_and_sends_the_key_as_the_authorization_header() {
        let linear = client(FakeLinear::new().on(
            "Issues",
            LinearResponse::ok(json!({ "data": { "i0": node("ABC-12", "Fix login") } })),
        ));
        let issue = linear.issue("abc-12").await.expect("issue");
        assert_eq!(issue.identifier, "ABC-12");
        assert_eq!(issue.title, "Fix login");
        assert_eq!(issue.description.as_deref(), Some("Steps to reproduce"));
        let state = issue.state.as_ref().expect("state");
        assert_eq!(
            (state.name.as_str(), state.kind.as_str()),
            ("In Progress", "started")
        );
        assert_eq!(issue.priority, Some(2));
        let assignee = issue.assignee.as_ref().expect("assignee");
        assert_eq!(assignee.name, "frederik");
        assert_eq!(assignee.avatar_url.as_deref(), Some("https://a/f.png"));
        assert_eq!(issue.labels[0].name, "Bug");
        assert_eq!(
            issue.branch_name.as_deref(),
            Some("frederik/abc-12-fix-login")
        );
        assert_eq!(issue.team_key.as_deref(), Some("ABC"));
        assert_eq!(issue.url, "https://linear.app/acme/issue/abc-12");

        let requests = linear.transport().requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].key.expose(), "lin_test");
        assert_eq!(requests[0].request.operation().as_deref(), Some("Issues"));
        assert_eq!(requests[0].request.variables, json!({ "i0": "ABC-12" }));
    }

    #[tokio::test]
    async fn search_uses_linear_search_for_text_a_lookup_for_identifiers_and_my_issues_when_empty()
    {
        let linear = client(
            FakeLinear::new()
                .on(
                    "SearchIssues",
                    LinearResponse::ok(json!({ "data": { "searchIssues": { "nodes": [
                        node("ABC-12", "Fix login"), node("ABC-14", "Login copy"),
                    ] } } })),
                )
                .on(
                    "Issues",
                    LinearResponse::ok(json!({ "data": { "i0": node("ABC-12", "Fix login") } })),
                )
                .on(
                    "MyIssues",
                    LinearResponse::ok(
                        json!({ "data": { "issues": { "nodes": [node("ABC-7", "Mine")] } } }),
                    ),
                ),
        );
        let found = linear.search(" login ", 500).await.expect("search");
        assert_eq!(
            found
                .iter()
                .map(|i| i.identifier.as_str())
                .collect::<Vec<_>>(),
            ["ABC-12", "ABC-14"]
        );
        let requests = linear.transport().requests();
        assert_eq!(
            requests[0].request.variables,
            json!({ "term": "login", "first": 50 }),
            "trimmed, and the limit is capped"
        );

        let found = linear.search("abc-12", 10).await.expect("lookup");
        assert_eq!(
            found[0].branch_name.as_deref(),
            Some("frederik/abc-12-fix-login")
        );
        assert_eq!(
            linear.transport().requests()[1]
                .request
                .operation()
                .as_deref(),
            Some("Issues")
        );

        let found = linear.search("", 10).await.expect("mine");
        assert_eq!(found[0].identifier, "ABC-7");
    }

    #[tokio::test]
    async fn a_batch_is_one_query_and_unknown_identifiers_come_back_as_none() {
        let linear = client(FakeLinear::new().on(
            "Issues",
            LinearResponse::ok(json!({
                "data": { "i0": node("ABC-1", "One"), "i1": null, "i2": node("XYZ-3", "Three") },
                "errors": [{ "message": "Entity not found: Issue", "path": ["i1"] }],
            })),
        ));
        let found = linear
            .issues(&[
                "xyz-3".into(),
                "ABC-1".into(),
                "ABC-2".into(),
                "abc-1".into(),
            ])
            .await
            .expect("batch");
        assert_eq!(linear.transport().count(), 1);
        let request = &linear.transport().requests()[0].request;
        assert_eq!(
            request.variables,
            json!({ "i0": "ABC-1", "i1": "ABC-2", "i2": "XYZ-3" }),
            "deduplicated, normalized and sorted"
        );
        assert_eq!(
            found["ABC-1"].as_ref().map(|i| i.title.as_str()),
            Some("One")
        );
        assert_eq!(found["ABC-2"], None);
        assert_eq!(
            found["XYZ-3"].as_ref().map(|i| i.title.as_str()),
            Some("Three")
        );
    }

    #[tokio::test]
    async fn an_unknown_identifier_that_nulls_the_whole_response_is_dropped_and_the_rest_asked_again()
     {
        let fake = FakeLinear::new()
            .on(
                "Issues",
                LinearResponse::ok(json!({
                    "data": null,
                    "errors": [{ "message": "Entity not found: Issue", "path": ["i1"] }],
                })),
            )
            .on(
                "Issues",
                LinearResponse::ok(json!({
                    "data": { "i0": node("ABC-1", "One"), "i1": node("ABC-3", "Three") },
                })),
            );
        let linear = client(fake);
        let found = linear
            .issues(&["ABC-1".into(), "ABC-2".into(), "ABC-3".into()])
            .await
            .expect("batch");
        assert_eq!(found["ABC-2"], None);
        assert_eq!(
            found["ABC-1"].as_ref().map(|i| i.title.as_str()),
            Some("One")
        );
        assert_eq!(
            found["ABC-3"].as_ref().map(|i| i.title.as_str()),
            Some("Three")
        );
        let requests = linear.transport().requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].request.variables,
            json!({ "i0": "ABC-1", "i1": "ABC-3" })
        );

        let linear = client(FakeLinear::new().on(
            "Issues",
            LinearResponse::ok(json!({ "data": null, "errors": [{ "message": "Entity not found", "path": ["i0"] }] })),
        ));
        assert!(matches!(
            linear.issue("ABC-9").await,
            Err(LinearError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn large_batches_are_split_into_chunks() {
        let fake = FakeLinear::new().on("Issues", LinearResponse::ok(json!({ "data": {} })));
        let linear = client(fake);
        let ids: Vec<String> = (1..=BATCH_MAX + 1).map(|n| format!("ABC-{n}")).collect();
        let found = linear.issues(&ids).await.expect("batch");
        assert_eq!(found.len(), BATCH_MAX + 1);
        assert_eq!(linear.transport().count(), 2);
    }

    #[tokio::test]
    async fn a_missing_issue_is_not_found() {
        let linear = client(FakeLinear::new().on(
            "Issues",
            LinearResponse::ok(json!({ "data": { "i0": null }, "errors": [{ "message": "Entity not found", "path": ["i0"] }] })),
        ));
        assert!(
            matches!(linear.issue("ABC-9").await, Err(LinearError::NotFound(id)) if id == "ABC-9")
        );
    }

    #[tokio::test]
    async fn invalid_identifiers_never_reach_the_network() {
        let linear = client(FakeLinear::new());
        assert!(matches!(
            linear.issue("nope").await,
            Err(LinearError::InvalidIdentifier(_))
        ));
        assert_eq!(linear.transport().count(), 0);
    }

    #[tokio::test]
    async fn a_401_with_the_env_key_is_unauthorized_and_stays_so() {
        let linear = client(FakeLinear::new().on(
            "Issues",
            LinearResponse::json(
                401,
                json!({ "errors": [{ "message": "Authentication required" }] }),
            ),
        ));
        assert!(matches!(
            linear.issue("ABC-1").await,
            Err(LinearError::Unauthorized)
        ));
        assert!(matches!(
            linear.issue("ABC-1").await,
            Err(LinearError::Unauthorized)
        ));
        assert_eq!(
            linear.transport().count(),
            1,
            "a rejected env key is not retried"
        );
    }

    #[tokio::test]
    async fn a_401_reruns_the_key_command_once_and_retries() {
        let dir = tempfile::tempdir().expect("dir");
        let counter = dir.path().join("n");
        let script = dir.path().join("key.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho x >> '{c}'\necho lin_key_$(wc -l < '{c}' | tr -d ' ')\n",
                c = counter.display()
            ),
        )
        .expect("script");
        let fake = FakeLinear::new()
            .on(
                "Issues",
                LinearResponse::json(401, json!({ "errors": [{ "message": "expired" }] })),
            )
            .on(
                "Issues",
                LinearResponse::ok(json!({ "data": { "i0": node("ABC-1", "One") } })),
            );
        let linear = Linear::new(Arc::new(fake), KeyResolver::new(None));
        linear
            .keys()
            .set_command(Some(KeyCommand::Argv(vec![
                "/bin/sh".into(),
                script.to_string_lossy().into_owned(),
            ])))
            .await;
        let issue = linear.issue("ABC-1").await.expect("retried");
        assert_eq!(issue.title, "One");
        let keys: Vec<String> = linear
            .transport()
            .requests()
            .iter()
            .map(|r| r.key.expose().to_owned())
            .collect();
        assert_eq!(keys, ["lin_key_1", "lin_key_2"]);
    }

    #[tokio::test]
    async fn a_command_key_rejected_twice_is_unauthorized_without_rerunning_each_call() {
        let fake = FakeLinear::new().on(
            "Issues",
            LinearResponse::json(400, json!({ "errors": [{ "message": "bad key", "extensions": { "code": "AUTHENTICATION_ERROR" } }] })),
        );
        let linear = Linear::new(Arc::new(fake), KeyResolver::new(None));
        linear
            .keys()
            .set_command(Some(KeyCommand::Shell("echo lin_bad".into())))
            .await;
        assert!(matches!(
            linear.issue("ABC-1").await,
            Err(LinearError::Unauthorized)
        ));
        assert_eq!(linear.transport().count(), 2);
        assert!(matches!(
            linear.issue("ABC-1").await,
            Err(LinearError::Unauthorized)
        ));
        assert_eq!(
            linear.transport().count(),
            2,
            "backs off instead of hammering Linear"
        );
        assert_eq!(linear.keys().status().await, crate::KeyStatus::Rejected);
    }

    #[tokio::test]
    async fn rate_limits_block_further_requests_until_the_reset() {
        let linear = client(FakeLinear::new().on(
            "Issues",
            LinearResponse::json(400, json!({ "errors": [{ "message": "Rate limit exceeded", "extensions": { "code": "RATELIMITED" } }] }))
                .with_header("Retry-After", "120"),
        ));
        let error = linear.issue("ABC-1").await.expect_err("limited");
        assert!(
            matches!(error, LinearError::RateLimited { retry_after: Some(wait) } if wait == Duration::from_secs(120))
        );
        assert!(matches!(
            linear.issue("ABC-1").await,
            Err(LinearError::RateLimited { .. })
        ));
        assert_eq!(
            linear.transport().count(),
            1,
            "no request while rate limited"
        );
    }

    #[tokio::test]
    async fn a_429_uses_linears_reset_header() {
        let reset = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_millis() as u64
            + 30_000;
        let linear = client(
            FakeLinear::new().on(
                "Issues",
                LinearResponse::json(429, json!({}))
                    .with_header("X-RateLimit-Requests-Reset", reset.to_string()),
            ),
        );
        match linear.issue("ABC-1").await {
            Err(LinearError::RateLimited {
                retry_after: Some(wait),
            }) => {
                assert!(
                    wait > Duration::from_secs(25) && wait <= Duration::from_secs(30),
                    "{wait:?}"
                );
            }
            other => panic!("expected a rate limit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_and_graphql_errors_are_reported() {
        let linear = client(FakeLinear::new().on(
            "Issues",
            LinearResponse::json(500, json!({ "errors": [{ "message": "boom" }] })),
        ));
        let error = linear.issue("ABC-1").await.expect_err("fails");
        assert!(matches!(&error, LinearError::Http { status: 500, message } if message == "boom"));
        assert_eq!(error.code(), "linear_failed");

        let linear = client(FakeLinear::new().on(
            "Viewer",
            LinearResponse::ok(json!({ "data": null, "errors": [{ "message": "bad field" }] })),
        ));
        let error = linear
            .query("query Viewer { viewer { nope } }", json!({}))
            .await
            .expect_err("fails");
        assert!(matches!(error, LinearError::GraphQl(m) if m == ["bad field"]));
    }

    #[tokio::test]
    async fn without_a_key_nothing_is_sent() {
        let linear = Linear::new(Arc::new(FakeLinear::new()), KeyResolver::new(None));
        assert!(matches!(
            linear.issue("ABC-1").await,
            Err(LinearError::NotConfigured)
        ));
        assert_eq!(linear.transport().count(), 0);
    }

    #[tokio::test]
    async fn mutations_are_refused() {
        let linear = client(FakeLinear::new());
        let result = linear
            .query(
                "mutation Delete { issueDelete(id: \"x\") { success } }",
                json!({}),
            )
            .await;
        assert!(matches!(result, Err(LinearError::Malformed(_))));
        assert_eq!(linear.transport().count(), 0);
    }

    #[tokio::test]
    async fn the_fixture_dir_answers_batches_and_logs_without_the_key() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(
            dir.path().join("ABC-12.json"),
            node("ABC-12", "From fixture").to_string(),
        )
        .expect("fixture");
        let linear = Linear::new(
            crate::FixtureLinear::new(dir.path()),
            KeyResolver::new(Some("lin_fixture_secret".into())),
        );
        let found = linear
            .issues(&["ABC-12".into(), "ABC-13".into()])
            .await
            .expect("batch");
        assert_eq!(
            found["ABC-12"].as_ref().map(|i| i.title.as_str()),
            Some("From fixture")
        );
        assert_eq!(found["ABC-13"], None);
        let log = std::fs::read_to_string(dir.path().join("requests.jsonl")).expect("log");
        assert!(log.contains("\"Issues\"") && log.contains("\"authorized\":true"));
        assert!(!log.contains("secret"));

        std::fs::write(dir.path().join("_status.json"), r#"{"status": 401}"#).expect("status");
        assert!(matches!(
            linear.issue("ABC-12").await,
            Err(LinearError::Unauthorized)
        ));
    }
}
