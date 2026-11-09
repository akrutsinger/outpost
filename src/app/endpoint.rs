//! Endpoints: the sidebar rows that own tabs. An endpoint is a listener (one
//! bound socket, its listening task, and the sessions it accepted), the
//! local machine, whose tabs are shells, or a host those shells are
//! connected to over ssh.

use std::net::SocketAddr;

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::AbortHandle;

use super::{Session, Terminal};
use crate::Event;
use crate::detect::Destination;
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
    /// This machine. Its tabs are shells.
    Local,
    /// A machine shells are connected to, named as first typed. Its tabs are
    /// those shells; it goes once they all leave.
    Ssh(Destination),
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
// A terminal (its emulator's screen) is a few hundred bytes bigger than a
// session; an endpoint holds a handful of tabs, so boxing buys nothing.
#[expect(clippy::large_enum_variant)]
pub enum Tab {
    Session(Session),
    Terminal(Terminal),
}

impl Tab {
    pub fn id(&self) -> SessId {
        match self {
            Tab::Session(s) => s.id,
            Tab::Terminal(t) => t.id,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Tab::Session(s) => s.label(),
            Tab::Terminal(t) => t.label(),
        }
    }

    /// What the tab is called without a custom name.
    pub(super) fn default_label(&self) -> String {
        match self {
            Tab::Session(s) => s.peer.to_string(),
            Tab::Terminal(t) => t.default_label(),
        }
    }

    pub(super) fn set_name(&mut self, name: Option<String>) {
        match self {
            Tab::Session(s) => s.name = name,
            Tab::Terminal(t) => t.name = name,
        }
    }

    pub fn is_open(&self) -> bool {
        match self {
            Tab::Session(s) => s.is_open(),
            Tab::Terminal(t) => t.is_open(),
        }
    }

    pub fn status(&self) -> Status {
        match self {
            Tab::Session(s) => s.status(),
            Tab::Terminal(t) => t.status(),
        }
    }

    pub fn unread(&self) -> bool {
        match self {
            Tab::Session(s) => s.unread,
            Tab::Terminal(t) => t.unread,
        }
    }

    pub(super) fn mark_read(&mut self) {
        match self {
            Tab::Session(s) => s.unread = false,
            Tab::Terminal(t) => t.unread = false,
        }
    }

    pub fn session(&self) -> Option<&Session> {
        match self {
            Tab::Session(s) => Some(s),
            Tab::Terminal(_) => None,
        }
    }

    pub fn session_mut(&mut self) -> Option<&mut Session> {
        match self {
            Tab::Session(s) => Some(s),
            Tab::Terminal(_) => None,
        }
    }

    pub fn terminal_mut(&mut self) -> Option<&mut Terminal> {
        match self {
            Tab::Terminal(t) => Some(t),
            Tab::Session(_) => None,
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

    /// A host endpoint for `dest`, with no shells yet.
    pub(super) fn new_ssh(dest: Destination) -> Self {
        Self {
            id: net::next_id(),
            name: None,
            kind: Kind::Ssh(dest),
            tabs: Vec::new(),
            active_tab: 0,
        }
    }

    /// The local endpoint, with no shells yet.
    pub(super) fn new_local() -> Self {
        Self {
            id: net::next_id(),
            name: None,
            kind: Kind::Local,
            tabs: Vec::new(),
            active_tab: 0,
        }
    }

    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.addr_label())
    }

    /// A listener's protocol and port, e.g. `tcp :4444`, `local`, or a
    /// host's name as typed.
    pub fn addr_label(&self) -> String {
        match &self.kind {
            Kind::Listener(listener) => format!(
                "{} :{}",
                listener.config.protocol.label(),
                listener.config.addr.port()
            ),
            Kind::Local => "local".into(),
            Kind::Ssh(dest) => dest.alias.clone(),
        }
    }

    pub fn listener(&self) -> Option<&Listener> {
        match &self.kind {
            Kind::Listener(listener) => Some(listener),
            Kind::Local | Kind::Ssh(_) => None,
        }
    }

    pub fn listener_mut(&mut self) -> Option<&mut Listener> {
        match &mut self.kind {
            Kind::Listener(listener) => Some(listener),
            Kind::Local | Kind::Ssh(_) => None,
        }
    }

    pub fn open_count(&self) -> usize {
        self.tabs.iter().filter(|tab| tab.is_open()).count()
    }

    pub fn status(&self) -> Status {
        match self.listener().map(|listener| &listener.state) {
            Some(ListenState::Failed(_)) => Status::Failed,
            Some(ListenState::Binding) => Status::Closed,
            _ if self.tabs.iter().any(Tab::unread) => Status::Unread,
            _ if self.open_count() > 0 => Status::Open,
            _ => Status::Idle,
        }
    }

    pub(super) fn start(&mut self, events: &UnboundedSender<Event>) {
        let Kind::Listener(listener) = &mut self.kind else {
            return;
        };
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
        let Kind::Listener(listener) = &mut self.kind else {
            return;
        };
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
