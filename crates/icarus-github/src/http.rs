use crate::error::{GitHubError, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn parse(raw: &[u8]) -> Option<Self> {
        let (head, body) = split_head(raw)?;
        let head = std::str::from_utf8(head).ok()?;
        let mut lines = head.lines();
        let status_line = lines.next()?.trim();
        let mut parts = status_line.split_whitespace();
        if !parts.next()?.starts_with("HTTP/") {
            return None;
        }
        let status = parts.next()?.parse().ok()?;
        let headers = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            })
            .collect();
        Some(Self {
            status,
            headers,
            body: body.to_vec(),
        })
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body)
            .map_err(|error| GitHubError::Malformed(format!("HTTP {} body: {error}", self.status)))
    }

    pub fn json_value(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }

    pub fn message(&self) -> String {
        self.json_value()
            .and_then(|value| {
                value
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| String::from_utf8_lossy(&self.body).trim().to_owned())
    }
}

fn split_head(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    let crlf = find(raw, b"\r\n\r\n").map(|index| (index, 4));
    let lf = find(raw, b"\n\n").map(|index| (index, 2));
    let (index, len) = match (crlf, lf) {
        (Some(a), Some(b)) => {
            if a.0 <= b.0 {
                a
            } else {
                b
            }
        }
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => (raw.len(), 0),
    };
    Some((&raw[..index], &raw[index + len..]))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub(crate) fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
