use crate::runner::{Gh, GhOutput, GhRequest};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl FakeResponse {
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: body.into(),
        }
    }

    pub fn ok(body: impl Into<String>) -> Self {
        Self::json(200, body)
    }

    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: String::new(),
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    fn output(&self) -> GhOutput {
        let mut stdout = format!("HTTP/2.0 {} {}\r\n", self.status, reason(self.status));
        for (name, value) in &self.headers {
            stdout.push_str(&format!("{name}: {value}\r\n"));
        }
        stdout.push_str("\r\n");
        stdout.push_str(&self.body);
        GhOutput {
            success: self.status < 400,
            stdout: stdout.into_bytes(),
            stderr: if self.status < 400 {
                String::new()
            } else {
                format!("gh: HTTP {}", self.status)
            },
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        304 => "Not Modified",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        _ => "Status",
    }
}

#[derive(Debug, Clone)]
enum Reply {
    Http(FakeResponse),
    Raw(GhOutput),
}

#[derive(Debug, Default)]
pub struct FakeGh {
    replies: Mutex<BTreeMap<String, VecDeque<Reply>>>,
    requests: Mutex<Vec<GhRequest>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl FakeGh {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on(self, route: impl Into<String>, response: FakeResponse) -> Self {
        self.push(route, Reply::Http(response));
        self
    }

    pub fn on_raw(self, route: impl Into<String>, output: GhOutput) -> Self {
        self.push(route, Reply::Raw(output));
        self
    }

    fn push(&self, route: impl Into<String>, reply: Reply) {
        lock(&self.replies)
            .entry(route.into())
            .or_default()
            .push_back(reply);
    }

    pub fn requests(&self) -> Vec<GhRequest> {
        lock(&self.requests).clone()
    }

    pub fn request(&self, route: &str) -> Option<GhRequest> {
        self.requests()
            .into_iter()
            .find(|request| request.route() == route)
    }

    pub fn count(&self, route: &str) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.route() == route)
            .count()
    }

    // Why the last reply repeats: pollers ask the same route many times and only the final state matters.
    fn reply(&self, route: &str) -> Option<Reply> {
        let mut replies = lock(&self.replies);
        let queue = replies.get_mut(route)?;
        if queue.len() > 1 {
            queue.pop_front()
        } else {
            queue.front().cloned()
        }
    }
}

impl Gh for FakeGh {
    async fn execute(&self, request: &GhRequest) -> std::io::Result<GhOutput> {
        lock(&self.requests).push(request.clone());
        let route = request.route();
        Ok(match self.reply(&route) {
            Some(Reply::Http(response)) => response.output(),
            Some(Reply::Raw(output)) => output,
            None => GhOutput {
                success: false,
                stdout: Vec::new(),
                stderr: format!("no fixture for {route}"),
            },
        })
    }
}
