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

/// Real sockets on 127.0.0.1, all inside the test process.
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::time::timeout;

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
        let handle = spawn_listener(1, protocol, "127.0.0.1:0".parse().unwrap(), tx);
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
        spawn_listener(7, Protocol::Tcp, addr, tx);
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
