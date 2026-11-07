//! Listener and connection tasks. All results flow back to the UI as [`NetEvent`]s.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::AbortHandle;

use crate::Event;

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
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Protocol::Tcp => Protocol::Udp,
            Protocol::Udp => Protocol::Tcp,
        }
    }
}

/// Write side of a session. Dropping it closes a TCP connection.
pub enum Outlet {
    Tcp(UnboundedSender<Vec<u8>>),
    Udp { tx: UdpOut, peer: SocketAddr },
}

impl Outlet {
    pub fn send(&self, bytes: Vec<u8>) -> bool {
        match self {
            Outlet::Tcp(tx) => tx.send(bytes).is_ok(),
            Outlet::Udp { tx, peer } => tx.send((*peer, bytes)).is_ok(),
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
    Data {
        sess: SessId,
        bytes: Vec<u8>,
    },
    Closed {
        sess: SessId,
        error: Option<String>,
    },
    /// UDP has no connections; the app maps peers to sessions.
    Datagram {
        ws: WsId,
        peer: SocketAddr,
        bytes: Vec<u8>,
    },
}

pub fn spawn_listener(
    ws: WsId,
    protocol: Protocol,
    addr: SocketAddr,
    events: UnboundedSender<Event>,
) -> AbortHandle {
    match protocol {
        Protocol::Tcp => tokio::spawn(tcp_listener(ws, addr, events)).abort_handle(),
        Protocol::Udp => tokio::spawn(udp_listener(ws, addr, events)).abort_handle(),
    }
}

fn emit(events: &UnboundedSender<Event>, ev: NetEvent) -> bool {
    events.send(Event::Net(ev)).is_ok()
}

async fn tcp_listener(ws: WsId, addr: SocketAddr, events: UnboundedSender<Event>) {
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
                tokio::spawn(tcp_session(sess, stream, rx, events.clone()));
            }
            // Usually transient (e.g. out of file descriptors); back off instead of spinning.
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

async fn tcp_session(
    sess: SessId,
    mut stream: TcpStream,
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
                    if let Err(e) = stream.write_all(&bytes).await {
                        break Some(e.to_string());
                    }
                }
                // The app dropped the outlet: close locally.
                None => break None,
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
