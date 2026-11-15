use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::classify::ProbeResponse;

const TIMEOUT: Duration = Duration::from_millis(800);
const MAX_BYTES: usize = 64 * 1024;

pub async fn get(hosts: &[String], port: u16, path: &str) -> Option<ProbeResponse> {
    let ipv6_only = !hosts.is_empty()
        && hosts
            .iter()
            .all(|h| h.starts_with('[') && h != "[::]" && h != "*");
    let address = if ipv6_only {
        format!("[::1]:{port}")
    } else {
        format!("127.0.0.1:{port}")
    };
    tokio::time::timeout(TIMEOUT, fetch(&address, port, path))
        .await
        .ok()
        .flatten()
}

async fn fetch(address: &str, port: u16, path: &str) -> Option<ProbeResponse> {
    let mut stream = TcpStream::connect(address).await.ok()?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\nAccept: text/html,application/json;q=0.9,*/*;q=0.8\r\nUser-Agent: icarus-ports\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    while raw.len() < MAX_BYTES {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
        }
    }
    parse_response(&raw)
}

pub fn parse_response(raw: &[u8]) -> Option<ProbeResponse> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()?
        .strip_prefix("HTTP/")?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let content_type = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.trim().to_string())
        .unwrap_or_default();
    Some(ProbeResponse {
        status,
        content_type,
        body: body.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_headers_and_body() {
        let response = parse_response(
            b"HTTP/1.0 200 OK\r\nServer: SimpleHTTP\r\nContent-type: text/html\r\n\r\n<html>hi</html>",
        )
        .expect("response");
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "text/html");
        assert_eq!(response.body, "<html>hi</html>");
        assert!(parse_response(b"SSH-2.0-OpenSSH\r\n").is_none());
    }
}
