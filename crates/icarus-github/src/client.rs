use crate::error::{GitHubError, Result};
use crate::http::HttpResponse;
use crate::rate::{RateBudget, RateLimit, retry_after};
use crate::runner::{Gh, GhRequest};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

pub const STACKS_API_VERSION: &str = "2026-03-10";
const ETAG_CAPACITY: usize = 512;

pub struct GitHub<G> {
    gh: G,
    budget: Mutex<RateBudget>,
    etags: Mutex<EtagCache>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Conditional {
    pub body: Value,
    pub changed: bool,
}

#[derive(Debug, Default)]
struct EtagCache {
    entries: BTreeMap<String, (String, Value)>,
    order: VecDeque<String>,
}

impl EtagCache {
    fn insert(&mut self, key: String, etag: String, body: Value) {
        if self.entries.insert(key.clone(), (etag, body)).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > ETAG_CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    fn remove(&mut self, key: &str) {
        if self.entries.remove(key).is_some() {
            self.order.retain(|entry| entry != key);
        }
    }
}

#[derive(Debug, Deserialize)]
struct GraphQlEnvelope {
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    errors: Option<Vec<GraphQlErrorEntry>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GraphQlErrorEntry {
    #[serde(default)]
    pub message: String,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub path: Option<Vec<Value>>,
}

impl GraphQlErrorEntry {
    pub fn alias(&self) -> Option<&str> {
        self.path.as_ref()?.first()?.as_str()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphQlResponse {
    pub data: Value,
    pub errors: Vec<GraphQlErrorEntry>,
}

impl GraphQlResponse {
    pub fn into_data<T: DeserializeOwned>(self) -> Result<T> {
        if !self.errors.is_empty() {
            return Err(graphql_error(&self.errors));
        }
        decode(self.data)
    }
}

pub(crate) fn decode<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|error| GitHubError::Malformed(error.to_string()))
}

fn graphql_error(errors: &[GraphQlErrorEntry]) -> GitHubError {
    GitHubError::GraphQl(errors.iter().map(|error| error.message.clone()).collect())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl<G: Gh> GitHub<G> {
    pub fn new(gh: G) -> Self {
        Self {
            gh,
            budget: Mutex::new(RateBudget::default()),
            etags: Mutex::new(EtagCache::default()),
        }
    }

    pub fn runner(&self) -> &G {
        &self.gh
    }

    pub fn budget(&self) -> RateBudget {
        lock(&self.budget).clone()
    }

    pub async fn send(&self, request: GhRequest) -> Result<HttpResponse> {
        let resource = if request.is_graphql() {
            "graphql"
        } else if request.endpoint.starts_with("search/") {
            "search"
        } else {
            "core"
        };
        if lock(&self.budget).exhausted_at(resource, crate::rate::now_epoch()) {
            return Err(GitHubError::RateLimited {
                resource: resource.to_owned(),
            });
        }
        let output = self.gh.execute(&request).await?;
        let Some(response) = HttpResponse::parse(&output.stdout) else {
            let stderr = output.stderr.trim();
            return Err(if output.success {
                GitHubError::Malformed("gh printed no HTTP response".to_owned())
            } else if stderr.is_empty() {
                GitHubError::Gh("gh exited without output".to_owned())
            } else {
                GitHubError::Gh(stderr.to_owned())
            });
        };
        lock(&self.budget).observe_response(&response);
        Ok(response)
    }

    pub async fn rest(&self, request: GhRequest) -> Result<HttpResponse> {
        let response = self.send(request).await?;
        if response.is_success() {
            Ok(response)
        } else {
            Err(http_error(&response))
        }
    }

    pub async fn rest_json<T: DeserializeOwned>(&self, request: GhRequest) -> Result<T> {
        self.rest(request).await?.json()
    }

    pub async fn graphql(&self, query: &str, variables: Value) -> Result<GraphQlResponse> {
        let response = self.send(GhRequest::graphql(query, variables)).await?;
        let envelope: Option<GraphQlEnvelope> = serde_json::from_slice(&response.body).ok();
        let Some(envelope) = envelope else {
            return Err(if response.is_success() {
                GitHubError::Malformed("GraphQL body is not JSON".to_owned())
            } else {
                http_error(&response)
            });
        };
        let errors = envelope.errors.unwrap_or_default();
        match envelope.data {
            Some(data) if !data.is_null() => Ok(GraphQlResponse { data, errors }),
            _ if !errors.is_empty() => Err(graphql_error(&errors)),
            _ if !response.is_success() => Err(http_error(&response)),
            _ => Err(GitHubError::Malformed(
                "GraphQL response has no data".to_owned(),
            )),
        }
    }

    pub async fn graphql_data<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: Value,
    ) -> Result<T> {
        self.graphql(query, variables).await?.into_data()
    }

    pub async fn get_conditional(&self, request: GhRequest) -> Result<Conditional> {
        let key = format!("{}\0{:?}", request.endpoint, request.headers);
        let previous = lock(&self.etags).entries.get(&key).cloned();
        let request = match &previous {
            Some((etag, _)) => request.header("If-None-Match", etag.clone()),
            None => request,
        };
        let response = match self.send(request).await {
            Ok(response) => response,
            Err(error) => {
                lock(&self.etags).remove(&key);
                return Err(error);
            }
        };
        if response.status == 304 {
            return match previous {
                Some((_, body)) => Ok(Conditional {
                    body,
                    changed: false,
                }),
                None => Err(GitHubError::Malformed(
                    "HTTP 304 without a cached representation".to_owned(),
                )),
            };
        }
        if !response.is_success() {
            lock(&self.etags).remove(&key);
            return Err(http_error(&response));
        }
        let body: Value = response.json()?;
        let mut etags = lock(&self.etags);
        match response.header("etag") {
            Some(etag) => etags.insert(key, etag.to_owned(), body.clone()),
            None => etags.remove(&key),
        }
        Ok(Conditional {
            body,
            changed: true,
        })
    }

    pub async fn rate_limits(&self) -> Result<BTreeMap<String, RateLimit>> {
        #[derive(Deserialize)]
        struct Limits {
            resources: BTreeMap<String, RateLimit>,
        }
        let limits: Limits = self.rest_json(GhRequest::get("rate_limit")).await?;
        let mut budget = lock(&self.budget);
        for (resource, limit) in &limits.resources {
            budget.observe(resource, *limit);
        }
        Ok(limits.resources)
    }
}

pub(crate) fn http_error(response: &HttpResponse) -> GitHubError {
    GitHubError::Http {
        status: response.status,
        message: response.message(),
        retry_after: retry_after(response),
    }
}
