use serde::Serialize;

const LINE_BUFFER_LIMIT: usize = 4096;
const URL_LIMIT: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParsedUrl {
    pub url: String,
    pub host: String,
    pub port: u16,
    pub https: bool,
}

impl ParsedUrl {
    // Custom names (`app.localhost`) usually carry the cookies and certs the app expects.
    pub fn host_score(&self) -> u8 {
        let host = self.host.trim_matches(['[', ']']).to_ascii_lowercase();
        if host == "localhost" || host == "127.0.0.1" || host == "::1" {
            return 2;
        }
        if host == "0.0.0.0" || host == "::" {
            return 0;
        }
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            return match ip {
                std::net::IpAddr::V4(v4) if v4.is_private() || v4.is_link_local() => 1,
                _ => 0,
            };
        }
        3
    }

    pub fn is_unspecified(&self) -> bool {
        matches!(self.host.trim_matches(['[', ']']), "0.0.0.0" | "::")
    }
}

pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    let mut last = None;
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            last = Some(n);
                            break;
                        }
                    }
                    // Cursor moves skip cells; a separator keeps the fragments on either side apart.
                    if matches!(last, Some('C' | 'D' | 'G' | 'H' | 'f')) {
                        out.push(' ');
                    }
                }
                Some(']') => {
                    while let Some(n) = chars.next() {
                        if n == '\u{7}' {
                            break;
                        }
                        if n == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' | '\n' => out.push('\n'),
            '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

pub fn extract_urls(text: &str) -> Vec<ParsedUrl> {
    let lower = text.to_ascii_lowercase();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(offset) = lower[from..].find("http") {
        let start = from + offset;
        from = start + 4;
        let rest = &lower[start..];
        if !(rest.starts_with("http://") || rest.starts_with("https://")) {
            continue;
        }
        if start > 0
            && lower[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric())
        {
            continue;
        }
        let end = text[start..]
            .find(|c: char| c.is_whitespace() || "<>\"'`|".contains(c))
            .map_or(text.len(), |i| start + i);
        let mut candidate = &text[start..end];
        while let Some(last) = candidate.chars().last() {
            if ".,;:!?)]}>'\"`".contains(last) {
                candidate = &candidate[..candidate.len() - last.len_utf8()];
            } else {
                break;
            }
        }
        from = end.max(from);
        if candidate.len() <= URL_LIMIT
            && let Some(url) = parse_url(candidate)
        {
            found.push(url);
        }
    }
    found
}

pub fn parse_url(candidate: &str) -> Option<ParsedUrl> {
    let lower = candidate.to_ascii_lowercase();
    let (https, rest) = if lower.starts_with("https://") {
        (true, &candidate[8..])
    } else if lower.starts_with("http://") {
        (false, &candidate[7..])
    } else {
        return None;
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let close = stripped.find(']')?;
        let host = &authority[..close + 2];
        let after = &stripped[close + 1..];
        (host, after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let valid_host = !host.is_empty()
        && (host.starts_with('[')
            || host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_'));
    if !valid_host {
        return None;
    }
    let port = match port {
        Some(text) => text.parse::<u16>().ok().filter(|p| *p > 0)?,
        None if https => 443,
        None => 80,
    };
    Some(ParsedUrl {
        url: candidate.to_string(),
        host: host.to_ascii_lowercase(),
        port,
        https,
    })
}

// A URL can straddle PTY reads, so only completed lines are scanned.
#[derive(Default)]
pub struct LineBuffer {
    raw: String,
}

impl LineBuffer {
    pub fn ingest(&mut self, chunk: &[u8]) -> Vec<ParsedUrl> {
        let text = String::from_utf8_lossy(chunk);
        self.raw.push_str(&text);
        if self.raw.len() > LINE_BUFFER_LIMIT {
            let mut cut = self.raw.len() - LINE_BUFFER_LIMIT;
            while !self.raw.is_char_boundary(cut) {
                cut += 1;
            }
            self.raw.drain(..cut);
        }
        let Some(last) = self.raw.rfind(['\n', '\r']) else {
            return Vec::new();
        };
        let finished: String = self.raw.drain(..=last).collect();
        if !finished.to_ascii_lowercase().contains("http") {
            return Vec::new();
        }
        extract_urls(&strip_ansi(&finished))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(text: &str) -> Vec<String> {
        extract_urls(&strip_ansi(text))
            .into_iter()
            .map(|u| format!("{}|{}|{}", u.url, u.host, u.port))
            .collect()
    }

    #[test]
    fn finds_dev_server_urls_through_colors() {
        let vite = "  \u{1b}[32m➜\u{1b}[39m  \u{1b}[1mLocal\u{1b}[22m:   \u{1b}[36mhttp://localhost:\u{1b}[1m5173\u{1b}[22m/\u{1b}[39m\r\n";
        assert_eq!(urls(vite), ["http://localhost:5173/|localhost|5173"]);
        assert_eq!(
            urls("Serving HTTP on 127.0.0.1 port 8000 (http://127.0.0.1:8000/) ..."),
            ["http://127.0.0.1:8000/|127.0.0.1|8000"]
        );
        assert_eq!(
            urls("ready - started server on http://my-feature.localhost, url: https://[::1]:3443."),
            [
                "http://my-feature.localhost|my-feature.localhost|80",
                "https://[::1]:3443|[::1]|3443"
            ]
        );
        assert_eq!(
            urls("\u{1b}]8;;http://evil:1/\u{7}link\u{1b}]8;;\u{7} plain"),
            Vec::<String>::new()
        );
        assert!(urls("xhttp://nope:1 http://:80 http://a:99999").is_empty());
    }

    #[test]
    fn line_buffer_joins_chunks_and_waits_for_newlines() {
        let mut buffer = LineBuffer::default();
        assert!(buffer.ingest(b"Local: http://local").is_empty());
        let found = buffer.ingest(b"host:3000/\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].port, 3000);
        assert!(buffer.ingest(b"no urls here\n").is_empty());
    }

    #[test]
    fn hosts_rank_custom_names_first() {
        let score = |u: &str| parse_url(u).map(|p| p.host_score());
        assert_eq!(score("http://app.localhost:1355"), Some(3));
        assert_eq!(score("http://localhost:3000"), Some(2));
        assert_eq!(score("http://192.168.1.4:3000"), Some(1));
        assert_eq!(score("http://0.0.0.0:3000"), Some(0));
    }
}
