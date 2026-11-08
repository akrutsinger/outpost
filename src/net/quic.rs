//! QUIC listener and connections. Each connection is one session; data on its
//! streams is reported per stream ID.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{HandshakeData, QuicServerConfig};
use quinn::{ConnectionError, Endpoint, Incoming, ReadError, RecvStream, SendStream, VarInt};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::timeout;
use tokio_rustls::rustls::ServerConfig;

use super::{HANDSHAKE_TIMEOUT, ListenerId, NetEvent, Outlet, SessId, emit, next_id};
use crate::Event;

/// Keeps a quiet connection open while the user types a reply.
const KEEP_ALIVE: Duration = Duration::from_secs(10);

/// A stream's QUIC ID: client bidi 0, 4, 8…; server bidi 1, 5, 9…; uni 2, 3, 6, 7…
pub type StreamId = u64;

/// Whether both sides can send on the stream.
pub fn is_bidi(stream: StreamId) -> bool {
    stream & 2 == 0
}

/// The ID of the `n`th bidirectional stream the server opens (from 0).
pub fn server_bidi(n: u64) -> StreamId {
    n * 4 + 1
}

/// What the app asks a connection to do.
#[derive(Debug, PartialEq, Eq)]
pub enum QuicOut {
    /// Writes to a stream. A server stream that isn't open yet is opened first;
    /// the app picks its ID with [`server_bidi`], in order.
    Write { stream: StreamId, bytes: Vec<u8> },
    /// Ends our side of the stream (FIN).
    Finish(StreamId),
}

/// How one direction of a stream ended.
#[derive(Debug, PartialEq, Eq)]
pub enum StreamEnd {
    /// The peer finished sending (FIN).
    PeerFinished,
    /// The peer abandoned its side with this error code (RESET_STREAM).
    PeerReset(u64),
    /// We can't send on it any more, e.g. the peer sent STOP_SENDING.
    WriteFailed(String),
}

pub async fn listener(
    id: ListenerId,
    addr: SocketAddr,
    tls: Arc<ServerConfig>,
    events: UnboundedSender<Event>,
) {
    let endpoint = match bind(addr, tls) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            emit(
                &events,
                NetEvent::ListenFailed {
                    listener: id,
                    error,
                },
            );
            return;
        }
    };
    // Stopping a listener aborts this task. Closing here frees the port and
    // ends its connections, which share the socket.
    let _close = CloseOnDrop(endpoint.clone());
    let local = endpoint.local_addr().unwrap_or(addr);
    emit(
        &events,
        NetEvent::Listening {
            listener: id,
            local,
            udp: None,
        },
    );

    while let Some(incoming) = endpoint.accept().await {
        let sess = next_id();
        let (tx, rx) = mpsc::unbounded_channel();
        let accepted = NetEvent::Accepted {
            listener: id,
            sess,
            peer: incoming.remote_address(),
            outlet: Outlet::Quic(tx),
        };
        if !emit(&events, accepted) {
            return;
        }
        tokio::spawn(session(sess, incoming, rx, events.clone()));
    }
}

fn bind(addr: SocketAddr, tls: Arc<ServerConfig>) -> Result<Endpoint, String> {
    let crypto = QuicServerConfig::try_from(tls).map_err(|e| e.to_string())?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(KEEP_ALIVE));
    config.transport_config(Arc::new(transport));
    Endpoint::server(config, addr).map_err(|e| e.to_string())
}

struct CloseOnDrop(Endpoint);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close(VarInt::from_u32(0), b"listener stopped");
    }
}

/// Like TLS, the session exists before the handshake, so a failure shows as
/// a closed tab with the reason.
async fn session(
    sess: SessId,
    incoming: Incoming,
    mut outgoing: UnboundedReceiver<QuicOut>,
    events: UnboundedSender<Event>,
) {
    let handshake = timeout(HANDSHAKE_TIMEOUT, async { incoming.accept()?.await }).await;
    let conn = match handshake {
        Ok(Ok(conn)) => conn,
        Ok(Err(e)) => {
            let error = Some(format!("quic handshake failed: {e}"));
            emit(&events, NetEvent::Closed { sess, error });
            return;
        }
        Err(_) => {
            let error = Some("quic handshake timed out".into());
            emit(&events, NetEvent::Closed { sess, error });
            return;
        }
    };
    let summary = summary(&conn);
    if !emit(&events, NetEvent::Secured { sess, summary }) {
        conn.close(VarInt::from_u32(0), b"");
        return;
    }

    let mut sends: HashMap<StreamId, SendStream> = HashMap::new();
    let error = loop {
        tokio::select! {
            bi = conn.accept_bi() => match bi {
                Ok((send, recv)) => {
                    sends.insert(id(send.id()), send);
                    tokio::spawn(read(sess, recv, events.clone()));
                }
                Err(e) => break close_reason(e),
            },
            uni = conn.accept_uni() => match uni {
                Ok(recv) => {
                    tokio::spawn(read(sess, recv, events.clone()));
                }
                Err(e) => break close_reason(e),
            },
            out = outgoing.recv() => match out {
                Some(QuicOut::Write { stream, bytes }) => {
                    if !sends.contains_key(&stream) && stream & 3 == 1 {
                        match conn.open_bi().await {
                            Ok((send, recv)) => {
                                sends.insert(id(send.id()), send);
                                tokio::spawn(read(sess, recv, events.clone()));
                            }
                            Err(e) => break close_reason(e),
                        }
                    }
                    let result = match sends.get_mut(&stream) {
                        Some(send) => send.write_all(&bytes).await.map_err(|e| e.to_string()),
                        // Only if the app's ID guess was wrong, or the stream is finished.
                        None => Err("stream is not open for sending".into()),
                    };
                    if let Err(e) = result {
                        sends.remove(&stream);
                        let end = StreamEnd::WriteFailed(e);
                        emit(&events, NetEvent::StreamEnded { sess, stream, end });
                    }
                }
                Some(QuicOut::Finish(stream)) => {
                    if let Some(mut send) = sends.remove(&stream) {
                        let _ = send.finish();
                    }
                }
                // The app dropped the outlet: close the connection.
                None => {
                    conn.close(VarInt::from_u32(0), b"");
                    break None;
                }
            },
        }
    };
    emit(&events, NetEvent::Closed { sess, error });
}

async fn read(sess: SessId, mut recv: RecvStream, events: UnboundedSender<Event>) {
    let stream = id(recv.id());
    let mut buf = vec![0u8; 8192];
    let end = loop {
        match recv.read(&mut buf).await {
            Ok(Some(n)) => {
                let bytes = buf[..n].to_vec();
                if !emit(
                    &events,
                    NetEvent::StreamData {
                        sess,
                        stream,
                        bytes,
                    },
                ) {
                    return;
                }
            }
            Ok(None) => break StreamEnd::PeerFinished,
            Err(ReadError::Reset(code)) => break StreamEnd::PeerReset(code.into_inner()),
            // Connection failures are reported once, by the session.
            Err(_) => return,
        }
    };
    emit(&events, NetEvent::StreamEnded { sess, stream, end });
}

fn id(stream: quinn::StreamId) -> StreamId {
    VarInt::from(stream).into_inner()
}

/// A clean close (code 0, or ours) is not an error.
fn close_reason(e: ConnectionError) -> Option<String> {
    match e {
        ConnectionError::ApplicationClosed(close) if close.error_code == VarInt::from_u32(0) => {
            None
        }
        ConnectionError::LocallyClosed => None,
        e => Some(e.to_string()),
    }
}

/// e.g. `quic · alpn h3 · sni localhost`.
fn summary(conn: &quinn::Connection) -> String {
    let mut parts = vec!["quic".to_string()];
    if let Some(data) = conn
        .handshake_data()
        .and_then(|data| data.downcast::<HandshakeData>().ok())
    {
        if let Some(alpn) = &data.protocol {
            parts.push(format!("alpn {}", String::from_utf8_lossy(alpn)));
        }
        if let Some(sni) = &data.server_name {
            parts.push(format!("sni {sni}"));
        }
    }
    parts.join(" · ")
}
