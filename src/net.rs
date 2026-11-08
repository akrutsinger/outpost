//! Listener and connection tasks. All results flow back to the UI as [`NetEvent`]s.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::AbortHandle;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;

use crate::Event;

pub mod quic;
pub mod tls;

/// A client that connects but never finishes the handshake is dropped after this.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a hang-up waits to tell the peer (TLS close_notify) before dropping.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

pub type WsId = u64;
pub type SessId = u64;
pub type UdpOut = UnboundedSender<(SocketAddr, Vec<u8>)>;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    Tcp,
    Udp,
    /// TLS over TCP.
    Tls,
    /// QUIC over UDP (always TLS 1.3).
    Quic,
}

impl Protocol {
    /// In the order the settings form shows them.
    pub const ALL: [Protocol; 4] = [Protocol::Tcp, Protocol::Udp, Protocol::Tls, Protocol::Quic];

    pub fn label(self) -> &'static str {
        match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
            Protocol::Tls => "tls",
            Protocol::Quic => "quic",
        }
    }

    /// Whether the listener needs a certificate.
    pub fn uses_tls(self) -> bool {
        matches!(self, Protocol::Tls | Protocol::Quic)
    }
}

/// Write side of a session. Dropping it closes a TCP (or TLS) connection.
pub enum Outlet {
    Tcp(UnboundedSender<Vec<u8>>),
    Udp {
        tx: UdpOut,
        peer: SocketAddr,
    },
    /// Dropping it closes the QUIC connection.
    Quic(UnboundedSender<quic::QuicOut>),
}

impl Outlet {
    /// Sends on a TCP or UDP session. QUIC needs a stream: use [`Outlet::quic`].
    pub fn send(&self, bytes: Vec<u8>) -> bool {
        match self {
            Outlet::Tcp(tx) => tx.send(bytes).is_ok(),
            Outlet::Udp { tx, peer } => tx.send((*peer, bytes)).is_ok(),
            Outlet::Quic(_) => false,
        }
    }

    pub fn quic(&self, out: quic::QuicOut) -> bool {
        match self {
            Outlet::Quic(tx) => tx.send(out).is_ok(),
            Outlet::Tcp(_) | Outlet::Udp { .. } => false,
        }
    }
}

pub enum NetEvent {
    /// Listener bound. UDP listeners hand back the channel used to send datagrams.
    Listening {
        ws: WsId,
        local: SocketAddr,
        udp: Option<UdpOut>,
    },
    ListenFailed {
        ws: WsId,
        error: String,
    },
    Accepted {
        ws: WsId,
        sess: SessId,
        peer: SocketAddr,
        outlet: Outlet,
    },
    /// A TLS or QUIC session finished its handshake; `summary` says what was agreed.
    Secured {
        sess: SessId,
        summary: String,
    },
    Data {
        sess: SessId,
        bytes: Vec<u8>,
    },
    Closed {
        sess: SessId,
        error: Option<String>,
    },
    /// Data on one stream of a QUIC session.
    StreamData {
        sess: SessId,
        stream: quic::StreamId,
        bytes: Vec<u8>,
    },
    StreamEnded {
        sess: SessId,
        stream: quic::StreamId,
        end: quic::StreamEnd,
    },
    /// UDP has no connections; the app maps peers to sessions.
    Datagram {
        ws: WsId,
        peer: SocketAddr,
        bytes: Vec<u8>,
    },
}

/// `tls` is the server config for TLS and QUIC, and is ignored otherwise.
pub fn spawn_listener(
    ws: WsId,
    protocol: Protocol,
    addr: SocketAddr,
    tls: Option<Arc<ServerConfig>>,
    events: UnboundedSender<Event>,
) -> AbortHandle {
    match protocol {
        Protocol::Tcp => tokio::spawn(tcp_listener(ws, addr, None, events)).abort_handle(),
        Protocol::Udp => tokio::spawn(udp_listener(ws, addr, events)).abort_handle(),
        Protocol::Tls | Protocol::Quic => match tls {
            Some(config) if protocol == Protocol::Quic => {
                tokio::spawn(quic::listener(ws, addr, config, events)).abort_handle()
            }
            Some(config) => {
                let acceptor = TlsAcceptor::from(config);
                tokio::spawn(tcp_listener(ws, addr, Some(acceptor), events)).abort_handle()
            }
            None => tokio::spawn(async move {
                emit(
                    &events,
                    NetEvent::ListenFailed {
                        ws,
                        error: "no TLS certificate".into(),
                    },
                );
            })
            .abort_handle(),
        },
    }
}

fn emit(events: &UnboundedSender<Event>, ev: NetEvent) -> bool {
    events.send(Event::Net(ev)).is_ok()
}

/// Accepts TCP connections, running a TLS handshake on each when `tls` is set.
async fn tcp_listener(
    ws: WsId,
    addr: SocketAddr,
    tls: Option<TlsAcceptor>,
    events: UnboundedSender<Event>,
) {
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            emit(
                &events,
                NetEvent::ListenFailed {
                    ws,
                    error: e.to_string(),
                },
            );
            return;
        }
    };
    let local = listener.local_addr().unwrap_or(addr);
    emit(
        &events,
        NetEvent::Listening {
            ws,
            local,
            udp: None,
        },
    );

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let sess = next_id();
                let (tx, rx) = mpsc::unbounded_channel();
                let outlet = Outlet::Tcp(tx);
                if !emit(
                    &events,
                    NetEvent::Accepted {
                        ws,
                        sess,
                        peer,
                        outlet,
                    },
                ) {
                    return;
                }
                match &tls {
                    Some(acceptor) => tokio::spawn(tls_session(
                        sess,
                        stream,
                        acceptor.clone(),
                        rx,
                        events.clone(),
                    )),
                    None => tokio::spawn(stream_session(sess, stream, rx, events.clone())),
                };
            }
            // Usually transient (e.g. out of file descriptors); back off instead of spinning.
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// The session exists from the TCP accept, so a failed handshake shows up as
/// a closed tab with the reason.
async fn tls_session(
    sess: SessId,
    stream: TcpStream,
    acceptor: TlsAcceptor,
    outgoing: UnboundedReceiver<Vec<u8>>,
    events: UnboundedSender<Event>,
) {
    let error = match timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
        Ok(Ok(stream)) => {
            let summary = tls::summary(stream.get_ref().1);
            if emit(&events, NetEvent::Secured { sess, summary }) {
                stream_session(sess, stream, outgoing, events).await;
            }
            return;
        }
        Ok(Err(e)) => format!("tls handshake failed: {e}"),
        Err(_) => "tls handshake timed out".into(),
    };
    emit(
        &events,
        NetEvent::Closed {
            sess,
            error: Some(error),
        },
    );
}

async fn stream_session<S: AsyncRead + AsyncWrite + Unpin>(
    sess: SessId,
    mut stream: S,
    mut outgoing: UnboundedReceiver<Vec<u8>>,
    events: UnboundedSender<Event>,
) {
    let mut buf = vec![0u8; 8192];
    let error = loop {
        tokio::select! {
            read = stream.read(&mut buf) => match read {
                Ok(0) => break None,
                Ok(n) => {
                    if !emit(&events, NetEvent::Data { sess, bytes: buf[..n].to_vec() }) {
                        return;
                    }
                }
                Err(e) => break Some(e.to_string()),
            },
            msg = outgoing.recv() => match msg {
                Some(bytes) => {
                    // TLS buffers writes; flush so the peer sees them now.
                    if let Err(e) = async { stream.write_all(&bytes).await?; stream.flush().await }.await {
                        break Some(e.to_string());
                    }
                }
                // The app dropped the outlet: close locally, telling a TLS peer first.
                None => {
                    let _ = timeout(SHUTDOWN_TIMEOUT, stream.shutdown()).await;
                    break None;
                }
            },
        }
    };
    emit(&events, NetEvent::Closed { sess, error });
}

async fn udp_listener(ws: WsId, addr: SocketAddr, events: UnboundedSender<Event>) {
    let socket = match UdpSocket::bind(addr).await {
        Ok(socket) => socket,
        Err(e) => {
            emit(
                &events,
                NetEvent::ListenFailed {
                    ws,
                    error: e.to_string(),
                },
            );
            return;
        }
    };
    let local = socket.local_addr().unwrap_or(addr);
    let (tx, mut rx) = mpsc::unbounded_channel::<(SocketAddr, Vec<u8>)>();
    emit(
        &events,
        NetEvent::Listening {
            ws,
            local,
            udp: Some(tx),
        },
    );

    let mut buf = vec![0u8; 65535];
    loop {
        tokio::select! {
            // Errors (e.g. ICMP port unreachable from an earlier send) aren't fatal for the socket.
            recv = socket.recv_from(&mut buf) => if let Ok((n, peer)) = recv
                && !emit(&events, NetEvent::Datagram { ws, peer, bytes: buf[..n].to_vec() })
            {
                return;
            },
            out = rx.recv() => match out {
                Some((peer, bytes)) => {
                    let _ = socket.send_to(&bytes, peer).await;
                }
                None => return,
            },
        }
    }
}

/// Real sockets on 127.0.0.1, all inside the test process.
#[cfg(test)]
mod tests {
    use super::*;
    use quinn::crypto::rustls::QuicClientConfig;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio_rustls::TlsConnector;
    use tokio_rustls::rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use tokio_rustls::rustls::crypto::{
        CryptoProvider, verify_tls12_signature, verify_tls13_signature,
    };
    use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use tokio_rustls::rustls::{self, ClientConfig, DigitallySignedStruct, SignatureScheme};

    const WAIT: Duration = Duration::from_secs(5);

    async fn next(rx: &mut UnboundedReceiver<Event>) -> NetEvent {
        match timeout(WAIT, rx.recv()).await {
            Ok(Some(Event::Net(ev))) => ev,
            Ok(Some(Event::Input(_))) => panic!("unexpected input event"),
            Ok(None) => panic!("event channel closed"),
            Err(_) => panic!("timed out waiting for a net event"),
        }
    }

    /// Starts a listener on an OS-assigned port and returns its address.
    async fn listen(
        protocol: Protocol,
    ) -> (
        UnboundedReceiver<Event>,
        SocketAddr,
        Option<UdpOut>,
        AbortHandle,
    ) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = spawn_listener(1, protocol, "127.0.0.1:0".parse().unwrap(), None, tx);
        let NetEvent::Listening { ws: 1, local, udp } = next(&mut rx).await else {
            panic!("expected Listening");
        };
        (rx, local, udp, handle)
    }

    #[tokio::test]
    async fn tcp_session_round_trip_and_peer_close() {
        let (mut rx, local, udp, _listener) = listen(Protocol::Tcp).await;
        assert!(udp.is_none());

        let mut client = TcpStream::connect(local).await.unwrap();
        let NetEvent::Accepted {
            ws: 1,
            sess,
            peer,
            outlet,
        } = next(&mut rx).await
        else {
            panic!("expected Accepted");
        };
        assert_eq!(peer, client.local_addr().unwrap());

        client.write_all(b"hello\n").await.unwrap();
        let NetEvent::Data { sess: s, bytes } = next(&mut rx).await else {
            panic!("expected Data");
        };
        assert_eq!((s, bytes.as_slice()), (sess, b"hello\n".as_slice()));

        assert!(outlet.send(b"hi back\n".to_vec()));
        let mut buf = [0u8; 8];
        timeout(WAIT, client.read_exact(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf, b"hi back\n");

        drop(client);
        let NetEvent::Closed { sess: s, error } = next(&mut rx).await else {
            panic!("expected Closed");
        };
        assert_eq!((s, error), (sess, None));
    }

    #[tokio::test]
    async fn dropping_the_outlet_hangs_up_on_the_peer() {
        let (mut rx, local, _, _listener) = listen(Protocol::Tcp).await;
        let mut client = TcpStream::connect(local).await.unwrap();
        let NetEvent::Accepted { sess, outlet, .. } = next(&mut rx).await else {
            panic!("expected Accepted");
        };

        drop(outlet);
        let mut buf = [0u8; 1];
        let read = timeout(WAIT, client.read(&mut buf)).await.unwrap().unwrap();
        assert_eq!(read, 0, "peer sees EOF");
        let NetEvent::Closed {
            sess: s,
            error: None,
        } = next(&mut rx).await
        else {
            panic!("expected a clean Closed");
        };
        assert_eq!(s, sess);
    }

    #[tokio::test]
    async fn bind_failure_is_reported() {
        let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = taken.local_addr().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        spawn_listener(7, Protocol::Tcp, addr, None, tx);
        let NetEvent::ListenFailed { ws: 7, error } = next(&mut rx).await else {
            panic!("expected ListenFailed");
        };
        assert!(!error.is_empty());
    }

    #[tokio::test]
    async fn udp_datagrams_in_and_replies_out() {
        let (mut rx, local, udp, _listener) = listen(Protocol::Udp).await;
        let udp = udp.expect("UDP listeners hand back a send channel");

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(b"ping", local).await.unwrap();
        let NetEvent::Datagram { ws: 1, peer, bytes } = next(&mut rx).await else {
            panic!("expected Datagram");
        };
        assert_eq!(peer, client.local_addr().unwrap());
        assert_eq!(bytes, b"ping");

        let outlet = Outlet::Udp { tx: udp, peer };
        assert!(outlet.send(b"pong".to_vec()));
        let mut buf = [0u8; 16];
        let (n, from) = timeout(WAIT, client.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((&buf[..n], from), (b"pong".as_slice(), local));
    }

    /// A test client that trusts any certificate; tests check the fingerprint instead.
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
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
            .with_no_client_auth();
        config.alpn_protocols = vec![alpn.to_vec()];
        config
    }

    fn tls_client(alpn: &[u8]) -> TlsConnector {
        TlsConnector::from(Arc::new(client_config(alpn)))
    }

    /// Starts a TLS listener with a self-signed certificate and the given ALPN.
    async fn listen_tls(alpn: &str) -> (UnboundedReceiver<Event>, SocketAddr, tls::TlsServer) {
        let server = tls::build(&tls::TlsConfig {
            cert: tls::CertSource::SelfSigned,
            alpn: vec![alpn.into()],
        })
        .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let addr = "127.0.0.1:0".parse().unwrap();
        spawn_listener(1, Protocol::Tls, addr, Some(server.config.clone()), tx);
        let NetEvent::Listening { ws: 1, local, .. } = next(&mut rx).await else {
            panic!("expected Listening");
        };
        (rx, local, server)
    }

    #[tokio::test]
    async fn tls_session_handshakes_round_trips_and_hangs_up_cleanly() {
        let (mut rx, local, server) = listen_tls("outpost-test").await;

        let tcp = TcpStream::connect(local).await.unwrap();
        let name = ServerName::try_from("localhost").unwrap();
        let connect = tls_client(b"outpost-test").connect(name, tcp);
        let mut client = timeout(WAIT, connect).await.unwrap().unwrap();
        let peer_cert = &client.get_ref().1.peer_certificates().unwrap()[0];
        assert_eq!(tls::fingerprint(peer_cert), server.fingerprint);

        let NetEvent::Accepted { sess, outlet, .. } = next(&mut rx).await else {
            panic!("expected Accepted");
        };
        let NetEvent::Secured { sess: s, summary } = next(&mut rx).await else {
            panic!("expected Secured");
        };
        assert_eq!(s, sess);
        assert!(summary.starts_with("tls1.3 · TLS13_"), "{summary}");
        assert!(
            summary.ends_with(" · alpn outpost-test · sni localhost"),
            "{summary}"
        );

        client.write_all(b"hello\n").await.unwrap();
        client.flush().await.unwrap();
        let NetEvent::Data { bytes, .. } = next(&mut rx).await else {
            panic!("expected Data");
        };
        assert_eq!(bytes, b"hello\n");

        assert!(outlet.send(b"hi back\n".to_vec()));
        let mut buf = [0u8; 8];
        timeout(WAIT, client.read_exact(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf, b"hi back\n");

        // Hanging up sends close_notify: the client sees a clean EOF, not an error.
        drop(outlet);
        let read = timeout(WAIT, client.read(&mut buf)).await.unwrap().unwrap();
        assert_eq!(read, 0);
        let NetEvent::Closed { error: None, .. } = next(&mut rx).await else {
            panic!("expected a clean Closed");
        };
    }

    #[tokio::test]
    async fn a_failed_handshake_closes_the_session_with_the_reason() {
        let (mut rx, local, _server) = listen_tls("outpost-test").await;
        let mut client = TcpStream::connect(local).await.unwrap();
        let NetEvent::Accepted { sess, .. } = next(&mut rx).await else {
            panic!("expected Accepted");
        };

        client.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        let NetEvent::Closed {
            sess: s,
            error: Some(error),
        } = next(&mut rx).await
        else {
            panic!("expected Closed with an error");
        };
        assert_eq!(s, sess);
        assert!(error.starts_with("tls handshake failed: "), "{error}");
    }

    /// Starts a QUIC listener with a self-signed certificate and the given ALPN.
    async fn listen_quic(alpn: &str) -> (UnboundedReceiver<Event>, SocketAddr, AbortHandle) {
        let server = tls::build(&tls::TlsConfig {
            cert: tls::CertSource::SelfSigned,
            alpn: vec![alpn.into()],
        })
        .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let addr = "127.0.0.1:0".parse().unwrap();
        let handle = spawn_listener(1, Protocol::Quic, addr, Some(server.config), tx);
        let NetEvent::Listening { ws: 1, local, .. } = next(&mut rx).await else {
            panic!("expected Listening");
        };
        (rx, local, handle)
    }

    /// The client endpoint must outlive the connection.
    async fn quic_connect(
        server: SocketAddr,
        alpn: &[u8],
    ) -> (
        quinn::Endpoint,
        Result<quinn::Connection, quinn::ConnectionError>,
    ) {
        let endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let crypto = QuicClientConfig::try_from(Arc::new(client_config(alpn))).unwrap();
        let config = quinn::ClientConfig::new(Arc::new(crypto));
        let connecting = endpoint.connect_with(config, server, "localhost").unwrap();
        let conn = timeout(WAIT, connecting).await.unwrap();
        (endpoint, conn)
    }

    /// Collects one stream's data until the peer finishes it.
    async fn stream_text(rx: &mut UnboundedReceiver<Event>, want: quic::StreamId) -> Vec<u8> {
        let mut text = Vec::new();
        loop {
            match next(rx).await {
                NetEvent::StreamData { stream, bytes, .. } if stream == want => text.extend(bytes),
                NetEvent::StreamEnded {
                    stream,
                    end: quic::StreamEnd::PeerFinished,
                    ..
                } if stream == want => return text,
                _ => panic!("expected data on stream {want}"),
            }
        }
    }

    #[tokio::test]
    async fn quic_streams_flow_both_ways() {
        let (mut rx, local, _listener) = listen_quic("outpost-test").await;
        let (endpoint, conn) = quic_connect(local, b"outpost-test").await;
        let conn = conn.unwrap();

        let NetEvent::Accepted { peer, outlet, .. } = next(&mut rx).await else {
            panic!("expected Accepted");
        };
        assert_eq!(peer.port(), endpoint.local_addr().unwrap().port());
        let NetEvent::Secured { summary, .. } = next(&mut rx).await else {
            panic!("expected Secured");
        };
        assert_eq!(summary, "quic · alpn outpost-test · sni localhost");

        // A request on the client's first stream, answered on the same one.
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(b"GET /\r\n").await.unwrap();
        send.finish().unwrap();
        assert_eq!(stream_text(&mut rx, 0).await, b"GET /\r\n");
        assert!(outlet.quic(quic::QuicOut::Write {
            stream: 0,
            bytes: b"hello\n".to_vec(),
        }));
        assert!(outlet.quic(quic::QuicOut::Finish(0)));
        let reply = timeout(WAIT, recv.read_to_end(100)).await.unwrap().unwrap();
        assert_eq!(reply, b"hello\n");

        // Writing to the server's first stream ID opens it.
        assert!(outlet.quic(quic::QuicOut::Write {
            stream: quic::server_bidi(0),
            bytes: b"push\n".to_vec(),
        }));
        let (_send, mut pushed) = timeout(WAIT, conn.accept_bi()).await.unwrap().unwrap();
        assert_eq!(quinn::VarInt::from(pushed.id()).into_inner(), 1);
        let mut buf = [0u8; 5];
        timeout(WAIT, pushed.read_exact(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf, b"push\n");

        // A one-way stream from the client.
        let mut uni = conn.open_uni().await.unwrap();
        uni.write_all(b"one-way").await.unwrap();
        uni.finish().unwrap();
        assert_eq!(stream_text(&mut rx, 2).await, b"one-way");

        // Dropping the outlet closes the connection cleanly.
        drop(outlet);
        let closed = timeout(WAIT, conn.closed()).await.unwrap();
        assert!(
            matches!(closed, quinn::ConnectionError::ApplicationClosed(_)),
            "{closed}"
        );
        let NetEvent::Closed { error: None, .. } = next(&mut rx).await else {
            panic!("expected a clean Closed");
        };
    }

    #[tokio::test]
    async fn quic_peer_close_reports_nonzero_codes() {
        let (mut rx, local, _listener) = listen_quic("outpost-test").await;
        for (code, want_error) in [(0, false), (7, true)] {
            let (_endpoint, conn) = quic_connect(local, b"outpost-test").await;
            let conn = conn.unwrap();
            // Keep the outlet: dropping it would hang up from our side first.
            let NetEvent::Accepted {
                outlet: _outlet, ..
            } = next(&mut rx).await
            else {
                panic!("expected Accepted");
            };
            let NetEvent::Secured { .. } = next(&mut rx).await else {
                panic!("expected Secured");
            };
            conn.close(quinn::VarInt::from_u32(code), b"bye");
            let NetEvent::Closed { error, .. } = next(&mut rx).await else {
                panic!("expected Closed");
            };
            assert_eq!(error.is_some(), want_error, "code {code}: {error:?}");
        }
    }

    #[tokio::test]
    async fn quic_with_the_wrong_alpn_fails_the_handshake() {
        let (mut rx, local, _listener) = listen_quic("outpost-test").await;
        let (_endpoint, conn) = quic_connect(local, b"h3").await;
        assert!(conn.is_err());
        let NetEvent::Accepted { .. } = next(&mut rx).await else {
            panic!("expected Accepted");
        };
        let NetEvent::Closed {
            error: Some(error), ..
        } = next(&mut rx).await
        else {
            panic!("expected Closed with an error");
        };
        assert!(error.starts_with("quic handshake failed: "), "{error}");
    }

    #[tokio::test]
    async fn aborting_a_quic_listener_frees_its_port() {
        let (_rx, local, listener) = listen_quic("outpost-test").await;
        listener.abort();
        let rebound = timeout(WAIT, async {
            loop {
                if let Ok(socket) = UdpSocket::bind(local).await {
                    return socket;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(rebound.is_ok(), "port {local} was not released");
    }

    #[tokio::test]
    async fn aborting_a_listener_frees_its_port() {
        let (_rx, local, _, listener) = listen(Protocol::Tcp).await;
        listener.abort();
        // The aborted task drops its socket the next time the runtime polls it.
        let rebound = timeout(WAIT, async {
            loop {
                if let Ok(l) = TcpListener::bind(local).await {
                    return l;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(rebound.is_ok(), "port {local} was not released");
    }
}
