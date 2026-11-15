//! TLS to a server's `--listen` address. Servers use self-signed certificates, so trust comes from
//! the SHA-256 fingerprint pinned at pairing time, not from a CA.

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn normalize(fingerprint: &str) -> String {
    fingerprint
        .chars()
        .filter(char::is_ascii_hexdigit)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[derive(Debug)]
struct Pinned {
    expected: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual = fingerprint(end_entity.as_ref());
        if let Ok(mut seen) = self.seen.lock() {
            *seen = Some(actual.clone());
        }
        match &self.expected {
            Some(expected) if normalize(expected) != actual => Err(rustls::Error::General(
                "server certificate fingerprint does not match".into(),
            )),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Runs the TLS handshake, returning the stream and the server certificate's fingerprint.
pub(crate) async fn connect(
    stream: TcpStream,
    expected: Option<&str>,
) -> Result<(TlsStream<TcpStream>, String)> {
    // An explicit provider, so a process that links both ring and aws-lc-rs (e.g. Zed) still works.
    let provider: CryptoProvider = rustls::crypto::ring::default_provider();
    let seen = Arc::new(Mutex::new(None));
    let verifier = Pinned {
        expected: expected.map(str::to_string),
        seen: Arc::clone(&seen),
        algorithms: provider.signature_verification_algorithms,
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    let name = ServerName::try_from("icarus")?;
    let result = TlsConnector::from(Arc::new(config))
        .connect(name, stream)
        .await;
    let actual = seen.lock().ok().and_then(|s| s.clone());
    match (result, actual) {
        (Ok(stream), Some(actual)) => Ok((stream, actual)),
        (Err(_), Some(actual)) if expected.is_some_and(|e| normalize(e) != actual) => bail!(
            "the server's certificate changed (pinned {}, got {actual}). If the server was \
             reinstalled, pair again with `icarus remote add`; otherwise something is \
             intercepting the connection",
            expected.unwrap_or_default()
        ),
        (Err(error), _) => Err(anyhow::Error::new(error).context("TLS handshake")),
        (Ok(_), None) => bail!("TLS handshake finished without a server certificate"),
    }
}
