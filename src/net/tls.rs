//! TLS server settings: certificates, ALPN, and what a handshake negotiated.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcgen::generate_simple_self_signed;
use tokio_rustls::rustls::crypto::ring::default_provider;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::{ProtocolVersion, ServerConfig, ServerConnection};

/// What a TLS listener serves. Plain TCP/UDP listeners keep the default.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TlsConfig {
    pub cert: CertSource,
    /// Protocols offered in ALPN, most preferred first. Empty means no ALPN.
    pub alpn: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum CertSource {
    /// Generated when the listener is applied.
    #[default]
    SelfSigned,
    /// PEM files: a certificate chain (leaf first) and its private key.
    Files { cert: PathBuf, key: PathBuf },
}

/// A ready-to-serve TLS config and its leaf certificate's fingerprint.
#[derive(Clone)]
pub struct TlsServer {
    pub config: Arc<ServerConfig>,
    /// SHA-256 of the leaf certificate, as 64 uppercase hex digits.
    pub fingerprint: String,
}

/// Loads or generates the certificate. Errors are short enough for the form.
pub fn build(tls: &TlsConfig) -> Result<TlsServer, String> {
    let (certs, key) = match &tls.cert {
        CertSource::SelfSigned => {
            let generated = generate_simple_self_signed(vec!["localhost".to_string()])
                .map_err(|e| format!("could not generate certificate: {e}"))?;
            let key = PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der());
            (vec![generated.cert.der().clone()], key.into())
        }
        CertSource::Files { cert, key } => parse_pem(&read(cert, "cert")?, &read(key, "key")?)?,
    };
    let fingerprint = fingerprint(&certs[0]);
    Ok(TlsServer {
        config: Arc::new(server_config(certs, key, &tls.alpn)?),
        fingerprint,
    })
}

fn server_config(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    alpn: &[String],
) -> Result<ServerConfig, String> {
    let mut config = ServerConfig::builder_with_provider(Arc::new(default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("bad certificate or key: {e}"))?;
    config.alpn_protocols = alpn.iter().map(|p| p.as_bytes().to_vec()).collect();
    Ok(config)
}

fn read(path: &Path, what: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("{what} file: {e}"))
}

/// The certificate chain is never empty on success.
fn parse_pem(
    cert: &[u8],
    key: &[u8],
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), String> {
    let certs = CertificateDer::pem_slice_iter(cert)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("cert file: {e}"))?;
    if certs.is_empty() {
        return Err("cert file: no certificate found".into());
    }
    let key = PrivateKeyDer::from_pem_slice(key).map_err(|e| match e {
        tokio_rustls::rustls::pki_types::pem::Error::NoItemsFound => {
            "key file: no private key found".to_string()
        }
        e => format!("key file: {e}"),
    })?;
    Ok((certs, key))
}

pub(super) fn fingerprint(cert: &CertificateDer) -> String {
    ring::digest::digest(&ring::digest::SHA256, cert)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect()
}

/// One line on what a handshake agreed on, e.g.
/// `tls1.3 · TLS13_AES_128_GCM_SHA256 · alpn http/1.1 · sni example.com`.
pub fn summary(conn: &ServerConnection) -> String {
    let mut parts = Vec::new();
    match conn.protocol_version() {
        Some(ProtocolVersion::TLSv1_3) => parts.push("tls1.3".to_string()),
        Some(ProtocolVersion::TLSv1_2) => parts.push("tls1.2".to_string()),
        Some(other) => parts.push(format!("{other:?}")),
        None => {}
    }
    if let Some(suite) = conn.negotiated_cipher_suite() {
        parts.push(format!("{:?}", suite.suite()));
    }
    if let Some(alpn) = conn.alpn_protocol() {
        parts.push(format!("alpn {}", String::from_utf8_lossy(alpn)));
    }
    if let Some(sni) = conn.server_name() {
        parts.push(format!("sni {sni}"));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_signed_builds_with_a_fingerprint_and_alpn() {
        let server = build(&TlsConfig {
            cert: CertSource::SelfSigned,
            alpn: vec!["h2".into(), "http/1.1".into()],
        })
        .unwrap();
        assert_eq!(server.fingerprint.len(), 64);
        assert!(
            server
                .fingerprint
                .chars()
                .all(|c| c.is_ascii_digit() || ('A'..='F').contains(&c))
        );
        assert_eq!(
            server.config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[test]
    fn each_self_signed_build_is_a_new_certificate() {
        let a = build(&TlsConfig::default()).unwrap();
        let b = build(&TlsConfig::default()).unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn missing_files_name_which_file() {
        let error = build(&TlsConfig {
            cert: CertSource::Files {
                cert: "/nonexistent/outpost/cert.pem".into(),
                key: "/nonexistent/outpost/key.pem".into(),
            },
            alpn: Vec::new(),
        })
        .err()
        .unwrap();
        assert!(error.starts_with("cert file: "), "{error}");
    }

    #[test]
    fn pem_parsing_accepts_a_pair_and_rejects_swapped_files() {
        let generated = generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = generated.cert.pem();
        let key = generated.signing_key.serialize_pem();

        let (certs, _) = parse_pem(cert.as_bytes(), key.as_bytes()).unwrap();
        assert_eq!(certs[0], *generated.cert.der());

        let error = parse_pem(key.as_bytes(), cert.as_bytes()).err().unwrap();
        assert_eq!(error, "cert file: no certificate found");
        let error = parse_pem(cert.as_bytes(), cert.as_bytes()).err().unwrap();
        assert_eq!(error, "key file: no private key found");
    }

    #[test]
    fn a_key_from_another_certificate_is_rejected() {
        let a = generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let b = generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let (certs, key) = parse_pem(
            a.cert.pem().as_bytes(),
            b.signing_key.serialize_pem().as_bytes(),
        )
        .unwrap();
        let error = server_config(certs, key, &[]).err().unwrap();
        assert!(error.starts_with("bad certificate or key: "), "{error}");
    }
}
