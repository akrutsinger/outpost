//! Endpoints: the sidebar rows that own tabs. Every endpoint is a listener
//! for now: one bound socket, its listening task, and the sessions it
//! accepted.

use std::net::SocketAddr;

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::AbortHandle;

use super::Session;
use crate::Event;
use crate::net::tls::{TlsConfig, TlsServer};
use crate::net::{self, EndpointId, Protocol, SessId, UdpOut};

#[derive(Clone, PartialEq, Eq)]
pub struct ListenerConfig {
    pub protocol: Protocol,
    pub addr: SocketAddr,
    /// Only used by TLS listeners; the default otherwise.
    pub tls: TlsConfig,
}

pub enum ListenState {
    Binding,
    Listening,
    Failed(String),
}

/// Drives the sidebar icon/color for both endpoints and sessions.
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

pub struct Endpoint {
    pub id: EndpointId,
    /// User-chosen name; `None` shows the address.
    pub name: Option<String>,
    pub kind: Kind,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
}

/// What an endpoint is, holding what only that kind has.
pub enum Kind {
    Listener(Listener),
}

/// A bound socket and its listening task.
pub struct Listener {
    pub config: ListenerConfig,
    pub state: ListenState,
    /// The certificate in use, built from `config.tls`. TLS listeners only.
    pub tls: Option<TlsServer>,
    pub(super) task: Option<AbortHandle>,
    pub(super) udp_out: Option<UdpOut>,
}

/// One tab of an endpoint.
pub enum Tab {
    Session(Session),
}

impl Tab {
    pub fn id(&self) -> SessId {
        match self {
            Tab::Session(s) => s.id,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Tab::Session(s) => s.label(),
        }
    }

    /// What the tab is called without a custom name.
    pub(super) fn default_label(&self) -> String {
        match self {
            Tab::Session(s) => s.peer.to_string(),
        }
    }

    pub(super) fn set_name(&mut self, name: Option<String>) {
        match self {
            Tab::Session(s) => s.name = name,
        }
    }

    pub fn is_open(&self) -> bool {
        match self {
            Tab::Session(s) => s.is_open(),
        }
    }

    pub fn unread(&self) -> bool {
        match self {
            Tab::Session(s) => s.unread,
        }
    }

    pub(super) fn mark_read(&mut self) {
        match self {
            Tab::Session(s) => s.unread = false,
        }
    }

    pub fn session(&self) -> Option<&Session> {
        match self {
            Tab::Session(s) => Some(s),
        }
    }

    pub fn session_mut(&mut self) -> Option<&mut Session> {
        match self {
            Tab::Session(s) => Some(s),
        }
    }
}

impl Endpoint {
    /// A listener that isn't listening yet; [`Endpoint::start`] binds it.
    pub(super) fn new_listener(config: ListenerConfig, tls: Option<TlsServer>) -> Self {
        Self {
            id: net::next_id(),
            name: None,
            kind: Kind::Listener(Listener {
                config,
                state: ListenState::Binding,
                tls,
                task: None,
                udp_out: None,
            }),
            tabs: Vec::new(),
            active_tab: 0,
        }
    }

    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.addr_label())
    }

    /// Protocol and port, e.g. `tcp :4444`.
    pub fn addr_label(&self) -> String {
        match &self.kind {
            Kind::Listener(listener) => format!(
                "{} :{}",
                listener.config.protocol.label(),
                listener.config.addr.port()
            ),
        }
    }

    pub fn listener(&self) -> Option<&Listener> {
        match &self.kind {
            Kind::Listener(listener) => Some(listener),
        }
    }

    pub fn listener_mut(&mut self) -> Option<&mut Listener> {
        match &mut self.kind {
            Kind::Listener(listener) => Some(listener),
        }
    }

    pub fn open_count(&self) -> usize {
        self.tabs.iter().filter(|tab| tab.is_open()).count()
    }

    pub fn status(&self) -> Status {
        let Kind::Listener(listener) = &self.kind;
        match listener.state {
            ListenState::Failed(_) => Status::Failed,
            ListenState::Binding => Status::Closed,
            ListenState::Listening if self.tabs.iter().any(Tab::unread) => Status::Unread,
            ListenState::Listening if self.open_count() > 0 => Status::Open,
            ListenState::Listening => Status::Idle,
        }
    }

    pub(super) fn start(&mut self, events: &UnboundedSender<Event>) {
        let Kind::Listener(listener) = &mut self.kind;
        listener.state = ListenState::Binding;
        listener.task = Some(net::spawn_listener(
            self.id,
            listener.config.protocol,
            listener.config.addr,
            listener.tls.as_ref().map(|tls| tls.config.clone()),
            events.clone(),
        ));
    }

    /// Whether restarting with `config` would drop open sessions.
    pub(super) fn restart_drops_sessions(&self, config: &ListenerConfig) -> bool {
        self.listener().is_some_and(|listener| {
            listener.restarts_for(config) && listener.sessions_share_socket()
        }) && self.open_count() > 0
    }

    pub(super) fn stop(&mut self) {
        let Kind::Listener(listener) = &mut self.kind;
        if let Some(handle) = listener.task.take() {
            handle.abort();
        }
        listener.udp_out = None;
        if listener.sessions_share_socket() {
            for s in self.tabs.iter_mut().filter_map(Tab::session_mut) {
                s.close(Some("listener stopped".into()));
            }
        }
    }
}

impl Listener {
    /// Whether applying `config` stops and restarts the listening task.
    pub(super) fn restarts_for(&self, config: &ListenerConfig) -> bool {
        self.config != *config || matches!(self.state, ListenState::Failed(_))
    }

    /// TCP connections outlive the listening task; UDP and QUIC sessions share its socket.
    fn sessions_share_socket(&self) -> bool {
        matches!(self.config.protocol, Protocol::Udp | Protocol::Quic)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(handle) = self.task.take() {
            handle.abort();
        }
    }
}
