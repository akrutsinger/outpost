//! Application state: workspaces (listeners), sessions (connections), and input handling.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::Position;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::AbortHandle;

use crate::Event;
use crate::net::{self, NetEvent, Outlet, Protocol, SessId, UdpOut, WsId};
use crate::ui::Hits;

const MAX_LINES: usize = 10_000;
const DEFAULT_PORT: u16 = 4444;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ListenerConfig {
    pub protocol: Protocol,
    pub addr: SocketAddr,
}

pub enum ListenState {
    Binding,
    Listening,
    Failed(String),
}

/// Drives the sidebar icon/color for both workspaces and sessions.
#[derive(Clone, Copy, PartialEq, Eq)]
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
    pub config: ListenerConfig,
    pub state: ListenState,
    pub sessions: Vec<Session>,
    pub active_tab: usize,
    listener: Option<AbortHandle>,
    udp_out: Option<UdpOut>,
}

impl Workspace {
    pub fn label(&self) -> String {
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

    fn start(&mut self, events: &UnboundedSender<Event>) {
        self.state = ListenState::Binding;
        self.listener = Some(net::spawn_listener(
            self.id,
            self.config.protocol,
            self.config.addr,
            events.clone(),
        ));
    }

    fn stop(&mut self) {
        if let Some(handle) = self.listener.take() {
            handle.abort();
        }
        self.udp_out = None;
        // TCP connections outlive their listener; UDP sessions share its socket.
        if self.config.protocol == Protocol::Udp {
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    In,
    Out,
    Info,
}

pub struct Line {
    pub dir: Dir,
    pub text: String,
}

pub struct Session {
    pub id: SessId,
    pub peer: SocketAddr,
    outlet: Option<Outlet>,
    pub lines: Vec<Line>,
    /// The last line is incoming text still waiting for its newline.
    in_open: bool,
    pub unread: bool,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub input: String,
    /// Rows scrolled up from the bottom of the transcript.
    pub scroll: usize,
}

impl Session {
    fn new(id: SessId, peer: SocketAddr, outlet: Outlet) -> Self {
        Self {
            id,
            peer,
            outlet: Some(outlet),
            lines: vec![Line {
                dir: Dir::Info,
                text: format!("connected from {peer}"),
            }],
            in_open: false,
            unread: true,
            bytes_in: 0,
            bytes_out: 0,
            input: String::new(),
            scroll: 0,
        }
    }

    pub fn is_open(&self) -> bool {
        self.outlet.is_some()
    }

    pub fn status(&self) -> Status {
        if !self.is_open() {
            Status::Closed
        } else if self.unread {
            Status::Unread
        } else {
            Status::Open
        }
    }

    fn push(&mut self, dir: Dir, text: String) {
        self.lines.push(Line { dir, text });
        if self.lines.len() > MAX_LINES {
            self.lines.drain(..self.lines.len() - MAX_LINES);
        }
    }

    fn receive(&mut self, bytes: &[u8]) {
        self.bytes_in += bytes.len() as u64;
        self.unread = true;
        // Lossy: a multi-byte character split across reads shows as U+FFFD.
        for ch in String::from_utf8_lossy(bytes).chars() {
            if ch == '\r' {
                continue;
            }
            if ch == '\n' {
                if !self.in_open {
                    self.push(Dir::In, String::new());
                }
                self.in_open = false;
                continue;
            }
            if !self.in_open {
                self.push(Dir::In, String::new());
                self.in_open = true;
            }
            let line = &mut self.lines.last_mut().expect("just pushed").text;
            match ch {
                '\t' => line.push_str("    "),
                c if c.is_control() => line.push_str(&format!("\\x{:02x}", c as u32)),
                c => line.push(c),
            }
        }
    }

    fn submit(&mut self) {
        let Some(outlet) = &self.outlet else { return };
        let text = std::mem::take(&mut self.input);
        let mut bytes = text.clone().into_bytes();
        bytes.push(b'\n');
        let len = bytes.len() as u64;
        if !outlet.send(bytes) {
            self.close(Some("send failed".into()));
            return;
        }
        self.bytes_out += len;
        self.in_open = false;
        self.push(Dir::Out, text);
        self.scroll = 0;
    }

    fn close(&mut self, error: Option<String>) {
        if self.outlet.take().is_none() {
            return;
        }
        self.in_open = false;
        let text = match error {
            Some(e) => format!("disconnected: {e}"),
            None => "disconnected".into(),
        };
        self.push(Dir::Info, text);
    }
}

pub struct SettingsForm {
    /// `None` creates a new workspace.
    pub target: Option<WsId>,
    pub protocol: Protocol,
    pub address: String,
    pub port: String,
    /// 0 = protocol, 1 = address, 2 = port.
    pub field: usize,
    pub error: Option<String>,
}

pub enum Mode {
    Normal,
    Prefix,
    Help,
    Settings(SettingsForm),
}

pub struct App {
    pub workspaces: Vec<Workspace>,
    pub active: usize,
    pub mode: Mode,
    pub hits: Hits,
    pub quit: bool,
    events: UnboundedSender<Event>,
}

impl App {
    pub fn new(events: UnboundedSender<Event>) -> Self {
        Self {
            workspaces: Vec::new(),
            active: 0,
            mode: Mode::Normal,
            hits: Hits::default(),
            quit: false,
            events,
        }
    }

    pub fn handle(&mut self, ev: Event) {
        match ev {
            Event::Input(TermEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                self.on_key(key)
            }
            Event::Input(TermEvent::Mouse(mouse)) => self.on_mouse(mouse),
            Event::Input(_) => {}
            Event::Net(ev) => self.on_net(ev),
        }
        // Whatever is on screen has been seen.
        if let Some(s) = self.focused_session_mut() {
            s.unread = false;
        }
    }

    fn focused_session_mut(&mut self) -> Option<&mut Session> {
        let ws = self.workspaces.get_mut(self.active)?;
        ws.sessions.get_mut(ws.active_tab)
    }

    fn session_mut(&mut self, id: SessId) -> Option<&mut Session> {
        self.workspaces
            .iter_mut()
            .flat_map(|ws| ws.sessions.iter_mut())
            .find(|s| s.id == id)
    }

    fn workspace_mut(&mut self, id: WsId) -> Option<&mut Workspace> {
        self.workspaces.iter_mut().find(|ws| ws.id == id)
    }

    fn on_net(&mut self, ev: NetEvent) {
        match ev {
            NetEvent::Listening { ws, local, udp } => {
                if let Some(w) = self.workspace_mut(ws) {
                    w.state = ListenState::Listening;
                    w.config.addr = local;
                    w.udp_out = udp;
                }
            }
            NetEvent::ListenFailed { ws, error } => {
                if let Some(w) = self.workspace_mut(ws) {
                    w.state = ListenState::Failed(error);
                    w.listener = None;
                }
            }
            NetEvent::Accepted {
                ws,
                sess,
                peer,
                outlet,
            } => {
                // If the workspace is gone, dropping the outlet closes the connection.
                if let Some(w) = self.workspace_mut(ws) {
                    w.sessions.push(Session::new(sess, peer, outlet));
                }
            }
            NetEvent::Data { sess, bytes } => {
                if let Some(s) = self.session_mut(sess) {
                    s.receive(&bytes);
                }
            }
            NetEvent::Closed { sess, error } => {
                if let Some(s) = self.session_mut(sess) {
                    s.close(error);
                }
            }
            NetEvent::Datagram { ws, peer, bytes } => {
                let Some(w) = self.workspace_mut(ws) else {
                    return;
                };
                let Some(tx) = w.udp_out.clone() else { return };
                let index = match w
                    .sessions
                    .iter()
                    .position(|s| s.is_open() && s.peer == peer)
                {
                    Some(index) => index,
                    None => {
                        let outlet = Outlet::Udp { tx, peer };
                        w.sessions.push(Session::new(net::next_id(), peer, outlet));
                        w.sessions.len() - 1
                    }
                };
                w.sessions[index].receive(&bytes);
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.normal_key(key),
            Mode::Prefix => self.prefix_key(key),
            Mode::Help => {}
            Mode::Settings(form) => self.settings_key(form, key),
        }
    }

    fn normal_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('b') {
            self.mode = Mode::Prefix;
            return;
        }
        let Some(s) = self.focused_session_mut() else {
            return;
        };
        match key.code {
            KeyCode::Char(c)
                if s.is_open() && !ctrl && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                s.input.push(c)
            }
            KeyCode::Backspace => {
                s.input.pop();
            }
            KeyCode::Enter => s.submit(),
            KeyCode::PageUp => s.scroll += 10,
            KeyCode::PageDown => s.scroll = s.scroll.saturating_sub(10),
            _ => {}
        }
    }

    fn prefix_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('N') => self.open_new_form(),
            KeyCode::Char('s') => self.open_settings_form(),
            KeyCode::Char('n') => self.cycle_tab(1),
            KeyCode::Char('p') => self.cycle_tab(-1),
            KeyCode::Char(c @ '1'..='9') => self.select_tab(c as usize - '1' as usize),
            KeyCode::Char('j') | KeyCode::Down => self.cycle_workspace(1),
            KeyCode::Char('k') | KeyCode::Up => self.cycle_workspace(-1),
            KeyCode::Char('X') => self.close_tab(),
            KeyCode::Char('D') => self.close_workspace(),
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('q') => self.quit = true,
            _ => {}
        }
    }

    fn settings_key(&mut self, mut form: SettingsForm, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => match self.apply_form(&form) {
                Ok(()) => return,
                Err(e) => form.error = Some(e),
            },
            KeyCode::Tab | KeyCode::Down => form.field = (form.field + 1) % 3,
            KeyCode::BackTab | KeyCode::Up => form.field = (form.field + 2) % 3,
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if form.field == 0 => {
                form.protocol = form.protocol.toggled()
            }
            KeyCode::Backspace => {
                match form.field {
                    1 => form.address.pop(),
                    2 => form.port.pop(),
                    _ => None,
                };
            }
            KeyCode::Char(c)
                if form.field == 1 && (c.is_ascii_hexdigit() || c == '.' || c == ':') =>
            {
                form.address.push(c)
            }
            KeyCode::Char(c) if form.field == 2 && c.is_ascii_digit() && form.port.len() < 5 => {
                form.port.push(c)
            }
            _ => {}
        }
        self.mode = Mode::Settings(form);
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let pos = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if matches!(self.mode, Mode::Settings(_) | Mode::Help) {
                    return;
                }
                self.mode = Mode::Normal;
                if self.hits.new_workspace.contains(pos) {
                    self.open_new_form();
                } else if self.hits.settings.contains(pos) {
                    self.open_settings_form();
                } else if let Some(&(_, ws)) =
                    self.hits.workspaces.iter().find(|(r, _)| r.contains(pos))
                {
                    self.active = ws;
                } else if let Some(&(_, ws, tab)) =
                    self.hits.sessions.iter().find(|(r, ..)| r.contains(pos))
                {
                    self.active = ws;
                    self.workspaces[ws].active_tab = tab;
                } else if let Some(&(_, tab)) = self.hits.tabs.iter().find(|(r, _)| r.contains(pos))
                {
                    self.select_tab(tab);
                }
            }
            MouseEventKind::ScrollUp if self.hits.pane.contains(pos) => {
                if let Some(s) = self.focused_session_mut() {
                    s.scroll += 3;
                }
            }
            MouseEventKind::ScrollDown if self.hits.pane.contains(pos) => {
                if let Some(s) = self.focused_session_mut() {
                    s.scroll = s.scroll.saturating_sub(3);
                }
            }
            _ => {}
        }
    }

    fn open_new_form(&mut self) {
        // Suggest the next port not already used by a workspace.
        let mut port = DEFAULT_PORT;
        while self
            .workspaces
            .iter()
            .any(|ws| ws.config.addr.port() == port)
        {
            port = port.wrapping_add(1);
        }
        self.mode = Mode::Settings(SettingsForm {
            target: None,
            protocol: Protocol::Tcp,
            address: Ipv4Addr::UNSPECIFIED.to_string(),
            port: port.to_string(),
            field: 2,
            error: None,
        });
    }

    fn open_settings_form(&mut self) {
        let Some(ws) = self.workspaces.get(self.active) else {
            self.open_new_form();
            return;
        };
        self.mode = Mode::Settings(SettingsForm {
            target: Some(ws.id),
            protocol: ws.config.protocol,
            address: ws.config.addr.ip().to_string(),
            port: ws.config.addr.port().to_string(),
            field: 0,
            error: None,
        });
    }

    fn apply_form(&mut self, form: &SettingsForm) -> Result<(), String> {
        let ip: IpAddr = form
            .address
            .trim()
            .parse()
            .map_err(|_| "invalid address".to_string())?;
        let port: u16 = form.port.parse().map_err(|_| "invalid port".to_string())?;
        let config = ListenerConfig {
            protocol: form.protocol,
            addr: SocketAddr::new(ip, port),
        };
        if self
            .workspaces
            .iter()
            .any(|ws| Some(ws.id) != form.target && ws.config == config)
        {
            return Err("a workspace already uses that address".into());
        }

        match form.target {
            None => {
                let mut ws = Workspace {
                    id: net::next_id(),
                    config,
                    state: ListenState::Binding,
                    sessions: Vec::new(),
                    active_tab: 0,
                    listener: None,
                    udp_out: None,
                };
                ws.start(&self.events);
                self.workspaces.push(ws);
                self.active = self.workspaces.len() - 1;
            }
            Some(id) => {
                let events = self.events.clone();
                let Some(ws) = self.workspace_mut(id) else {
                    return Ok(());
                };
                let failed = matches!(ws.state, ListenState::Failed(_));
                if ws.config != config || failed {
                    ws.stop();
                    ws.config = config;
                    ws.start(&events);
                }
            }
        }
        Ok(())
    }

    fn cycle_tab(&mut self, delta: isize) {
        let Some(ws) = self.workspaces.get_mut(self.active) else {
            return;
        };
        let len = ws.sessions.len() as isize;
        if len > 0 {
            ws.active_tab = (ws.active_tab as isize + delta).rem_euclid(len) as usize;
        }
    }

    fn select_tab(&mut self, index: usize) {
        if let Some(ws) = self.workspaces.get_mut(self.active)
            && index < ws.sessions.len()
        {
            ws.active_tab = index;
        }
    }

    fn cycle_workspace(&mut self, delta: isize) {
        let len = self.workspaces.len() as isize;
        if len > 0 {
            self.active = (self.active as isize + delta).rem_euclid(len) as usize;
        }
    }

    /// Disconnects (if still open) and removes the focused session.
    fn close_tab(&mut self) {
        let Some(ws) = self.workspaces.get_mut(self.active) else {
            return;
        };
        if ws.active_tab < ws.sessions.len() {
            ws.sessions.remove(ws.active_tab);
            ws.active_tab = ws.active_tab.min(ws.sessions.len().saturating_sub(1));
        }
    }

    /// Stops the listener and drops every session in the focused workspace.
    fn close_workspace(&mut self) {
        if self.active < self.workspaces.len() {
            self.workspaces.remove(self.active);
            self.active = self.active.min(self.workspaces.len().saturating_sub(1));
        }
    }
}
