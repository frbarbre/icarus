use serde_json::Value;
use std::ffi::OsString;
use std::future::Future;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Json(Value),
    Raw {
        bytes: Vec<u8>,
        content_type: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct GhRequest {
    pub method: Method,
    pub endpoint: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Body>,
}

impl GhRequest {
    pub fn new(method: Method, endpoint: impl Into<String>) -> Self {
        Self {
            method,
            endpoint: endpoint.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn get(endpoint: impl Into<String>) -> Self {
        Self::new(Method::Get, endpoint)
    }

    pub fn graphql(query: &str, variables: Value) -> Self {
        Self::new(Method::Post, "graphql").json(serde_json::json!({
            "query": query,
            "variables": variables,
        }))
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn json(mut self, body: Value) -> Self {
        self.body = Some(Body::Json(body));
        self
    }

    pub fn raw(mut self, bytes: Vec<u8>, content_type: impl Into<String>) -> Self {
        self.body = Some(Body::Raw {
            bytes,
            content_type: content_type.into(),
        });
        self
    }

    pub fn is_graphql(&self) -> bool {
        self.endpoint == "graphql"
    }

    pub fn json_body(&self) -> Option<&Value> {
        match &self.body {
            Some(Body::Json(value)) => Some(value),
            _ => None,
        }
    }

    pub fn graphql_operation(&self) -> Option<String> {
        let query = self.json_body()?.get("query")?.as_str()?;
        let mut words = query.split(|c: char| c.is_whitespace() || c == '(' || c == '{');
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

    pub fn graphql_variables(&self) -> Option<&Value> {
        self.json_body()?.get("variables")
    }

    pub fn route(&self) -> String {
        if self.is_graphql() {
            let operation = self
                .graphql_operation()
                .unwrap_or_else(|| "anonymous".to_owned());
            return format!("graphql {operation}");
        }
        format!("{} {}", self.method.as_str(), self.endpoint)
    }

    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            "api".to_owned(),
            "--include".to_owned(),
            "--method".to_owned(),
            self.method.as_str().to_owned(),
            self.endpoint.clone(),
        ];
        for (name, value) in &self.headers {
            args.push("--header".to_owned());
            args.push(format!("{name}: {value}"));
        }
        match &self.body {
            Some(Body::Json(_)) => {
                args.push("--header".to_owned());
                args.push("Content-Type: application/json".to_owned());
            }
            Some(Body::Raw {
                bytes,
                content_type,
            }) => {
                args.push("--header".to_owned());
                args.push(format!("Content-Type: {content_type}"));
                // Why: gh streams stdin chunked, and the uploads host refuses a body without a length.
                args.push("--header".to_owned());
                args.push(format!("Content-Length: {}", bytes.len()));
            }
            None => {}
        }
        if self.body.is_some() {
            args.push("--input".to_owned());
            args.push("-".to_owned());
        }
        args
    }

    pub fn stdin(&self) -> Option<Vec<u8>> {
        match &self.body {
            Some(Body::Json(value)) => Some(value.to_string().into_bytes()),
            Some(Body::Raw { bytes, .. }) => Some(bytes.clone()),
            None => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GhOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

pub trait Gh: Send + Sync {
    fn execute(
        &self,
        request: &GhRequest,
    ) -> impl Future<Output = std::io::Result<GhOutput>> + Send;
}

impl<T: Gh> Gh for std::sync::Arc<T> {
    fn execute(
        &self,
        request: &GhRequest,
    ) -> impl Future<Output = std::io::Result<GhOutput>> + Send {
        (**self).execute(request)
    }
}

#[derive(Debug, Clone)]
pub struct GhCli {
    pub program: OsString,
    pub hostname: Option<String>,
    pub timeout: Duration,
}

impl Default for GhCli {
    fn default() -> Self {
        Self {
            program: OsString::from("gh"),
            hostname: None,
            timeout: Duration::from_secs(600),
        }
    }
}

impl GhCli {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_hostname(mut self, hostname: impl Into<String>) -> Self {
        self.hostname = Some(hostname.into());
        self
    }
}

impl Gh for GhCli {
    async fn execute(&self, request: &GhRequest) -> std::io::Result<GhOutput> {
        let mut command = Command::new(&self.program);
        let mut args = request.args();
        if let Some(hostname) = &self.hostname {
            args.insert(1, "--hostname".to_owned());
            args.insert(2, hostname.clone());
        }
        let stdin = request.stdin();
        command
            .args(&args)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("NO_COLOR", "1")
            .env("GH_PAGER", "")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(&bytes).await?;
            pipe.shutdown().await?;
        }
        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "gh timed out"))??;
        Ok(GhOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
