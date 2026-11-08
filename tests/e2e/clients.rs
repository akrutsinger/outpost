//! TLS and QUIC clients that trust any certificate; tests compare the
//! fingerprint outpost shows instead.

use std::sync::Arc;

use quinn::crypto::rustls::QuicClientConfig;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{
    CryptoProvider, ring::default_provider, verify_tls12_signature, verify_tls13_signature,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{self, ClientConfig, DigitallySignedStruct, SignatureScheme};

use crate::harness::WAIT;

#[derive(Debug)]
struct AcceptAnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer,
        _intermediates: &[CertificateDer],
        _server_name: &ServerName,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn client_config(alpn: &[u8]) -> ClientConfig {
    let provider = Arc::new(default_provider());
    let mut config = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
        .with_no_client_auth();
    config.alpn_protocols = vec![alpn.to_vec()];
    config
}

/// SHA-256 as outpost shows it: 64 uppercase hex digits.
pub fn fingerprint(cert: &CertificateDer) -> String {
    ring::digest::digest(&ring::digest::SHA256, cert)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect()
}

/// Connects over TLS as `localhost`; returns the stream and the server
/// certificate's fingerprint.
pub async fn tls_connect(port: u16, alpn: &[u8]) -> (TlsStream<TcpStream>, String) {
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let connector = TlsConnector::from(Arc::new(client_config(alpn)));
    let name = ServerName::try_from("localhost").unwrap();
    let tls = timeout(WAIT, connector.connect(name, tcp))
        .await
        .unwrap()
        .unwrap();
    let fingerprint = fingerprint(&tls.get_ref().1.peer_certificates().unwrap()[0]);
    (tls, fingerprint)
}

/// Connects over QUIC as `localhost`. Keep the endpoint while the connection is used.
pub async fn quic_connect(port: u16, alpn: &[u8]) -> (quinn::Endpoint, quinn::Connection) {
    let endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    let crypto = QuicClientConfig::try_from(Arc::new(client_config(alpn))).unwrap();
    let config = quinn::ClientConfig::new(Arc::new(crypto));
    let addr = ([127, 0, 0, 1], port).into();
    let connecting = endpoint.connect_with(config, addr, "localhost").unwrap();
    let conn = timeout(WAIT, connecting).await.unwrap().unwrap();
    (endpoint, conn)
}
