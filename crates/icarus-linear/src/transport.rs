use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

use crate::key::ApiKey;

pub const ENDPOINT: &str = "https://api.linear.app/graphql";

#[derive(Debug, Clone, PartialEq)]
pub struct LinearRequest {
    pub query: String,
    pub variables: Value,
}

impl LinearRequest {
    pub fn new(query: impl Into<String>, variables: Value) -> Self {
        Self {
            query: query.into(),
            variables,
        }
    }

    pub fn body(&self) -> Value {
        json!({ "query": self.query, "variables": self.variables })
    }

    /// The operation name, e.g. `Issues` for `query Issues(...) { ... }`.
    pub fn operation(&self) -> Option<String> {
        let mut words = self
            .query
            .split(|c: char| c.is_whitespace() || c == '(' || c == '{');
        loop {
            match words.next()? {
                "query" | "mutation" => {
                    return words.find(|word| !word.is_empty()).map(str::to_owned);
                }
                "" => {}
                _ => return None,
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinearResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl LinearResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

pub trait Transport: Send + Sync {
    fn post(
        &self,
        key: &ApiKey,
        request: &LinearRequest,
    ) -> impl Future<Output = std::io::Result<LinearResponse>> + Send;
}

impl<T: Transport> Transport for Arc<T> {
    fn post(
        &self,
        key: &ApiKey,
        request: &LinearRequest,
    ) -> impl Future<Output = std::io::Result<LinearResponse>> + Send {
        (**self).post(key, request)
    }
}

/// Talks to Linear over HTTPS with rustls (ring) and the platform's root certificates.
#[derive(Clone)]
pub struct HttpTransport {
    endpoint: String,
    timeout: Duration,
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self {
            endpoint: ENDPOINT.to_owned(),
            timeout: Duration::from_secs(20),
        }
    }
}

const BODY_LIMIT: u64 = 8 * 1024 * 1024;

fn agent(timeout: Duration) -> std::io::Result<ureq::Agent> {
    static ROOTS: OnceLock<Result<Vec<ureq::tls::Certificate<'static>>, String>> = OnceLock::new();
    let roots = ROOTS
        .get_or_init(|| {
            let loaded = rustls_native_certs::load_native_certs();
            let certs: Vec<_> = loaded
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
        .map_err(std::io::Error::other)?;
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::from(roots))
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    Ok(ureq::Agent::config_builder()
        .tls_config(tls)
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .user_agent("icarus")
        .build()
        .new_agent())
}

impl HttpTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    fn post_blocking(
        endpoint: &str,
        timeout: Duration,
        key: &ApiKey,
        body: &Value,
    ) -> std::io::Result<LinearResponse> {
        let mut response = agent(timeout)?
            .post(endpoint)
            .header("Authorization", key.expose())
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(|e| std::io::Error::other(redact(&e.to_string(), key)))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        let body = response
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_vec()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(LinearResponse {
            status,
            headers,
            body,
        })
    }
}

fn redact(message: &str, key: &ApiKey) -> String {
    if key.expose().is_empty() {
        message.to_owned()
    } else {
        message.replace(key.expose(), "[redacted]")
    }
}

impl Transport for HttpTransport {
    async fn post(&self, key: &ApiKey, request: &LinearRequest) -> std::io::Result<LinearResponse> {
        let (endpoint, timeout, key, body) = (
            self.endpoint.clone(),
            self.timeout,
            key.clone(),
            request.body(),
        );
        tokio::task::spawn_blocking(move || Self::post_blocking(&endpoint, timeout, &key, &body))
            .await
            .map_err(std::io::Error::other)?
    }
}
