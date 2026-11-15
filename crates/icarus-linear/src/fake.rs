use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use serde_json::{Value, json};

use crate::key::ApiKey;
use crate::transport::{LinearRequest, LinearResponse, Transport};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl LinearResponse {
    pub fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: body.to_string().into_bytes(),
        }
    }

    pub fn ok(body: Value) -> Self {
        Self::json(200, body)
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// A recorded request; holds the key so tests can check the header, so never serialize it.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub key: ApiKey,
    pub request: LinearRequest,
}

/// In-memory replies queued per GraphQL operation name; the last reply repeats.
#[derive(Debug, Default)]
pub struct FakeLinear {
    replies: Mutex<BTreeMap<String, VecDeque<LinearResponse>>>,
    requests: Mutex<Vec<Recorded>>,
}

impl FakeLinear {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on(self, operation: impl Into<String>, response: LinearResponse) -> Self {
        lock(&self.replies)
            .entry(operation.into())
            .or_default()
            .push_back(response);
        self
    }

    pub fn requests(&self) -> Vec<Recorded> {
        lock(&self.requests).clone()
    }

    pub fn count(&self) -> usize {
        lock(&self.requests).len()
    }
}

impl Transport for FakeLinear {
    async fn post(&self, key: &ApiKey, request: &LinearRequest) -> std::io::Result<LinearResponse> {
        lock(&self.requests).push(Recorded {
            key: key.clone(),
            request: request.clone(),
        });
        let operation = request.operation().unwrap_or_default();
        let mut replies = lock(&self.replies);
        let Some(queue) = replies.get_mut(&operation) else {
            return Err(std::io::Error::other(format!("no fixture for {operation}")));
        };
        let reply = if queue.len() > 1 {
            queue.pop_front()
        } else {
            queue.front().cloned()
        };
        reply.ok_or_else(|| std::io::Error::other(format!("no fixture for {operation}")))
    }
}

/// Answers `Issues` queries from `<dir>/<IDENTIFIER>.json` issue nodes (Linear's GraphQL shape), re-read
/// on every call. A `<dir>/_status.json` of `{"status": 401}` (or 429, …) answers every request with that
/// status instead. Each request is appended to `<dir>/requests.jsonl` without the key.
pub struct FixtureLinear {
    dir: PathBuf,
}

impl FixtureLinear {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn log(&self, request: &LinearRequest, key: &ApiKey) {
        let line = json!({
            "operation": request.operation(),
            "variables": request.variables,
            "authorized": !key.expose().is_empty(),
        });
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("requests.jsonl"))
        {
            let _ = writeln!(log, "{line}");
        }
    }

    fn read(&self, name: &str) -> Option<Value> {
        let text = std::fs::read_to_string(self.dir.join(name)).ok()?;
        serde_json::from_str(&text).ok()
    }
}

impl Transport for FixtureLinear {
    async fn post(&self, key: &ApiKey, request: &LinearRequest) -> std::io::Result<LinearResponse> {
        self.log(request, key);
        if let Some(status) = self.read("_status.json") {
            let code = status["status"].as_u64().unwrap_or(500) as u16;
            return Ok(LinearResponse::json(
                code,
                json!({ "errors": [{ "message": format!("fixture status {code}") }] }),
            ));
        }
        if request.operation().as_deref() != Some("Issues") {
            return Ok(LinearResponse::json(
                400,
                json!({ "errors": [{ "message": "the fixture only answers Issues" }] }),
            ));
        }
        let mut data = serde_json::Map::new();
        let mut errors = Vec::new();
        for (alias, identifier) in request.variables.as_object().into_iter().flatten() {
            let identifier = identifier.as_str().unwrap_or_default();
            match self.read(&format!("{identifier}.json")) {
                Some(node) => {
                    data.insert(alias.clone(), node);
                }
                None => {
                    data.insert(alias.clone(), Value::Null);
                    errors.push(json!({
                        "message": "Entity not found: Issue",
                        "path": [alias],
                        "extensions": { "code": "INPUT_ERROR" },
                    }));
                }
            }
        }
        // Like Linear, whose `issue` is non-null: one missing issue nulls all of `data`.
        Ok(LinearResponse::ok(if errors.is_empty() {
            json!({ "data": data })
        } else {
            json!({ "data": null, "errors": errors })
        }))
    }
}
