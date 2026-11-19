//! Each provider's plan usage, asked of the provider the way Orca asks it: the provider's own login
//! is read, never written, and is only sent to that provider. Nothing here logs or returns a token.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::updates::https_client;

const TIMEOUT: Duration = Duration::from_secs(10);
/// The usage endpoints are rate limited, so a poll within this long reuses the last answer.
pub const FRESH_FOR_MS: u64 = 3 * 60 * 1000;
pub const SESSION_MINUTES: u64 = 300;
pub const WEEK_MINUTES: u64 = 10_080;
pub const PROVIDERS: [&str; 2] = ["claude", "codex"];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Window {
    pub key: &'static str,
    pub used_percent: f64,
    pub window_minutes: u64,
    pub resets_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    NoLogin,
    Expired,
    Limited,
    Network,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Usage {
    pub provider: &'static str,
    pub account: String,
    pub plan: Option<String>,
    pub windows: Vec<Window>,
    pub failure: Option<Failure>,
    pub error: Option<String>,
    pub updated_at_ms: Option<u64>,
    /// `api` when the provider answered, `turns` when only an agent's turn reported it.
    pub source: &'static str,
    #[serde(skip)]
    pub checked_at_ms: u64,
}

type Fetched = Result<(Option<String>, Vec<Window>), (Failure, String)>;

fn cache() -> &'static Mutex<HashMap<String, Usage>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Usage>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The provider's usage for the login in `home` (its default login when `None`), refetched when
/// `force` or older than [`FRESH_FOR_MS`]. A failed refresh keeps the last windows it had.
pub fn usage(
    provider: &'static str,
    account: &str,
    home: Option<&Path>,
    force: bool,
    now_ms: u64,
) -> Usage {
    let key = format!("{provider}:{account}");
    let previous = cache().lock().ok().and_then(|c| c.get(&key).cloned());
    if let Some(previous) = &previous
        && !force
        && now_ms.saturating_sub(previous.checked_at_ms) < FRESH_FOR_MS
    {
        return previous.clone();
    }
    let fetched = match provider {
        "claude" => fetch_claude(home),
        _ => fetch_codex(home),
    };
    let usage = settle(provider, account, fetched, previous, now_ms);
    if let Ok(mut cache) = cache().lock() {
        cache.insert(key, usage.clone());
    }
    usage
}

fn settle(
    provider: &'static str,
    account: &str,
    fetched: Fetched,
    previous: Option<Usage>,
    now_ms: u64,
) -> Usage {
    match fetched {
        Ok((plan, windows)) => Usage {
            provider,
            account: account.to_owned(),
            plan,
            windows,
            failure: None,
            error: None,
            updated_at_ms: Some(now_ms),
            source: "api",
            checked_at_ms: now_ms,
        },
        Err((failure, error)) => {
            let kept =
                previous.filter(|_| failure != Failure::NoLogin && failure != Failure::Expired);
            Usage {
                provider,
                account: account.to_owned(),
                plan: kept.as_ref().and_then(|p| p.plan.clone()),
                windows: kept.as_ref().map(|p| p.windows.clone()).unwrap_or_default(),
                failure: Some(failure),
                error: Some(error),
                updated_at_ms: kept.as_ref().and_then(|p| p.updated_at_ms),
                source: "api",
                checked_at_ms: now_ms,
            }
        }
    }
}

/// Fills in what an agent's turn reported (`unifiedWindows`, utilization as a fraction) when the
/// provider could not be asked.
pub fn with_turn_windows(mut usage: Usage, rate_limits: &Value, at_ms: u64, now_ms: u64) -> Usage {
    if !usage.windows.is_empty() || usage.failure == Some(Failure::Expired) {
        return usage;
    }
    let windows: Vec<Window> = [
        ("five_hour", "session", SESSION_MINUTES),
        ("seven_day", "weekly", WEEK_MINUTES),
    ]
    .into_iter()
    .filter_map(|(field, key, minutes)| {
        let window = &rate_limits["unifiedWindows"][field];
        let resets_at_ms = window["resetsAt"].as_u64().map(|s| s.saturating_mul(1000));
        let used = if resets_at_ms.is_some_and(|r| r <= now_ms) {
            0.0
        } else {
            window["utilization"].as_f64()? * 100.0
        };
        Some(Window {
            key,
            used_percent: clamp_percent(used),
            window_minutes: minutes,
            resets_at_ms,
        })
    })
    .collect();
    if !windows.is_empty() {
        usage.windows = windows;
        usage.updated_at_ms = Some(at_ms);
        usage.source = "turns";
    }
    usage
}

fn clamp_percent(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

/// A generic password from the login keychain, through `security` so the provider's own item
/// grants it without a prompt.
fn keychain(service: &str, account: &str) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let secret = String::from_utf8(output.stdout).ok()?;
    let secret = secret.trim();
    (!secret.is_empty()).then(|| secret.to_owned())
}

fn get_json(url: &str, headers: &[(&str, &str)]) -> Result<Value, (Failure, String)> {
    let agent = https_client(TIMEOUT).map_err(|e| (Failure::Network, e))?;
    let mut request = agent.get(url);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut response = request
        .call()
        .map_err(|e| (Failure::Network, format!("could not reach {url}: {e}")))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_vec()
        .map_err(|e| (Failure::Network, e.to_string()))?;
    match status {
        200..=299 => serde_json::from_slice(&body)
            .map_err(|e| (Failure::Failed, format!("unreadable usage reply: {e}"))),
        401 | 403 => Err((
            Failure::Expired,
            format!("sign-in rejected (HTTP {status})"),
        )),
        429 => Err((
            Failure::Limited,
            "usage requests are rate limited".to_owned(),
        )),
        other => Err((
            Failure::Failed,
            format!("usage request failed (HTTP {other})"),
        )),
    }
}

/// Epoch seconds or milliseconds, or an ISO 8601 date, as epoch milliseconds.
pub fn timestamp_ms(value: &Value) -> Option<u64> {
    let number = match value {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => match s.trim().parse::<f64>() {
            Ok(n) => n,
            Err(_) => return parse_iso_ms(s.trim()),
        },
        _ => return None,
    };
    if !number.is_finite() || number <= 0.0 {
        return None;
    }
    Some(if number > 1e11 {
        number as u64
    } else {
        (number * 1000.0) as u64
    })
}

fn parse_iso_ms(text: &str) -> Option<u64> {
    let (date, time) = text.split_once(['T', ' '])?;
    let mut date_parts = date.splitn(3, '-').map(|p| p.parse::<i64>());
    let (year, month, day) = (
        date_parts.next()?.ok()?,
        date_parts.next()?.ok()?,
        date_parts.next()?.ok()?,
    );
    let (clock, offset_minutes) = if let Some(clock) = time.strip_suffix(['Z', 'z']) {
        (clock, 0)
    } else if let Some(at) = time.rfind(['+', '-']) {
        let (clock, zone) = time.split_at(at);
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let zone = zone[1..].replace(':', "");
        let hours: i64 = zone.get(..2)?.parse().ok()?;
        let minutes: i64 = zone
            .get(2..)
            .filter(|m| !m.is_empty())
            .map_or(Some(0), |m| m.parse().ok())?;
        (clock, sign * (hours * 60 + minutes))
    } else {
        (time, 0)
    };
    let mut clock_parts = clock.splitn(3, ':');
    let hour: i64 = clock_parts.next()?.parse().ok()?;
    let minute: i64 = clock_parts.next()?.parse().ok()?;
    let seconds: f64 = clock_parts.next().map_or(Some(0.0), |s| s.parse().ok())?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Howard Hinnant's days-from-civil.
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let total_seconds = days * 86_400 + hour * 3600 + minute * 60 - offset_minutes * 60;
    let ms = total_seconds as f64 * 1000.0 + seconds * 1000.0;
    (ms > 0.0).then_some(ms as u64)
}

fn plan_label(raw: &str) -> Option<String> {
    let words: Vec<String> = raw
        .split([' ', '_', '-'])
        .filter(|w| !w.is_empty())
        .map(|word| {
            if word.eq_ignore_ascii_case("chatgpt") {
                return "ChatGPT".to_owned();
            }
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first
                    .to_uppercase()
                    .chain(chars.flat_map(char::to_lowercase))
                    .collect()
            })
        })
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

// Claude

/// Claude Code's keychain item for a config dir: its own name plus a short hash of the dir.
pub fn claude_keychain_service(config_dir: Option<&Path>) -> String {
    match config_dir {
        None => "Claude Code-credentials".to_owned(),
        Some(dir) => {
            let digest = Sha256::digest(dir.to_string_lossy().as_bytes());
            let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
            format!("Claude Code-credentials-{hex}")
        }
    }
}

fn claude_login(config_dir: Option<&Path>) -> Option<(String, Option<String>)> {
    let user = std::env::var("USER")
        .ok()
        .filter(|u| {
            !u.is_empty()
                && u.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        })
        .unwrap_or_else(|| "claude-code-user".to_owned());
    let stored = keychain(&claude_keychain_service(config_dir), &user).or_else(|| {
        let dir = config_dir.map_or_else(|| home_dir().join(".claude"), Path::to_path_buf);
        std::fs::read_to_string(dir.join(".credentials.json")).ok()
    })?;
    let stored: Value = serde_json::from_str(&stored).ok()?;
    let oauth = &stored["claudeAiOauth"];
    let token = oauth["accessToken"].as_str().filter(|t| !t.is_empty())?;
    Some((
        token.to_owned(),
        oauth["subscriptionType"].as_str().and_then(plan_label),
    ))
}

fn claude_window(value: &Value, key: &'static str, minutes: u64) -> Option<Window> {
    let used = value["utilization"]
        .as_f64()
        .or_else(|| value["used_percentage"].as_f64())
        .or_else(|| value["percent"].as_f64())?;
    Some(Window {
        key,
        used_percent: clamp_percent(used),
        window_minutes: minutes,
        resets_at_ms: timestamp_ms(&value["resets_at"]),
    })
}

pub fn parse_claude(body: &Value) -> Vec<Window> {
    let mut windows: Vec<Window> = [
        ("five_hour", "session", SESSION_MINUTES),
        ("seven_day", "weekly", WEEK_MINUTES),
    ]
    .into_iter()
    .filter_map(|(field, key, minutes)| claude_window(&body[field], key, minutes))
    .collect();
    let fable = body["limits"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|limit| {
            limit["kind"] == "weekly_scoped"
                && limit["scope"]["model"]["display_name"]
                    .as_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("fable"))
        })
        .and_then(|limit| claude_window(limit, "fable", WEEK_MINUTES))
        .or_else(|| {
            ["fable_weekly", "fable_seven_day", "seven_day_fable"]
                .iter()
                .find_map(|field| claude_window(&body[*field], "fable", WEEK_MINUTES))
        });
    windows.extend(fable);
    windows
}

fn fetch_claude(config_dir: Option<&Path>) -> Fetched {
    let (token, plan) = claude_login(config_dir).ok_or((
        Failure::NoLogin,
        "No Claude sign-in on this computer".to_owned(),
    ))?;
    let body = get_json(
        "https://api.anthropic.com/api/oauth/usage",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("anthropic-beta", "oauth-2025-04-20"),
            ("User-Agent", "claude-code/2.1.0"),
            ("Accept", "application/json"),
        ],
    )
    .map_err(|(failure, error)| match failure {
        Failure::Expired => (
            failure,
            "Claude sign-in expired — run `claude /login` again".to_owned(),
        ),
        _ => (failure, error),
    })?;
    Ok((plan, parse_claude(&body)))
}

// Codex

fn codex_login(home: Option<&Path>) -> Option<(String, Option<String>)> {
    let dir = home.map(Path::to_path_buf).unwrap_or_else(|| {
        std::env::var_os("CODEX_HOME")
            .filter(|v| !v.is_empty())
            .map_or_else(|| home_dir().join(".codex"), PathBuf::from)
    });
    let auth: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("auth.json")).ok()?).ok()?;
    let token = auth["tokens"]["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())?;
    Some((
        token.to_owned(),
        auth["tokens"]["account_id"].as_str().map(str::to_owned),
    ))
}

fn codex_window_key(minutes: u64) -> Option<&'static str> {
    if minutes.abs_diff(SESSION_MINUTES) <= 1 {
        Some("session")
    } else if minutes.abs_diff(WEEK_MINUTES) <= 1 {
        Some("weekly")
    } else {
        None
    }
}

pub fn parse_codex(body: &Value) -> (Option<String>, Vec<Window>) {
    let windows = ["primary_window", "secondary_window"]
        .iter()
        .filter_map(|field| {
            let window = &body["rate_limit"][*field];
            let minutes = (window["limit_window_seconds"].as_f64()? / 60.0).round() as u64;
            Some(Window {
                key: codex_window_key(minutes)?,
                used_percent: clamp_percent(window["used_percent"].as_f64()?),
                window_minutes: minutes,
                resets_at_ms: timestamp_ms(&window["reset_at"]),
            })
        })
        .collect();
    (body["plan_type"].as_str().and_then(plan_label), windows)
}

fn fetch_codex(home: Option<&Path>) -> Fetched {
    let (token, account_id) = codex_login(home).ok_or((
        Failure::NoLogin,
        "No Codex sign-in on this computer".to_owned(),
    ))?;
    let bearer = format!("Bearer {token}");
    let mut headers = vec![
        ("Authorization", bearer.as_str()),
        ("User-Agent", "codex-cli"),
        ("OpenAI-Beta", "codex-1"),
        ("originator", "Codex Desktop"),
        ("Accept", "application/json"),
    ];
    if let Some(id) = account_id.as_deref() {
        headers.push(("ChatGPT-Account-Id", id));
    }
    let body = get_json("https://chatgpt.com/backend-api/wham/usage", &headers).map_err(
        |(failure, error)| match failure {
            Failure::Expired => (
                failure,
                "Codex sign-in expired — run `codex login` again".to_owned(),
            ),
            _ => (failure, error),
        },
    )?;
    Ok(parse_codex(&body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_reads_session_weekly_and_the_fable_weekly_limit() {
        let windows = parse_claude(&json!({
            "five_hour": { "utilization": 2.0, "resets_at": "2026-11-19T15:00:00.5+00:00" },
            "seven_day": { "utilization": 11.4, "resets_at": 1_795_000_000 },
            "limits": [
                { "kind": "weekly_scoped", "scope": { "model": { "display_name": "Opus" } }, "percent": 50 },
                { "kind": "weekly_scoped", "scope": { "model": { "display_name": "Fable" } }, "percent": 0, "resets_at": 1_795_000_000_000u64 },
            ],
        }));
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].key, "session");
        assert_eq!(windows[0].resets_at_ms, Some(1_795_100_400_500));
        assert_eq!(windows[1].used_percent, 11.4);
        assert_eq!(windows[1].resets_at_ms, Some(1_795_000_000_000));
        assert_eq!((windows[2].key, windows[2].used_percent), ("fable", 0.0));
        assert_eq!(windows[2].resets_at_ms, Some(1_795_000_000_000));

        let fallback = parse_claude(&json!({ "fable_weekly": { "utilization": 130 } }));
        assert_eq!(
            (fallback[0].key, fallback[0].used_percent),
            ("fable", 100.0)
        );
    }

    #[test]
    fn iso_dates_honour_their_offset() {
        assert_eq!(parse_iso_ms("1970-01-02T00:00:00Z"), Some(86_400_000));
        assert_eq!(parse_iso_ms("1970-01-02T01:00:00+01:00"), Some(86_400_000));
        assert_eq!(parse_iso_ms("2000-03-01T00:00:00Z"), Some(951_868_800_000));
        assert_eq!(parse_iso_ms("not a date"), None);
    }

    #[test]
    fn codex_windows_are_classified_by_their_length() {
        let (plan, windows) = parse_codex(&json!({
            "plan_type": "chatgpt_plus",
            "rate_limit": {
                "primary_window": { "used_percent": 4, "limit_window_seconds": 18_000, "reset_at": 1_795_000_000 },
                "secondary_window": { "used_percent": 61, "limit_window_seconds": 604_800, "reset_at": 1_795_500_000 },
            },
        }));
        assert_eq!(plan.as_deref(), Some("ChatGPT Plus"));
        assert_eq!(
            windows.iter().map(|w| w.key).collect::<Vec<_>>(),
            ["session", "weekly"]
        );
        assert_eq!(windows[1].resets_at_ms, Some(1_795_500_000_000));

        let (_, weekly_only) = parse_codex(&json!({ "rate_limit": {
            "primary_window": { "used_percent": 9, "limit_window_seconds": 604_800 },
            "secondary_window": null,
        } }));
        assert_eq!(weekly_only[0].key, "weekly");
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_windows_but_an_expired_login_does_not() {
        let ok = settle(
            "claude",
            "system",
            Ok((
                None,
                parse_claude(&json!({ "five_hour": { "utilization": 5 } })),
            )),
            None,
            10,
        );
        let failed = settle(
            "claude",
            "system",
            Err((Failure::Network, "offline".into())),
            Some(ok.clone()),
            20,
        );
        assert_eq!(failed.windows, ok.windows);
        assert_eq!(
            (failed.failure, failed.updated_at_ms),
            (Some(Failure::Network), Some(10))
        );
        let expired = settle(
            "claude",
            "system",
            Err((Failure::Expired, "expired".into())),
            Some(ok),
            30,
        );
        assert!(expired.windows.is_empty());
    }

    #[test]
    fn turn_reported_windows_fill_in_when_the_provider_cannot_be_asked() {
        let failed = settle(
            "claude",
            "system",
            Err((Failure::Network, "offline".into())),
            None,
            0,
        );
        let limits = json!({ "unifiedWindows": {
            "five_hour": { "utilization": 0.02, "resetsAt": 5_000 },
            "seven_day": { "utilization": 0.11, "resetsAt": 1 },
        } });
        let usage = with_turn_windows(failed, &limits, 900, 2_000_000);
        assert_eq!(usage.source, "turns");
        assert_eq!(usage.windows[0].used_percent, 2.0);
        assert_eq!(usage.windows[1].used_percent, 0.0);
        assert_eq!(usage.updated_at_ms, Some(900));
    }

    #[test]
    #[ignore = "asks the real providers with this computer's logins"]
    fn live_usage() {
        for provider in PROVIDERS {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            let usage = usage(provider, "live-test", None, true, now);
            println!(
                "{provider}: {:?} {:?} {:?} {:?}",
                usage.plan, usage.failure, usage.error, usage.windows
            );
        }
    }

    #[test]
    fn keychain_names_and_cookie_parts_follow_the_providers() {
        assert_eq!(claude_keychain_service(None), "Claude Code-credentials");
        let managed = claude_keychain_service(Some(Path::new("/x")));
        assert_eq!(managed.len(), "Claude Code-credentials-".len() + 8);
        assert_eq!(plan_label("max-20x").as_deref(), Some("Max 20x"));
    }
}
