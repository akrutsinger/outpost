//! Application state and input handling. The endpoint, session, terminal,
//! and settings-form types live in submodules; this module owns `App`, its
//! overlays (`Mode`), and how keys and clicks drive them.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use tokio::sync::mpsc::UnboundedSender;

use crate::Event;
use crate::detect::{self, Resolved};
use crate::net::{self, EndpointId, NetEvent, Outlet, SessId};
use crate::pty::{self, Pty, PtyEvent};
use crate::text_input::TextInput;
use crate::ui::{FormHit, Hits, TextTarget};

mod endpoint;
mod session;
mod settings;
mod terminal;

pub use endpoint::{Endpoint, Kind, ListenState, Listener, ListenerConfig, Status, Tab};
pub use session::{Dir, Session};
pub use settings::{Applied, CertKind, Field, SettingsForm};
pub use terminal::Terminal;

/// Sidebar width limits, matching herdr's defaults.
pub const DEFAULT_SIDEBAR_WIDTH: u16 = 26;
const MIN_SIDEBAR_WIDTH: u16 = 18;
const MAX_SIDEBAR_WIDTH: u16 = 36;
/// Share of the sidebar given to endpoints; sessions get the rest.
const DEFAULT_SIDEBAR_SPLIT: f32 = 0.5;
const DOUBLE_CLICK: Duration = Duration::from_millis(350);

/// The index `delta` steps from `index` in a list of `len` items, wrapping
/// around. `len` must not be zero.
fn wrap(index: usize, delta: isize, len: usize) -> usize {
    (index as isize + delta).rem_euclid(len as isize) as usize
}

/// The value recorded for the clickable region at `pos`.
fn hit<T: Copy>(targets: &[(Rect, T)], pos: Position) -> Option<T> {
    targets
        .iter()
        .find(|(r, _)| r.contains(pos))
        .map(|&(_, value)| value)
}

/// What a right-click menu or rename acts on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Endpoint(EndpointId),
    Tab(SessId),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuItem {
    Rename,
    /// A listener's settings form.
    ListenerSettings,
    Close,
    /// The global menu's items.
    Settings,
    Keybinds,
    Quit,
}

impl MenuItem {
    pub fn label(self) -> &'static str {
        match self {
            MenuItem::Rename => "Rename",
            MenuItem::ListenerSettings => "Settings",
            MenuItem::Close => "Close",
            MenuItem::Settings => "settings",
            MenuItem::Keybinds => "keybinds",
            MenuItem::Quit => "quit",
        }
    }
}

/// A right-click menu, or global menu above the sidebar footer.
pub struct ContextMenu {
    /// What the items act on; `None` for the global menu.
    pub target: Option<Target>,
    /// In display order.
    pub items: Vec<MenuItem>,
    /// The top-left corner it opens at, kept on screen when drawn.
    pub x: u16,
    pub y: u16,
    pub highlighted: usize,
}

impl ContextMenu {
    /// Width and height, border included.
    pub fn size(&self) -> (u16, u16) {
        let label_width = self
            .items
            .iter()
            .map(|item| item.label().len())
            .max()
            .unwrap_or(0) as u16;
        ((label_width + 4).max(14), self.items.len() as u16 + 2)
    }
}

pub struct RenameForm {
    pub target: Target,
    pub input: TextInput,
}

pub enum Mode {
    Normal,
    Prefix,
    Help,
    /// settings overlay: for now, its integrations section.
    AppSettings,
    /// A listener's settings form.
    Settings(SettingsForm),
    Menu(ContextMenu),
    Rename(RenameForm),
    /// Asks before an action that would drop live connections.
    Confirm(Confirm),
}

impl Mode {
    /// The endpoint or tab the open overlay acts on, highlighted while it's open.
    pub fn target(&self) -> Option<Target> {
        match self {
            Mode::Menu(menu) => menu.target,
            Mode::Rename(form) => Some(form.target),
            Mode::Confirm(Confirm::Close(target)) => Some(*target),
            Mode::Confirm(Confirm::Restart(form, _)) => form.target.map(Target::Endpoint),
            Mode::Normal
            | Mode::Prefix
            | Mode::Help
            | Mode::AppSettings
            | Mode::Settings(_)
            | Mode::Confirm(Confirm::Quit) => None,
        }
    }
}

/// An action waiting on a confirmation, because it drops live connections.
// Only one exists at a time, and `Mode::Settings` holds a form unboxed too.
#[expect(clippy::large_enum_variant)]
pub enum Confirm {
    /// Closing an endpoint or tab.
    Close(Target),
    /// Applying settings that restart a UDP or QUIC listener. Cancelling
    /// returns to the form.
    Restart(SettingsForm, Applied),
    /// Quitting, which drops every connection.
    Quit,
}

/// A sidebar divider being dragged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Drag {
    /// The vertical line between the sidebar and the tabs.
    SidebarWidth,
    /// The horizontal line between endpoints and sessions.
    SidebarSplit,
}

pub struct App {
    pub endpoints: Vec<Endpoint>,
    pub active: usize,
    pub mode: Mode,
    pub hits: Hits,
    pub quit: bool,
    pub sidebar_width: u16,
    pub sidebar_split: f32,
    drag: Option<Drag>,
    last_divider_click: Option<Instant>,
    /// Rows and columns a terminal tab gets: the pane, inside its border.
    terminal_size: (u16, u16),
    events: UnboundedSender<Event>,
}

impl App {
    pub fn new(events: UnboundedSender<Event>) -> Self {
        Self {
            endpoints: Vec::new(),
            active: 0,
            mode: Mode::Normal,
            hits: Hits::default(),
            quit: false,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            sidebar_split: DEFAULT_SIDEBAR_SPLIT,
            drag: None,
            last_divider_click: None,
            terminal_size: (24, 80),
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
            Event::Pty(ev) => self.on_pty(ev),
            Event::Tick => self.detect(),
            Event::Resolved(resolved) => self.on_resolved(resolved),
        }
        // Whatever is on screen has been seen.
        if let Some(tab) = self.focused_tab_mut() {
            tab.mark_read();
        }
    }

    fn focused_tab_mut(&mut self) -> Option<&mut Tab> {
        let endpoint = self.endpoints.get_mut(self.active)?;
        endpoint.tabs.get_mut(endpoint.active_tab)
    }

    fn focused_session_mut(&mut self) -> Option<&mut Session> {
        self.focused_tab_mut()?.session_mut()
    }

    pub fn endpoint(&self, id: EndpointId) -> Option<&Endpoint> {
        self.endpoints.iter().find(|endpoint| endpoint.id == id)
    }

    pub fn tab(&self, id: SessId) -> Option<&Tab> {
        self.endpoints
            .iter()
            .flat_map(|endpoint| &endpoint.tabs)
            .find(|tab| tab.id() == id)
    }

    fn tab_mut(&mut self, id: SessId) -> Option<&mut Tab> {
        self.endpoints
            .iter_mut()
            .flat_map(|endpoint| endpoint.tabs.iter_mut())
            .find(|tab| tab.id() == id)
    }

    fn session_mut(&mut self, id: SessId) -> Option<&mut Session> {
        self.tab_mut(id)?.session_mut()
    }

    fn endpoint_mut(&mut self, id: EndpointId) -> Option<&mut Endpoint> {
        self.endpoints.iter_mut().find(|endpoint| endpoint.id == id)
    }

    fn listener_mut(&mut self, id: EndpointId) -> Option<&mut Listener> {
        self.endpoint_mut(id)?.listener_mut()
    }

    fn terminal_mut(&mut self, id: SessId) -> Option<&mut Terminal> {
        self.tab_mut(id)?.terminal_mut()
    }

    fn on_pty(&mut self, ev: PtyEvent) {
        match ev {
            PtyEvent::Output { tab, bytes } => {
                if let Some(t) = self.terminal_mut(tab) {
                    t.receive(&bytes);
                }
            }
            PtyEvent::Exited { tab, status } => {
                if let Some(t) = self.terminal_mut(tab) {
                    t.exited(status);
                }
            }
        }
    }

    fn on_net(&mut self, ev: NetEvent) {
        match ev {
            NetEvent::Listening {
                listener: id,
                local,
                udp,
            } => {
                if let Some(listener) = self.listener_mut(id) {
                    listener.state = ListenState::Listening;
                    listener.config.addr = local;
                    listener.udp_out = udp;
                }
            }
            NetEvent::ListenFailed {
                listener: id,
                error,
            } => {
                if let Some(listener) = self.listener_mut(id) {
                    listener.state = ListenState::Failed(error);
                    listener.task = None;
                }
            }
            NetEvent::Accepted {
                listener: id,
                sess,
                peer,
                outlet,
            } => {
                // If the endpoint is gone, dropping the outlet closes the connection.
                if let Some(endpoint) = self.endpoint_mut(id) {
                    endpoint
                        .tabs
                        .push(Tab::Session(Session::new(sess, peer, outlet)));
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
            NetEvent::StreamData {
                sess,
                stream,
                bytes,
            } => {
                if let Some(s) = self.session_mut(sess) {
                    s.receive_stream(stream, &bytes);
                }
            }
            NetEvent::StreamEnded { sess, stream, end } => {
                if let Some(s) = self.session_mut(sess) {
                    s.stream_ended(stream, end);
                }
            }
            NetEvent::Closed { sess, error } => {
                if let Some(s) = self.session_mut(sess) {
                    s.close(error);
                }
            }
            NetEvent::Datagram {
                listener: id,
                peer,
                bytes,
            } => {
                let Some(endpoint) = self.endpoint_mut(id) else {
                    return;
                };
                let Some(tx) = endpoint.listener().and_then(|l| l.udp_out.clone()) else {
                    return;
                };
                let open = endpoint
                    .tabs
                    .iter_mut()
                    .filter_map(Tab::session_mut)
                    .find(|s| s.is_open() && s.peer == peer);
                match open {
                    Some(s) => s.receive(&bytes),
                    None => {
                        let outlet = Outlet::Udp { tx, peer };
                        let mut s = Session::new(net::next_id(), peer, outlet);
                        s.receive(&bytes);
                        endpoint.tabs.push(Tab::Session(s));
                    }
                }
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.normal_key(key),
            Mode::Prefix => self.prefix_key(key),
            Mode::Help => {}
            Mode::AppSettings if key.code == KeyCode::Esc => {}
            Mode::AppSettings => self.mode = Mode::AppSettings,
            Mode::Settings(form) => self.settings_key(form, key),
            Mode::Menu(menu) => self.menu_key(menu, key),
            Mode::Rename(form) => self.rename_key(form, key),
            Mode::Confirm(confirm) => match key.code {
                KeyCode::Enter => self.confirmed(confirm),
                KeyCode::Esc => self.cancelled(confirm),
                _ => self.mode = Mode::Confirm(confirm),
            },
        }
    }

    fn normal_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('b') {
            self.mode = Mode::Prefix;
            return;
        }
        // Everything but the prefix goes to the shell.
        if let Some(t) = self.focused_tab_mut().and_then(Tab::terminal_mut) {
            t.send_key(key);
            return;
        }
        let Some(s) = self.focused_session_mut() else {
            return;
        };
        // End of input, as in a terminal: finishes the QUIC stream.
        if ctrl && key.code == KeyCode::Char('d') && s.input.len() == 0 {
            s.finish_stream();
            return;
        }
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
            // The prefix twice sends it to the shell, as in tmux.
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(t) = self.focused_tab_mut().and_then(Tab::terminal_mut) {
                    t.send(&[0x02]);
                }
            }
            KeyCode::Char('c') => self.open_shell(),
            KeyCode::Char('N') => self.open_new_form(),
            KeyCode::Char('s') => self.mode = Mode::AppSettings,
            KeyCode::Char('e') => self.open_settings_form(),
            KeyCode::Char('n') => self.cycle_tab(1),
            KeyCode::Char('p') => self.cycle_tab(-1),
            KeyCode::Tab => {
                if let Some(s) = self.focused_session_mut() {
                    s.cycle_stream(1);
                }
            }
            KeyCode::BackTab => {
                if let Some(s) = self.focused_session_mut() {
                    s.cycle_stream(-1);
                }
            }
            KeyCode::Char(c @ '1'..='9') => self.select_tab(c as usize - '1' as usize),
            KeyCode::Char('j') | KeyCode::Down => self.cycle_endpoint(1),
            KeyCode::Char('k') | KeyCode::Up => self.cycle_endpoint(-1),
            KeyCode::Char('X') => {
                if let Some(tab) = self.focused_tab_mut() {
                    let id = tab.id();
                    self.request_close(Target::Tab(id));
                }
            }
            KeyCode::Char('D') => {
                if let Some(endpoint) = self.endpoints.get(self.active) {
                    self.request_close(Target::Endpoint(endpoint.id));
                }
            }
            KeyCode::Char('W') => {
                if let Some(endpoint) = self.endpoints.get(self.active) {
                    self.open_rename(Target::Endpoint(endpoint.id));
                }
            }
            KeyCode::Char('T') => {
                if let Some(tab) = self.focused_tab_mut() {
                    let id = tab.id();
                    self.open_rename(Target::Tab(id));
                }
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('q') => self.request_quit(),
            _ => {}
        }
    }

    fn settings_key(&mut self, mut form: SettingsForm, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => match form.check(&self.endpoints) {
                Ok(applied) if self.restart_drops_sessions(form.target, &applied.config) => {
                    self.mode = Mode::Confirm(Confirm::Restart(form, applied));
                    return;
                }
                Ok(applied) => {
                    self.apply(form.target, applied);
                    return;
                }
                Err(e) => form.error = Some(e),
            },
            KeyCode::Tab | KeyCode::Down => form.focus_next(1),
            KeyCode::BackTab | KeyCode::Up => form.focus_next(-1),
            KeyCode::Left if form.field == Field::Protocol => form.cycle_protocol(-1),
            KeyCode::Right | KeyCode::Char(' ') if form.field == Field::Protocol => {
                form.cycle_protocol(1)
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
        let len = menu.items.len();
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => {
                self.activate_menu_item(&menu, menu.highlighted);
                return;
            }
            KeyCode::Down => menu.highlighted = wrap(menu.highlighted, 1, len),
            KeyCode::Up => menu.highlighted = wrap(menu.highlighted, -1, len),
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
                    Target::Endpoint(id) => {
                        if let Some(endpoint) = self.endpoint_mut(id) {
                            endpoint.name = custom(endpoint.addr_label());
                        }
                    }
                    Target::Tab(id) => {
                        if let Some(tab) = self.tab_mut(id) {
                            tab.set_name(custom(tab.default_label()));
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
        // A divider drag follows the mouse until the button is released.
        if let Some(drag) = self.drag {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    match drag {
                        Drag::SidebarWidth => self.set_sidebar_width(pos.x),
                        Drag::SidebarSplit => self.set_sidebar_split(pos.y),
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
        let left_click = mouse.kind == MouseEventKind::Down(MouseButton::Left);
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Help => self.mode = Mode::Help,
            Mode::AppSettings if left_click && self.hits.cancel.contains(pos) => {}
            Mode::AppSettings => self.mode = Mode::AppSettings,
            Mode::Rename(mut form) => {
                if left_click && let Some((TextTarget::Rename, cursor)) = self.text_hit(pos) {
                    form.input.set_cursor(cursor);
                }
                self.mode = Mode::Rename(form);
            }
            Mode::Confirm(confirm) if left_click && self.hits.confirm.contains(pos) => {
                self.confirmed(confirm)
            }
            Mode::Confirm(confirm) if left_click && self.hits.cancel.contains(pos) => {
                self.cancelled(confirm)
            }
            Mode::Confirm(confirm) => self.mode = Mode::Confirm(confirm),
            Mode::Settings(mut form) => {
                if left_click {
                    self.settings_click(&mut form, pos);
                }
                self.mode = Mode::Settings(form);
            }
            Mode::Menu(menu) => self.menu_mouse(menu, mouse.kind, pos),
            mode @ (Mode::Normal | Mode::Prefix) => {
                self.mode = mode;
                self.normal_mouse(mouse.kind, pos);
            }
        }
    }

    /// The text field at `pos`, and where a click there puts the cursor.
    fn text_hit(&self, pos: Position) -> Option<(TextTarget, usize)> {
        self.hits
            .text
            .iter()
            .find(|(r, ..)| r.contains(pos))
            .map(|&(r, target, scroll)| (target, scroll + (pos.x - r.x) as usize))
    }

    fn settings_click(&self, form: &mut SettingsForm, pos: Position) {
        if let Some((target, cursor)) = self.text_hit(pos) {
            if let Some(field) = Field::for_text(target) {
                form.field = field;
                if let Some(input) = form.input_mut() {
                    input.set_cursor(cursor);
                }
            }
            return;
        }
        match hit(&self.hits.form, pos) {
            Some(FormHit::Protocol(protocol)) => {
                form.protocol = protocol;
                form.field = Field::Protocol;
            }
            Some(FormHit::Cert(cert)) => {
                form.cert = cert;
                form.field = Field::Cert;
            }
            Some(FormHit::Field(field)) => form.field = field,
            None => {}
        }
    }

    fn menu_mouse(&mut self, mut menu: ContextMenu, kind: MouseEventKind, pos: Position) {
        let item = hit(&self.hits.menu_items, pos);
        match kind {
            // Any click picks the item under it, or dismisses the menu.
            MouseEventKind::Down(_) => {
                if let Some(item) = item {
                    self.activate_menu_item(&menu, item);
                }
                return;
            }
            MouseEventKind::Moved => {
                if let Some(item) = item {
                    menu.highlighted = item;
                }
            }
            _ => {}
        }
        self.mode = Mode::Menu(menu);
    }

    fn normal_mouse(&mut self, kind: MouseEventKind, pos: Position) {
        match kind {
            MouseEventKind::Down(MouseButton::Right) => {
                self.mode = match self.menu_target(pos) {
                    Some(target) => Mode::Menu(self.context_menu(target, pos)),
                    None => Mode::Normal,
                };
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.mode = Mode::Normal;
                self.left_click(pos);
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

    /// What right-clicking at `pos` opens a menu for. Endpoint rows and tabs
    /// have menus; the sessions list doesn't.
    fn menu_target(&self, pos: Position) -> Option<Target> {
        if let Some(index) = hit(&self.hits.endpoints, pos) {
            return self
                .endpoints
                .get(index)
                .map(|endpoint| Target::Endpoint(endpoint.id));
        }
        let tab = hit(&self.hits.tabs, pos)?;
        let tab = self.endpoints.get(self.active)?.tabs.get(tab)?;
        Some(Target::Tab(tab.id()))
    }

    fn left_click(&mut self, pos: Position) {
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
                self.set_sidebar_width(pos.x);
            }
        } else if self.hits.section_divider.contains(pos) {
            self.drag = Some(Drag::SidebarSplit);
            self.set_sidebar_split(pos.y);
        } else if self.hits.new_listener.contains(pos) {
            self.open_new_form();
        } else if self.hits.new_tab.contains(pos) {
            self.open_shell();
        } else if self.hits.menu.contains(pos) {
            self.open_global_menu();
        } else if let Some(index) = hit(&self.hits.endpoints, pos) {
            if index < self.endpoints.len() {
                self.active = index;
            }
        } else if let Some((index, tab)) = hit(&self.hits.sessions, pos) {
            if let Some(endpoint) = self.endpoints.get_mut(index)
                && tab < endpoint.tabs.len()
            {
                endpoint.active_tab = tab;
                self.active = index;
            }
        } else if let Some(tab) = hit(&self.hits.tabs, pos) {
            self.select_tab(tab);
        } else if self.hits.send_stream.contains(pos) {
            if let Some(s) = self.focused_session_mut() {
                s.cycle_stream(1);
            }
        } else if self.hits.finish_stream.contains(pos) {
            if let Some(s) = self.focused_session_mut() {
                s.finish_stream();
            }
        } else if let Some((TextTarget::Session, cursor)) = self.text_hit(pos)
            && let Some(s) = self.focused_session_mut()
        {
            s.input.set_cursor(cursor);
        }
    }

    /// Puts the sidebar's right edge (its separator) at `column`.
    fn set_sidebar_width(&mut self, column: u16) {
        self.sidebar_width = (column + 1).clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
    }

    /// Puts the endpoints/sessions divider at `row`.
    fn set_sidebar_split(&mut self, row: u16) {
        let sidebar = self.hits.sidebar_divider;
        if sidebar.height > 0 {
            let offset = row.saturating_sub(sidebar.y) as f32;
            self.sidebar_split = (offset / sidebar.height as f32).clamp(0.1, 0.9);
        }
    }

    /// `index` is into `menu.items`.
    fn activate_menu_item(&mut self, menu: &ContextMenu, index: usize) {
        let Some(&item) = menu.items.get(index) else {
            return;
        };
        match (item, menu.target) {
            (MenuItem::Rename, Some(target)) => self.open_rename(target),
            (MenuItem::Close, Some(target)) => self.request_close(target),
            (MenuItem::ListenerSettings, Some(Target::Endpoint(id))) => {
                if let Some(listener) = self.endpoint(id).and_then(Endpoint::listener) {
                    self.mode = Mode::Settings(SettingsForm::edit(id, listener));
                }
            }
            (MenuItem::Settings, _) => self.mode = Mode::AppSettings,
            (MenuItem::Keybinds, _) => self.mode = Mode::Help,
            (MenuItem::Quit, _) => self.request_quit(),
            _ => {}
        }
    }

    /// A right-click menu at `pos` for `target`. Listeners also have settings.
    fn context_menu(&self, target: Target, pos: Position) -> ContextMenu {
        let listener = match target {
            Target::Endpoint(id) => self.endpoint(id).and_then(Endpoint::listener).is_some(),
            Target::Tab(_) => false,
        };
        let items = if listener {
            vec![
                MenuItem::Rename,
                MenuItem::ListenerSettings,
                MenuItem::Close,
            ]
        } else {
            vec![MenuItem::Rename, MenuItem::Close]
        };
        ContextMenu {
            target: Some(target),
            items,
            x: pos.x,
            y: pos.y,
            highlighted: 0,
        }
    }

    /// global menu, right-aligned just above the footer's `menu`.
    fn open_global_menu(&mut self) {
        let mut menu = ContextMenu {
            target: None,
            items: vec![MenuItem::Settings, MenuItem::Keybinds, MenuItem::Quit],
            x: 0,
            y: 0,
            highlighted: 0,
        };
        let (width, height) = menu.size();
        let launcher = self.hits.menu;
        menu.x = launcher.right().saturating_sub(width);
        menu.y = launcher.y.saturating_sub(height);
        self.mode = Mode::Menu(menu);
    }

    fn open_rename(&mut self, target: Target) {
        let label = match target {
            Target::Endpoint(id) => self.endpoint(id).map(Endpoint::label),
            Target::Tab(id) => self.tab(id).map(Tab::label),
        };
        if let Some(label) = label {
            self.mode = Mode::Rename(RenameForm {
                target,
                input: label.into(),
            });
        }
    }

    /// (endpoint index, tab index) of a tab.
    fn locate_tab(&self, id: SessId) -> Option<(usize, usize)> {
        self.endpoints
            .iter()
            .enumerate()
            .find_map(|(index, endpoint)| {
                endpoint
                    .tabs
                    .iter()
                    .position(|tab| tab.id() == id)
                    .map(|tab| (index, tab))
            })
    }

    /// Opens a shell tab under the local endpoint and focuses it.
    fn open_shell(&mut self) {
        let id = net::next_id();
        let (rows, cols) = self.terminal_size;
        let pty = Pty::shell(id, rows, cols, self.events.clone());
        self.add_terminal(Terminal::new(id, pty::shell_name(), pty, rows, cols));
    }

    /// Adds a terminal tab to the local endpoint and focuses it.
    fn add_terminal(&mut self, terminal: Terminal) {
        let index = self.local_index();
        let endpoint = &mut self.endpoints[index];
        endpoint.tabs.push(Tab::Terminal(terminal));
        endpoint.active_tab = endpoint.tabs.len() - 1;
        self.active = index;
    }

    /// The local endpoint, which comes first in the sidebar, created if
    /// there isn't one yet.
    fn local_index(&mut self) -> usize {
        match self
            .endpoints
            .iter()
            .position(|endpoint| matches!(endpoint.kind, Kind::Local))
        {
            Some(index) => index,
            None => {
                self.endpoints.insert(0, Endpoint::new_local());
                0
            }
        }
    }

    /// The host endpoint for the machine `dest` names, created if there
    /// isn't one yet.
    fn ssh_index(&mut self, dest: &detect::Destination) -> usize {
        let existing = self.endpoints.iter().position(
            |endpoint| matches!(&endpoint.kind, Kind::Ssh(host) if host.same_machine(dest)),
        );
        existing.unwrap_or_else(|| {
            self.endpoints.push(Endpoint::new_ssh(dest.clone()));
            self.endpoints.len() - 1
        })
    }

    /// Checks what each shell is running, starts working out where newly
    /// recognized commands connect, and files the shells accordingly.
    fn detect(&mut self) {
        let mut new = Vec::new();
        for t in self
            .endpoints
            .iter_mut()
            .flat_map(|endpoint| endpoint.tabs.iter_mut())
            .filter_map(Tab::terminal_mut)
        {
            if let Some((integration, argv)) = t.poll() {
                new.push((t.id, integration, argv));
            }
        }
        for (tab, integration, argv) in new {
            detect::spawn_resolve(tab, integration, argv, self.events.clone());
        }
        self.place_terminals();
    }

    fn on_resolved(&mut self, Resolved { tab, argv, dest }: Resolved) {
        if let Some(t) = self.terminal_mut(tab) {
            t.resolved(&argv, dest);
            self.place_terminals();
        }
    }

    /// Moves each running shell under the host it's connected to, or back to
    /// local, then drops host endpoints with no shells left. Focus follows
    /// the focused tab, and otherwise stays on the same endpoint.
    fn place_terminals(&mut self) {
        let mut moves = Vec::new();
        for endpoint in &self.endpoints {
            for t in endpoint.tabs.iter().filter_map(|tab| match tab {
                Tab::Terminal(t) if t.is_open() => Some(t),
                _ => None,
            }) {
                let placed = match (&endpoint.kind, t.destination()) {
                    (Kind::Local, None) => true,
                    (Kind::Ssh(host), Some(dest)) => host.same_machine(dest),
                    _ => false,
                };
                if !placed {
                    moves.push((t.id, t.destination().cloned()));
                }
            }
        }
        if moves.is_empty() {
            return;
        }

        let focused_tab = self.focused_tab_mut().map(|tab| tab.id());
        let focused_endpoint = self.endpoints.get(self.active).map(|endpoint| endpoint.id);
        for (id, dest) in moves {
            let Some(tab) = self
                .locate_tab(id)
                .and_then(|(endpoint, index)| self.take_tab(endpoint, index))
            else {
                continue;
            };
            let index = match &dest {
                Some(dest) => self.ssh_index(dest),
                None => self.local_index(),
            };
            let endpoint = &mut self.endpoints[index];
            endpoint.tabs.push(tab);
            if Some(id) == focused_tab {
                endpoint.active_tab = endpoint.tabs.len() - 1;
            }
        }
        self.endpoints
            .retain(|endpoint| !matches!(endpoint.kind, Kind::Ssh(_)) || !endpoint.tabs.is_empty());

        if let Some((endpoint, tab)) = focused_tab.and_then(|id| self.locate_tab(id)) {
            self.active = endpoint;
            self.endpoints[endpoint].active_tab = tab;
        } else if let Some(index) = focused_endpoint
            .and_then(|id| self.endpoints.iter().position(|endpoint| endpoint.id == id))
        {
            self.active = index;
        }
        self.active = self.active.min(self.endpoints.len().saturating_sub(1));
    }

    /// Sizes every terminal tab to `area`, the pane inside its border. Every
    /// tab shares the pane, so a hidden shell is ready when it's shown.
    pub fn fit_terminals(&mut self, area: Rect) {
        self.terminal_size = (area.height, area.width);
        for t in self
            .endpoints
            .iter_mut()
            .flat_map(|endpoint| endpoint.tabs.iter_mut())
            .filter_map(Tab::terminal_mut)
        {
            t.resize(area.height, area.width);
        }
    }

    fn open_new_form(&mut self) {
        self.mode = Mode::Settings(SettingsForm::new(&self.endpoints));
    }

    fn open_settings_form(&mut self) {
        let active = self.endpoints.get(self.active);
        let form = match active.and_then(|endpoint| Some((endpoint.id, endpoint.listener()?))) {
            Some((id, listener)) => SettingsForm::edit(id, listener),
            None => SettingsForm::new(&self.endpoints),
        };
        self.mode = Mode::Settings(form);
    }

    /// Whether applying `config` to endpoint `target` would drop open sessions.
    fn restart_drops_sessions(&self, target: Option<EndpointId>, config: &ListenerConfig) -> bool {
        target
            .and_then(|id| self.endpoint(id))
            .is_some_and(|endpoint| endpoint.restart_drops_sessions(config))
    }

    /// Creates a listener (`target` is `None`) or applies new settings to one,
    /// restarting it if anything changed.
    fn apply(&mut self, target: Option<EndpointId>, Applied { config, tls }: Applied) {
        match target {
            None => {
                let mut endpoint = Endpoint::new_listener(config, tls);
                endpoint.start(&self.events);
                self.endpoints.push(endpoint);
                self.active = self.endpoints.len() - 1;
            }
            Some(id) => {
                let events = self.events.clone();
                let Some(endpoint) = self.endpoint_mut(id) else {
                    return;
                };
                if endpoint
                    .listener()
                    .is_some_and(|listener| listener.restarts_for(&config))
                {
                    endpoint.stop();
                    if let Some(listener) = endpoint.listener_mut() {
                        listener.config = config;
                        listener.tls = tls;
                    }
                    endpoint.start(&events);
                }
            }
        }
    }

    fn cycle_tab(&mut self, delta: isize) {
        if let Some(endpoint) = self.endpoints.get_mut(self.active)
            && !endpoint.tabs.is_empty()
        {
            endpoint.active_tab = wrap(endpoint.active_tab, delta, endpoint.tabs.len());
        }
    }

    fn select_tab(&mut self, index: usize) {
        if let Some(endpoint) = self.endpoints.get_mut(self.active)
            && index < endpoint.tabs.len()
        {
            endpoint.active_tab = index;
        }
    }

    fn cycle_endpoint(&mut self, delta: isize) {
        if !self.endpoints.is_empty() {
            self.active = wrap(self.active, delta, self.endpoints.len());
        }
    }

    /// Disconnects (if still open) and removes a tab.
    fn remove_tab(&mut self, endpoint: usize, index: usize) {
        self.take_tab(endpoint, index);
    }

    /// Takes a tab out of its endpoint, keeping that endpoint on the same
    /// tab when another one goes.
    fn take_tab(&mut self, endpoint: usize, index: usize) -> Option<Tab> {
        let endpoint = self.endpoints.get_mut(endpoint)?;
        if index >= endpoint.tabs.len() {
            return None;
        }
        let tab = endpoint.tabs.remove(index);
        if index < endpoint.active_tab {
            endpoint.active_tab -= 1;
        }
        endpoint.active_tab = endpoint
            .active_tab
            .min(endpoint.tabs.len().saturating_sub(1));
        Some(tab)
    }

    /// Closes an endpoint or tab, asking first if that would drop live connections.
    fn request_close(&mut self, target: Target) {
        let live = match target {
            Target::Endpoint(id) => self
                .endpoint(id)
                .is_some_and(|endpoint| endpoint.open_count() > 0),
            Target::Tab(id) => self.tab(id).is_some_and(Tab::is_open),
        };
        if live {
            self.mode = Mode::Confirm(Confirm::Close(target));
        } else {
            self.close_target(target);
        }
    }

    /// Quits, asking first if that would drop live connections.
    fn request_quit(&mut self) {
        if self
            .endpoints
            .iter()
            .any(|endpoint| endpoint.open_count() > 0)
        {
            self.mode = Mode::Confirm(Confirm::Quit);
        } else {
            self.quit = true;
        }
    }

    fn confirmed(&mut self, confirm: Confirm) {
        match confirm {
            Confirm::Close(target) => self.close_target(target),
            Confirm::Restart(form, applied) => self.apply(form.target, applied),
            Confirm::Quit => self.quit = true,
        }
    }

    fn cancelled(&mut self, confirm: Confirm) {
        match confirm {
            Confirm::Close(_) | Confirm::Quit => {}
            Confirm::Restart(form, _) => self.mode = Mode::Settings(form),
        }
    }

    /// Closes without asking, dropping any connections.
    fn close_target(&mut self, target: Target) {
        match target {
            Target::Endpoint(id) => {
                if let Some(index) = self.endpoints.iter().position(|endpoint| endpoint.id == id) {
                    self.remove_endpoint(index);
                }
            }
            Target::Tab(id) => {
                if let Some((endpoint, index)) = self.locate_tab(id) {
                    self.remove_tab(endpoint, index);
                }
            }
        }
    }

    /// Removes an endpoint (stopping its task and sessions), keeping focus
    /// on the same endpoint when another one is removed.
    fn remove_endpoint(&mut self, index: usize) {
        if index >= self.endpoints.len() {
            return;
        }
        self.endpoints.remove(index);
        if index < self.active {
            self.active -= 1;
        }
        self.active = self.active.min(self.endpoints.len().saturating_sub(1));
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
impl App {
    /// An app with no listeners. The receiver gets events from any listener the app starts.
    pub fn test_new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self::new(tx), rx)
    }

    /// Adds a listener that is already listening, with no socket behind it.
    pub fn add_test_listener(&mut self, protocol: crate::net::Protocol, addr: &str) -> EndpointId {
        let config = ListenerConfig {
            protocol,
            addr: addr.parse().expect("test address"),
            tls: crate::net::tls::TlsConfig::default(),
        };
        let mut endpoint = Endpoint::new_listener(config, None);
        if let Some(listener) = endpoint.listener_mut() {
            listener.state = ListenState::Listening;
        }
        let id = endpoint.id;
        self.endpoints.push(endpoint);
        id
    }

    /// The listener behind endpoint `index`.
    pub fn test_listener(&self, index: usize) -> &Listener {
        self.endpoints[index].listener().expect("a listener")
    }

    pub fn test_listener_mut(&mut self, index: usize) -> &mut Listener {
        self.endpoints[index].listener_mut().expect("a listener")
    }

    /// The transcript session in tab `tab` of endpoint `index`.
    pub fn test_session(&self, index: usize, tab: usize) -> &Session {
        self.endpoints[index].tabs[tab]
            .session()
            .expect("a session")
    }

    pub fn test_session_mut(&mut self, index: usize, tab: usize) -> &mut Session {
        self.endpoints[index].tabs[tab]
            .session_mut()
            .expect("a session")
    }

    /// Accepts a TCP session into `listener`, as the listener would. The receiver gets
    /// every chunk the app sends to that peer; it reports disconnected once the
    /// app drops the session.
    pub fn add_test_session(
        &mut self,
        listener: EndpointId,
        peer: &str,
    ) -> (SessId, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let sess = net::next_id();
        self.handle(Event::Net(NetEvent::Accepted {
            listener,
            sess,
            peer: peer.parse().expect("test peer"),
            outlet: Outlet::Tcp(tx),
        }));
        (sess, rx)
    }

    /// Opens a shell tab with no program behind it, as `ctrl+b c` would. The
    /// receiver gets every write to the shell.
    pub fn add_test_terminal(&mut self) -> (SessId, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
        let (pty, rx) = Pty::test();
        let id = net::next_id();
        let (rows, cols) = self.terminal_size;
        self.add_terminal(Terminal::new(id, "sh".into(), Ok(pty), rows, cols));
        (id, rx)
    }

    /// Has the shell in `tab` run `argv`, as the next check would see it.
    pub fn test_observe(&mut self, tab: SessId, argv: Vec<String>) {
        if let Some(t) = self.terminal_mut(tab) {
            t.observe(Some(argv));
        }
    }

    /// Like [`App::add_test_session`] for a QUIC connection: the receiver gets
    /// each write and finish the app asks for.
    pub fn add_test_quic_session(
        &mut self,
        listener: EndpointId,
        peer: &str,
    ) -> (
        SessId,
        tokio::sync::mpsc::UnboundedReceiver<crate::net::quic::QuicOut>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let sess = net::next_id();
        self.handle(Event::Net(NetEvent::Accepted {
            listener,
            sess,
            peer: peer.parse().expect("test peer"),
            outlet: Outlet::Quic(tx),
        }));
        (sess, rx)
    }
}
