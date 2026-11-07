//! Application state: workspaces (listeners), sessions (connections), and input handling.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::Position;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::AbortHandle;

use crate::Event;
use crate::net::tls::{CertSource, TlsConfig, TlsServer};
use crate::net::{self, NetEvent, Outlet, Protocol, SessId, UdpOut, WsId};
use crate::text_input::TextInput;
use crate::ui::{FormHit, Hits, TextTarget};

const MAX_LINES: usize = 10_000;
const DEFAULT_PORT: u16 = 4444;
/// Sidebar width limits, matching herdr's defaults.
pub const DEFAULT_SIDEBAR_WIDTH: u16 = 26;
const MIN_SIDEBAR_WIDTH: u16 = 18;
const MAX_SIDEBAR_WIDTH: u16 = 36;
/// Share of the sidebar given to workspaces; sessions get the rest.
const DEFAULT_SIDEBAR_SPLIT: f32 = 0.5;
const DOUBLE_CLICK: Duration = Duration::from_millis(350);

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
    listener: Option<AbortHandle>,
    udp_out: Option<UdpOut>,
}

impl Workspace {
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

    fn start(&mut self, events: &UnboundedSender<Event>) {
        self.state = ListenState::Binding;
        self.listener = Some(net::spawn_listener(
            self.id,
            self.config.protocol,
            self.config.addr,
            self.tls.as_ref().map(|tls| tls.config.clone()),
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
    /// User-chosen name; `None` shows the peer address.
    pub name: Option<String>,
    pub peer: SocketAddr,
    outlet: Option<Outlet>,
    pub lines: Vec<Line>,
    /// The last line is incoming text still waiting for its newline.
    in_open: bool,
    pub unread: bool,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub input: TextInput,
    /// Rows scrolled up from the bottom of the transcript.
    pub scroll: usize,
}

impl Session {
    fn new(id: SessId, peer: SocketAddr, outlet: Outlet) -> Self {
        Self {
            id,
            name: None,
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
            input: TextInput::default(),
            scroll: 0,
        }
    }

    pub fn is_open(&self) -> bool {
        self.outlet.is_some()
    }

    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.peer.to_string())
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
        let text = self.input.take();
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
    pub address: TextInput,
    pub port: TextInput,
    /// TLS only, like the fields below.
    pub cert: CertKind,
    pub cert_file: TextInput,
    pub key_file: TextInput,
    /// Comma-separated ALPN protocols.
    pub alpn: TextInput,
    pub field: Field,
    pub error: Option<String>,
}

/// A row of the settings form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Protocol,
    Address,
    Port,
    Cert,
    CertFile,
    KeyFile,
    Alpn,
}

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::Protocol => "protocol",
            Field::Address => "address",
            Field::Port => "port",
            Field::Cert => "cert",
            Field::CertFile => "cert file",
            Field::KeyFile => "key file",
            Field::Alpn => "alpn",
        }
    }

    /// The form field a text-field click belongs to.
    fn for_text(target: TextTarget) -> Option<Field> {
        match target {
            TextTarget::Address => Some(Field::Address),
            TextTarget::Port => Some(Field::Port),
            TextTarget::CertFile => Some(Field::CertFile),
            TextTarget::KeyFile => Some(Field::KeyFile),
            TextTarget::Alpn => Some(Field::Alpn),
            TextTarget::Session | TextTarget::Rename => None,
        }
    }
}

/// Where a TLS workspace's certificate comes from, as chosen in the form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CertKind {
    SelfSigned,
    Files,
}

impl CertKind {
    pub const ALL: [CertKind; 2] = [CertKind::SelfSigned, CertKind::Files];

    pub fn label(self) -> &'static str {
        match self {
            CertKind::SelfSigned => "self-signed",
            CertKind::Files => "files",
        }
    }

    fn toggled(self) -> Self {
        match self {
            CertKind::SelfSigned => CertKind::Files,
            CertKind::Files => CertKind::SelfSigned,
        }
    }
}

impl SettingsForm {
    /// The rows shown for the current choices, top to bottom.
    pub fn fields(&self) -> Vec<Field> {
        let mut fields = vec![Field::Protocol, Field::Address, Field::Port];
        if self.protocol == Protocol::Tls {
            fields.push(Field::Cert);
            if self.cert == CertKind::Files {
                fields.extend([Field::CertFile, Field::KeyFile]);
            }
            fields.push(Field::Alpn);
        }
        fields
    }

    fn focus_next(&mut self, delta: isize) {
        let fields = self.fields();
        let index = fields.iter().position(|&f| f == self.field).unwrap_or(0) as isize;
        self.field = fields[(index + delta).rem_euclid(fields.len() as isize) as usize];
    }

    fn input_mut(&mut self) -> Option<&mut TextInput> {
        match self.field {
            Field::Address => Some(&mut self.address),
            Field::Port => Some(&mut self.port),
            Field::CertFile => Some(&mut self.cert_file),
            Field::KeyFile => Some(&mut self.key_file),
            Field::Alpn => Some(&mut self.alpn),
            Field::Protocol | Field::Cert => None,
        }
    }

    /// Whether typing `c` adds it to the focused field.
    fn accepts(&self, c: char) -> bool {
        match self.field {
            Field::Address => c.is_ascii_hexdigit() || c == '.' || c == ':',
            Field::Port => c.is_ascii_digit() && self.port.len() < 5,
            Field::CertFile | Field::KeyFile | Field::Alpn => true,
            Field::Protocol | Field::Cert => false,
        }
    }

    fn tls_config(&self) -> Result<TlsConfig, String> {
        let cert = match self.cert {
            CertKind::SelfSigned => CertSource::SelfSigned,
            CertKind::Files => CertSource::Files {
                cert: form_path(&self.cert_file).ok_or("cert file is required")?,
                key: form_path(&self.key_file).ok_or("key file is required")?,
            },
        };
        let alpn = self
            .alpn
            .as_str()
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect();
        Ok(TlsConfig { cert, alpn })
    }
}

/// A path typed into the form; a leading `~/` means the home directory.
fn form_path(input: &TextInput) -> Option<PathBuf> {
    let text = input.as_str().trim();
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Some(PathBuf::from(home).join(rest));
    }
    Some(PathBuf::from(text))
}

/// What a right-click menu or rename acts on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Workspace(WsId),
    /// A tab.
    Session(SessId),
}

/// Items in the right-click menu, in display order.
pub const MENU_ITEMS: [&str; 2] = ["Rename", "Close"];

pub struct ContextMenu {
    pub target: Target,
    /// Where the click happened; the menu opens there.
    pub x: u16,
    pub y: u16,
    pub highlighted: usize,
}

pub struct RenameForm {
    pub target: Target,
    pub input: TextInput,
}

pub enum Mode {
    Normal,
    Prefix,
    Help,
    Settings(SettingsForm),
    Menu(ContextMenu),
    Rename(RenameForm),
    /// Asks before closing a workspace or tab that would drop live connections.
    ConfirmClose(Target),
}

/// A sidebar divider being dragged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Drag {
    /// The vertical line between the sidebar and the tabs.
    SidebarWidth,
    /// The horizontal line between workspaces and sessions.
    SidebarSplit,
}

pub struct App {
    pub workspaces: Vec<Workspace>,
    pub active: usize,
    pub mode: Mode,
    pub hits: Hits,
    pub quit: bool,
    pub sidebar_width: u16,
    pub sidebar_split: f32,
    drag: Option<Drag>,
    last_divider_click: Option<Instant>,
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
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            sidebar_split: DEFAULT_SIDEBAR_SPLIT,
            drag: None,
            last_divider_click: None,
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
            NetEvent::Secured { sess, summary } => {
                if let Some(s) = self.session_mut(sess) {
                    s.push(Dir::Info, summary);
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
            Mode::Menu(menu) => self.menu_key(menu, key),
            Mode::Rename(form) => self.rename_key(form, key),
            Mode::ConfirmClose(target) => match key.code {
                KeyCode::Enter => self.close_target(target),
                KeyCode::Esc => {}
                _ => self.mode = Mode::ConfirmClose(target),
            },
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
        if s.is_open() && s.input.edit(&key) {
            return;
        }
        match key.code {
            KeyCode::Char(c)
                if s.is_open() && !ctrl && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                s.input.insert(c)
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
            KeyCode::Char('X') => {
                if let Some(s) = self.focused_session_mut() {
                    let id = s.id;
                    self.request_close(Target::Session(id));
                }
            }
            KeyCode::Char('D') => {
                if let Some(ws) = self.workspaces.get(self.active) {
                    self.request_close(Target::Workspace(ws.id));
                }
            }
            KeyCode::Char('W') => {
                if let Some(ws) = self.workspaces.get(self.active) {
                    self.open_rename(Target::Workspace(ws.id));
                }
            }
            KeyCode::Char('T') => {
                if let Some(s) = self.focused_session_mut() {
                    let id = s.id;
                    self.open_rename(Target::Session(id));
                }
            }
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
            KeyCode::Tab | KeyCode::Down => form.focus_next(1),
            KeyCode::BackTab | KeyCode::Up => form.focus_next(-1),
            KeyCode::Left if form.field == Field::Protocol => {
                form.protocol = form.protocol.cycled(-1)
            }
            KeyCode::Right | KeyCode::Char(' ') if form.field == Field::Protocol => {
                form.protocol = form.protocol.cycled(1)
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if form.field == Field::Cert => {
                form.cert = form.cert.toggled()
            }
            _ if form.input_mut().is_some_and(|input| input.edit(&key)) => {}
            KeyCode::Char(c)
                if form.accepts(c)
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(input) = form.input_mut() {
                    input.insert(c);
                }
            }
            _ => {}
        }
        self.mode = Mode::Settings(form);
    }

    fn menu_key(&mut self, mut menu: ContextMenu, key: KeyEvent) {
        let len = MENU_ITEMS.len();
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => {
                self.activate_menu_item(menu.target, menu.highlighted);
                return;
            }
            KeyCode::Down => menu.highlighted = (menu.highlighted + 1) % len,
            KeyCode::Up => menu.highlighted = (menu.highlighted + len - 1) % len,
            _ => {}
        }
        self.mode = Mode::Menu(menu);
    }

    fn rename_key(&mut self, mut form: RenameForm, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => {
                // Empty, or the default label, means "no custom name".
                let name = form.input.as_str().trim();
                let custom = |default: String| {
                    (!name.is_empty() && name != default).then(|| name.to_string())
                };
                match form.target {
                    Target::Workspace(id) => {
                        if let Some(ws) = self.workspace_mut(id) {
                            ws.name = custom(ws.endpoint());
                        }
                    }
                    Target::Session(id) => {
                        if let Some(s) = self.session_mut(id) {
                            s.name = custom(s.peer.to_string());
                        }
                    }
                }
                return;
            }
            _ if form.input.edit(&key) => {}
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                form.input.insert(c)
            }
            _ => {}
        }
        self.mode = Mode::Rename(form);
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let pos = Position::new(mouse.column, mouse.row);
        let left_click = mouse.kind == MouseEventKind::Down(MouseButton::Left);
        // Where a click lands inside a text field, as (field, cursor position).
        let text_hit = self
            .hits
            .text
            .iter()
            .find(|(r, ..)| r.contains(pos))
            .map(|&(r, target, scroll)| (target, scroll + (pos.x - r.x) as usize));
        // A divider drag follows the mouse until the button is released.
        if let Some(drag) = self.drag {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    match drag {
                        Drag::SidebarWidth => self.set_sidebar_width(mouse.column),
                        Drag::SidebarSplit => self.set_sidebar_split(mouse.row),
                    }
                    return;
                }
                MouseEventKind::Up(_) => {
                    self.drag = None;
                    return;
                }
                _ => {}
            }
        }
        // Overlays capture the mouse while open.
        match &mut self.mode {
            Mode::Help => return,
            Mode::Rename(form) => {
                if left_click && let Some((TextTarget::Rename, cursor)) = text_hit {
                    form.input.set_cursor(cursor);
                }
                return;
            }
            &mut Mode::ConfirmClose(target) => {
                if left_click && self.hits.confirm.contains(pos) {
                    self.mode = Mode::Normal;
                    self.close_target(target);
                } else if left_click && self.hits.cancel.contains(pos) {
                    self.mode = Mode::Normal;
                }
                return;
            }
            Mode::Settings(form) => {
                if !left_click {
                    return;
                }
                if let Some((target, cursor)) = text_hit {
                    if let Some(field) = Field::for_text(target) {
                        form.field = field;
                        if let Some(input) = form.input_mut() {
                            input.set_cursor(cursor);
                        }
                    }
                } else if let Some(&(_, hit)) = self.hits.form.iter().find(|(r, _)| r.contains(pos))
                {
                    match hit {
                        FormHit::Protocol(protocol) => {
                            form.protocol = protocol;
                            form.field = Field::Protocol;
                        }
                        FormHit::Cert(cert) => {
                            form.cert = cert;
                            form.field = Field::Cert;
                        }
                        FormHit::Field(field) => form.field = field,
                    }
                }
                return;
            }
            Mode::Menu(menu) => {
                let item = self
                    .hits
                    .menu_items
                    .iter()
                    .find(|(r, _)| r.contains(pos))
                    .map(|&(_, item)| item);
                match mouse.kind {
                    MouseEventKind::Moved => {
                        if let Some(item) = item {
                            menu.highlighted = item;
                        }
                    }
                    // Any click picks the item under it, or dismisses the menu.
                    MouseEventKind::Down(_) => {
                        let target = menu.target;
                        self.mode = Mode::Normal;
                        if let Some(item) = item {
                            self.activate_menu_item(target, item);
                        }
                    }
                    _ => {}
                }
                return;
            }
            Mode::Normal | Mode::Prefix => {}
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Right) => {
                self.mode = Mode::Normal;
                // Workspace rows and tabs have menus; the sessions list doesn't.
                let target = if let Some(&(_, ws)) =
                    self.hits.workspaces.iter().find(|(r, _)| r.contains(pos))
                {
                    Some(Target::Workspace(self.workspaces[ws].id))
                } else if let Some(&(_, tab)) = self.hits.tabs.iter().find(|(r, _)| r.contains(pos))
                {
                    self.workspaces
                        .get(self.active)
                        .and_then(|ws| ws.sessions.get(tab))
                        .map(|s| Target::Session(s.id))
                } else {
                    None
                };
                if let Some(target) = target {
                    self.mode = Mode::Menu(ContextMenu {
                        target,
                        x: mouse.column,
                        y: mouse.row,
                        highlighted: 0,
                    });
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.mode = Mode::Normal;
                if self.hits.sidebar_divider.contains(pos) {
                    let now = Instant::now();
                    let double = self
                        .last_divider_click
                        .is_some_and(|last| now.duration_since(last) <= DOUBLE_CLICK);
                    self.last_divider_click = Some(now);
                    if double {
                        self.sidebar_width = DEFAULT_SIDEBAR_WIDTH;
                    } else {
                        self.drag = Some(Drag::SidebarWidth);
                        self.set_sidebar_width(mouse.column);
                    }
                } else if self.hits.section_divider.contains(pos) {
                    self.drag = Some(Drag::SidebarSplit);
                    self.set_sidebar_split(mouse.row);
                } else if self.hits.new_workspace.contains(pos) {
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
                } else if let Some((TextTarget::Session, cursor)) = text_hit
                    && let Some(s) = self.focused_session_mut()
                {
                    s.input.set_cursor(cursor);
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

    /// Puts the sidebar's right edge (its separator) at `column`.
    fn set_sidebar_width(&mut self, column: u16) {
        self.sidebar_width = (column + 1).clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
    }

    /// Puts the workspaces/sessions divider at `row`.
    fn set_sidebar_split(&mut self, row: u16) {
        let sidebar = self.hits.sidebar_divider;
        if sidebar.height > 0 {
            let offset = row.saturating_sub(sidebar.y) as f32;
            self.sidebar_split = (offset / sidebar.height as f32).clamp(0.1, 0.9);
        }
    }

    fn activate_menu_item(&mut self, target: Target, item: usize) {
        match (MENU_ITEMS.get(item), target) {
            (Some(&"Rename"), _) => self.open_rename(target),
            (Some(&"Close"), _) => self.request_close(target),
            _ => {}
        }
    }

    fn open_rename(&mut self, target: Target) {
        let label = match target {
            Target::Workspace(id) => self
                .workspaces
                .iter()
                .find(|w| w.id == id)
                .map(Workspace::label),
            Target::Session(id) => self
                .locate_session(id)
                .map(|(ws, index)| self.workspaces[ws].sessions[index].label()),
        };
        if let Some(label) = label {
            self.mode = Mode::Rename(RenameForm {
                target,
                input: label.into(),
            });
        }
    }

    /// (workspace index, tab index) of a session.
    fn locate_session(&self, id: SessId) -> Option<(usize, usize)> {
        self.workspaces.iter().enumerate().find_map(|(wi, ws)| {
            ws.sessions
                .iter()
                .position(|s| s.id == id)
                .map(|si| (wi, si))
        })
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
            address: Ipv4Addr::UNSPECIFIED.to_string().into(),
            port: port.to_string().into(),
            cert: CertKind::SelfSigned,
            cert_file: TextInput::default(),
            key_file: TextInput::default(),
            alpn: TextInput::default(),
            field: Field::Port,
            error: None,
        });
    }

    fn open_settings_form(&mut self) {
        let Some(ws) = self.workspaces.get(self.active) else {
            self.open_new_form();
            return;
        };
        let (cert, cert_file, key_file) = match &ws.config.tls.cert {
            CertSource::SelfSigned => (CertKind::SelfSigned, String::new(), String::new()),
            CertSource::Files { cert, key } => (
                CertKind::Files,
                cert.display().to_string(),
                key.display().to_string(),
            ),
        };
        self.mode = Mode::Settings(SettingsForm {
            target: Some(ws.id),
            protocol: ws.config.protocol,
            address: ws.config.addr.ip().to_string().into(),
            port: ws.config.addr.port().to_string().into(),
            cert,
            cert_file: cert_file.into(),
            key_file: key_file.into(),
            alpn: ws.config.tls.alpn.join(", ").into(),
            field: Field::Protocol,
            error: None,
        });
    }

    fn apply_form(&mut self, form: &SettingsForm) -> Result<(), String> {
        let ip: IpAddr = form
            .address
            .as_str()
            .trim()
            .parse()
            .map_err(|_| "invalid address".to_string())?;
        let port: u16 = form
            .port
            .as_str()
            .parse()
            .map_err(|_| "invalid port".to_string())?;
        let config = ListenerConfig {
            protocol: form.protocol,
            addr: SocketAddr::new(ip, port),
            tls: match form.protocol {
                Protocol::Tls => form.tls_config()?,
                Protocol::Tcp | Protocol::Udp => TlsConfig::default(),
            },
        };
        if self.workspaces.iter().any(|ws| {
            Some(ws.id) != form.target
                && ws.config.protocol == config.protocol
                && ws.config.addr == config.addr
        }) {
            return Err("a workspace already uses that address".into());
        }
        // Keep the certificate while its settings are unchanged, so a
        // self-signed fingerprint survives edits to the address or port.
        let current = form
            .target
            .and_then(|id| self.workspaces.iter().find(|ws| ws.id == id));
        let tls = match current {
            _ if config.protocol != Protocol::Tls => None,
            Some(ws) if ws.config.protocol == Protocol::Tls && ws.config.tls == config.tls => {
                match &ws.tls {
                    Some(tls) => Some(tls.clone()),
                    None => Some(net::tls::build(&config.tls)?),
                }
            }
            _ => Some(net::tls::build(&config.tls)?),
        };

        match form.target {
            None => {
                let mut ws = Workspace {
                    id: net::next_id(),
                    name: None,
                    config,
                    state: ListenState::Binding,
                    sessions: Vec::new(),
                    active_tab: 0,
                    tls,
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
                    ws.tls = tls;
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

    /// Disconnects (if still open) and removes a session, keeping focus on
    /// the same tab when another one is removed.
    fn remove_session(&mut self, ws: usize, index: usize) {
        let Some(ws) = self.workspaces.get_mut(ws) else {
            return;
        };
        if index >= ws.sessions.len() {
            return;
        }
        ws.sessions.remove(index);
        if index < ws.active_tab {
            ws.active_tab -= 1;
        }
        ws.active_tab = ws.active_tab.min(ws.sessions.len().saturating_sub(1));
    }

    /// Closes a workspace or tab, asking first if that would drop live connections.
    fn request_close(&mut self, target: Target) {
        let live = match target {
            Target::Workspace(id) => self
                .workspaces
                .iter()
                .find(|w| w.id == id)
                .is_some_and(|w| w.open_count() > 0),
            Target::Session(id) => self
                .locate_session(id)
                .is_some_and(|(ws, index)| self.workspaces[ws].sessions[index].is_open()),
        };
        if live {
            self.mode = Mode::ConfirmClose(target);
        } else {
            self.close_target(target);
        }
    }

    /// Closes without asking, dropping any connections.
    fn close_target(&mut self, target: Target) {
        match target {
            Target::Workspace(id) => {
                if let Some(index) = self.workspaces.iter().position(|w| w.id == id) {
                    self.remove_workspace(index);
                }
            }
            Target::Session(id) => {
                if let Some((ws, index)) = self.locate_session(id) {
                    self.remove_session(ws, index);
                }
            }
        }
    }

    /// Removes a workspace (stopping its listener and sessions), keeping focus
    /// on the same workspace when another one is removed.
    fn remove_workspace(&mut self, index: usize) {
        if index >= self.workspaces.len() {
            return;
        }
        self.workspaces.remove(index);
        if index < self.active {
            self.active -= 1;
        }
        self.active = self.active.min(self.workspaces.len().saturating_sub(1));
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
impl App {
    /// An app with no workspaces. The receiver gets events from any listener the app starts.
    pub fn test_new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self::new(tx), rx)
    }

    /// Adds a workspace that is already listening, with no socket behind it.
    pub fn add_test_workspace(&mut self, protocol: Protocol, addr: &str) -> WsId {
        let id = net::next_id();
        self.workspaces.push(Workspace {
            id,
            name: None,
            config: ListenerConfig {
                protocol,
                addr: addr.parse().expect("test address"),
                tls: TlsConfig::default(),
            },
            state: ListenState::Listening,
            sessions: Vec::new(),
            active_tab: 0,
            tls: None,
            listener: None,
            udp_out: None,
        });
        id
    }

    /// Accepts a TCP session into `ws`, as the listener would. The receiver gets
    /// every chunk the app sends to that peer; it reports disconnected once the
    /// app drops the session.
    pub fn add_test_session(
        &mut self,
        ws: WsId,
        peer: &str,
    ) -> (SessId, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let sess = net::next_id();
        self.handle(Event::Net(NetEvent::Accepted {
            ws,
            sess,
            peer: peer.parse().expect("test peer"),
            outlet: Outlet::Tcp(tx),
        }));
        (sess, rx)
    }
}
