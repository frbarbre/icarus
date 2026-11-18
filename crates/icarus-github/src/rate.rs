use crate::http::HttpResponse;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const TIER_TWO_FLOOR: u64 = 500;
pub const STOP_FLOOR: u64 = 50;
pub const SEARCH_FLOOR: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct RateLimit {
    pub limit: u64,
    pub remaining: u64,
    pub reset: u64,
    #[serde(default)]
    pub used: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollTier {
    Full,
    FingerprintOnly,
    Stopped,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RateBudget {
    limits: BTreeMap<String, RateLimit>,
    /// Per resource: hitting the 30-a-minute search limit mustn't stop GraphQL or REST calls.
    backoff_until: BTreeMap<String, u64>,
}

impl RateBudget {
    pub fn observe(&mut self, resource: &str, limit: RateLimit) {
        self.limits.insert(resource.to_owned(), limit);
    }

    pub fn observe_response(&mut self, response: &HttpResponse) {
        let number = |name: &str| response.header(name).and_then(|value| value.parse().ok());
        let resource = response.header("x-ratelimit-resource").unwrap_or("core");
        if let (Some(limit), Some(remaining), Some(reset)) = (
            number("x-ratelimit-limit"),
            number("x-ratelimit-remaining"),
            number("x-ratelimit-reset"),
        ) {
            self.observe(
                resource,
                RateLimit {
                    limit,
                    remaining,
                    reset,
                    used: number("x-ratelimit-used").unwrap_or(0),
                },
            );
        }
        if let Some(wait) = retry_after(response) {
            let until = now_epoch() + wait.as_secs();
            let entry = self
                .backoff_until
                .entry(resource.to_owned())
                .or_insert(until);
            *entry = (*entry).max(until);
        }
    }

    pub fn get(&self, resource: &str) -> Option<RateLimit> {
        self.limits.get(resource).copied()
    }

    /// When GitHub's requested wait for `resource` ends, if it hasn't yet.
    pub fn backoff_until(&self, resource: &str, now: u64) -> Option<u64> {
        self.backoff_until
            .get(resource)
            .copied()
            .filter(|until| *until > now)
    }

    pub fn tier_at(&self, now: u64) -> PollTier {
        if self.backoff_until("graphql", now).is_some() {
            return PollTier::Stopped;
        }
        match self.live("graphql", now) {
            Some(limit) if limit.remaining < STOP_FLOOR => PollTier::Stopped,
            Some(limit) if limit.remaining < TIER_TWO_FLOOR => PollTier::FingerprintOnly,
            _ => PollTier::Full,
        }
    }

    pub fn tier(&self) -> PollTier {
        self.tier_at(now_epoch())
    }

    pub fn allows_at(&self, resource: &str, now: u64) -> bool {
        if self.backoff_until(resource, now).is_some() {
            return false;
        }
        let floor = if resource == "search" {
            SEARCH_FLOOR
        } else {
            STOP_FLOOR
        };
        self.live(resource, now)
            .is_none_or(|limit| limit.remaining >= floor)
    }

    /// Whether a call to `resource` could only be refused: GitHub asked to wait, or none are left
    /// before the reset. Failed calls still count against secondary limits, so they aren't sent.
    pub fn exhausted_at(&self, resource: &str, now: u64) -> bool {
        self.backoff_until(resource, now).is_some()
            || self
                .live(resource, now)
                .is_some_and(|limit| limit.remaining == 0)
    }

    pub fn allows(&self, resource: &str) -> bool {
        self.allows_at(resource, now_epoch())
    }

    /// When calls to `resource` may go out again (epoch seconds), if they're held back now: until
    /// GitHub's backoff ends, or until the reset once fewer than [`STOP_FLOOR`] are left.
    pub fn resume_at(&self, resource: &str) -> Option<u64> {
        self.resume_at_time(resource, now_epoch())
    }

    pub fn resume_at_time(&self, resource: &str, now: u64) -> Option<u64> {
        let backoff = self.backoff_until(resource, now);
        let spent = self
            .live(resource, now)
            .filter(|limit| limit.remaining < STOP_FLOOR)
            .map(|limit| limit.reset);
        backoff.max(spent)
    }

    fn live(&self, resource: &str, now: u64) -> Option<RateLimit> {
        self.get(resource).filter(|limit| limit.reset > now)
    }
}

pub(crate) fn retry_after(response: &HttpResponse) -> Option<Duration> {
    if !matches!(response.status, 403 | 429) {
        return None;
    }
    if let Some(seconds) = response
        .header("retry-after")
        .and_then(|value| value.parse().ok())
    {
        return Some(Duration::from_secs(seconds));
    }
    if response.header("x-ratelimit-remaining") == Some("0") {
        let reset: u64 = response.header("x-ratelimit-reset")?.parse().ok()?;
        return Some(Duration::from_secs(reset.saturating_sub(now_epoch())));
    }
    None
}

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
