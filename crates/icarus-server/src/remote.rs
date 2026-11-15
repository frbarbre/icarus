//! Direct network access: TLS listeners, one-time pairing codes and per-client tokens.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use tokio_rustls::TlsAcceptor;

use crate::paths::ServerPaths;

pub const DEFAULT_CODE_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_CODE_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_FAILURES_PER_PEER: usize = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
const MAX_CODE_FAILURES: u32 = 20;
const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn parse_listen(text: &str) -> Result<SocketAddr> {
    text.trim()
        .trim_start_matches("tcp://")
        .parse()
        .with_context(|| {
            format!("`{text}` is not an ip:port address, e.g. 100.101.102.103:7878 or [fd7a:115c:a1e0::1]:7878")
        })
}

fn tailnet_or_private_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_loopback() || ip.is_private() || (a == 100 && (b & 0xc0) == 64)
}

pub fn is_private_or_tailnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => tailnet_or_private_v4(v4),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0]
                || v6.to_ipv4_mapped().is_some_and(tailnet_or_private_v4)
        }
    }
}

pub fn check_listen(addr: SocketAddr, allow_public: bool) -> Result<()> {
    if allow_public || is_private_or_tailnet(addr.ip()) {
        return Ok(());
    }
    bail!(
        "refusing to listen on {addr}: it is not a loopback, private (10.0.0.0/8, 172.16.0.0/12, \
         192.168.0.0/16) or Tailscale (100.64.0.0/10, fd7a:115c:a1e0::/48) address. Anyone who can \
         reach a public address can try pairing codes and probe the server. Listen on your Tailscale \
         address (`tailscale ip`), or pass --allow-public if you really mean to expose it."
    )
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

pub struct TlsIdentity {
    pub acceptor: TlsAcceptor,
    pub fingerprint: String,
}

/// Loads the server's self-signed certificate, creating it on first use. Clients pin its fingerprint when they pair.
pub fn load_or_create_tls(paths: &ServerPaths) -> Result<TlsIdentity> {
    let (cert_path, key_path) = (paths.tls_cert(), paths.tls_key());
    if !(cert_path.exists() && key_path.exists()) {
        let generated = rcgen::generate_simple_self_signed(vec!["icarus".to_string()])
            .context("generate a TLS certificate")?;
        std::fs::write(&key_path, generated.signing_key.serialize_der())?;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
        std::fs::write(&cert_path, generated.cert.der().as_ref())?;
    }
    let cert =
        std::fs::read(&cert_path).with_context(|| format!("read {}", cert_path.display()))?;
    let key = std::fs::read(&key_path).with_context(|| format!("read {}", key_path.display()))?;
    let fingerprint = sha256_hex(&cert);
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(cert)],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
    )
    .context("load the TLS certificate")?;
    Ok(TlsIdentity {
        acceptor: TlsAcceptor::from(Arc::new(config)),
        fingerprint,
    })
}

fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            '0' => 'O',
            '1' => 'I',
            other => other,
        })
        .collect()
}

pub fn format_code(code: &str) -> String {
    let (head, tail) = code.split_at(code.len() / 2);
    format!("{head}-{tail}")
}

#[derive(Default)]
struct PairingState {
    codes: Vec<(String, Instant)>,
    failures: HashMap<IpAddr, Vec<Instant>>,
    code_failures: u32,
}

#[derive(Default)]
pub struct Pairing {
    state: Mutex<PairingState>,
}

impl Pairing {
    fn state(&self) -> std::sync::MutexGuard<'_, PairingState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Returns a fresh 8-character code and how long it stays valid.
    pub fn issue(&self, ttl: Option<Duration>) -> (String, Duration) {
        let ttl = ttl.unwrap_or(DEFAULT_CODE_TTL).min(MAX_CODE_TTL);
        let random = uuid::Uuid::new_v4();
        let bits = random.as_bytes()[..5]
            .iter()
            .fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
        let code: String = (0..8)
            .rev()
            .map(|i| char::from(CODE_ALPHABET[((bits >> (i * 5)) & 31) as usize]))
            .collect();
        let mut state = self.state();
        let now = Instant::now();
        state.codes.retain(|(_, expires)| *expires > now);
        state.codes.push((code.clone(), now + ttl));
        (code, ttl)
    }

    pub fn allowed(&self, peer: IpAddr) -> bool {
        let mut state = self.state();
        let now = Instant::now();
        let recent = state.failures.entry(peer).or_default();
        recent.retain(|at| now.duration_since(*at) < FAILURE_WINDOW);
        recent.len() < MAX_FAILURES_PER_PEER
    }

    pub fn record_failure(&self, peer: IpAddr) {
        self.state()
            .failures
            .entry(peer)
            .or_default()
            .push(Instant::now());
    }

    /// Consumes `code` if it is outstanding. Too many wrong codes overall void every outstanding one.
    pub fn redeem(&self, code: &str) -> bool {
        let code = normalize_code(code);
        let mut state = self.state();
        let now = Instant::now();
        state.codes.retain(|(_, expires)| *expires > now);
        if let Some(index) = state.codes.iter().position(|(c, _)| *c == code) {
            state.codes.remove(index);
            return true;
        }
        state.code_failures += 1;
        if state.code_failures >= MAX_CODE_FAILURES {
            tracing::warn!("too many wrong pairing codes; voiding all outstanding codes");
            state.codes.clear();
            state.code_failures = 0;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("ip")
    }

    #[test]
    fn only_loopback_private_and_tailnet_addresses_are_allowed() {
        for allowed in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.10",
            "100.64.0.1",
            "100.127.255.254",
            "::1",
            "fd7a:115c:a1e0::1",
            "::ffff:100.100.1.1",
        ] {
            assert!(is_private_or_tailnet(ip(allowed)), "{allowed}");
        }
        for refused in [
            "0.0.0.0",
            "8.8.8.8",
            "100.63.255.255",
            "100.128.0.1",
            "172.32.0.1",
            "::",
            "2001:db8::1",
            "fd7a:115c:a1e1::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_private_or_tailnet(ip(refused)), "{refused}");
        }
        let public = parse_listen("8.8.8.8:7878").expect("parse");
        assert!(check_listen(public, false).is_err());
        assert!(check_listen(public, true).is_ok());
    }

    #[test]
    fn codes_are_single_use_and_forgiving_about_formatting() {
        let pairing = Pairing::default();
        let (code, ttl) = pairing.issue(None);
        assert_eq!(ttl, DEFAULT_CODE_TTL);
        assert_eq!(code.len(), 8);
        assert!(code.bytes().all(|b| CODE_ALPHABET.contains(&b)));
        let typed = format_code(&code).to_lowercase();
        assert!(pairing.redeem(&typed));
        assert!(!pairing.redeem(&code));
    }

    #[test]
    fn expired_codes_are_refused() {
        let pairing = Pairing::default();
        let (code, _) = pairing.issue(Some(Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(5));
        assert!(!pairing.redeem(&code));
    }

    #[test]
    fn repeated_failures_rate_limit_a_peer_and_void_codes() {
        let pairing = Pairing::default();
        let peer = ip("100.64.0.9");
        for _ in 0..MAX_FAILURES_PER_PEER {
            assert!(pairing.allowed(peer));
            pairing.record_failure(peer);
        }
        assert!(!pairing.allowed(peer));
        assert!(pairing.allowed(ip("100.64.0.10")));

        let (code, _) = pairing.issue(None);
        for _ in 0..MAX_CODE_FAILURES {
            assert!(!pairing.redeem("AAAAAAAA"));
        }
        assert!(!pairing.redeem(&code));
    }
}
