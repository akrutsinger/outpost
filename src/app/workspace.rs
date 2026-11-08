//! Workspaces: one listener each, and the sessions it accepted.

use std::net::SocketAddr;

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::AbortHandle;

use super::Session;
use crate::Event;
use crate::net::tls::{TlsConfig, TlsServer};
use crate::net::{self, Protocol, UdpOut, WsId};

#[derive(Clone, PartialEq, Eq)]
pub struct ListenerConfig {
    pub protocol: Protocol,
    pub addr: SocketAddr,
    /// Only used by TLS workspaces; the default otherwise.
    pub tls: TlsConfig,
}

pub enum ListenState {
    Binding,
    Listening,
    Failed(String),
}

/// Drives the sidebar icon/color for both workspaces and sessions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Unread,
    Open,
    Idle,
    Closed,
    Failed,
}

impl Status {
    pub fn text(self) -> &'static str {
        match self {
            Status::Unread => "unread",
            Status::Open => "open",
            Status::Idle => "idle",
            Status::Closed => "closed",
            Status::Failed => "failed",
        }
    }
}

pub struct Workspace {
    pub id: WsId,
    /// User-chosen name; `None` shows the endpoint.
    pub name: Option<String>,
    pub config: ListenerConfig,
    pub state: ListenState,
    pub sessions: Vec<Session>,
    pub active_tab: usize,
    /// The certificate in use, built from `config.tls`. TLS workspaces only.
    pub tls: Option<TlsServer>,
    pub(super) listener: Option<AbortHandle>,
    pub(super) udp_out: Option<UdpOut>,
}

impl Workspace {
    /// A workspace that isn't listening yet; [`Workspace::start`] binds it.
    pub(super) fn new(config: ListenerConfig, tls: Option<TlsServer>) -> Self {
        Self {
            id: net::next_id(),
            name: None,
            config,
            state: ListenState::Binding,
            sessions: Vec::new(),
            active_tab: 0,
            tls,
            listener: None,
            udp_out: None,
        }
    }

    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.endpoint())
    }

    /// Protocol and port, e.g. `tcp :4444`.
    pub fn endpoint(&self) -> String {
        format!(
            "{} :{}",
            self.config.protocol.label(),
            self.config.addr.port()
        )
    }

    pub fn open_count(&self) -> usize {
        self.sessions.iter().filter(|s| s.is_open()).count()
    }

    pub fn status(&self) -> Status {
        match self.state {
            ListenState::Failed(_) => Status::Failed,
            ListenState::Binding => Status::Closed,
            ListenState::Listening if self.sessions.iter().any(|s| s.unread) => Status::Unread,
            ListenState::Listening if self.open_count() > 0 => Status::Open,
            ListenState::Listening => Status::Idle,
        }
    }

    pub(super) fn start(&mut self, events: &UnboundedSender<Event>) {
        self.state = ListenState::Binding;
        self.listener = Some(net::spawn_listener(
            self.id,
            self.config.protocol,
            self.config.addr,
            self.tls.as_ref().map(|tls| tls.config.clone()),
            events.clone(),
        ));
    }

    /// Whether applying `config` stops and restarts the listener.
    pub(super) fn restarts_for(&self, config: &ListenerConfig) -> bool {
        self.config != *config || matches!(self.state, ListenState::Failed(_))
    }

    /// Whether restarting with `config` would drop open sessions.
    pub(super) fn restart_drops_sessions(&self, config: &ListenerConfig) -> bool {
        self.restarts_for(config) && self.sessions_share_socket() && self.open_count() > 0
    }

    /// TCP connections outlive their listener; UDP and QUIC sessions share its socket.
    fn sessions_share_socket(&self) -> bool {
        matches!(self.config.protocol, Protocol::Udp | Protocol::Quic)
    }

    pub(super) fn stop(&mut self) {
        if let Some(handle) = self.listener.take() {
            handle.abort();
        }
        self.udp_out = None;
        if self.sessions_share_socket() {
            for s in &mut self.sessions {
                s.close(Some("listener stopped".into()));
            }
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if let Some(handle) = self.listener.take() {
            handle.abort();
        }
    }
}
