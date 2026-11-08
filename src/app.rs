//! Application state and input handling. The workspace, session, and
//! settings-form types live in submodules; this module owns `App`, its
//! overlays (`Mode`), and how keys and clicks drive them.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use tokio::sync::mpsc::UnboundedSender;

use crate::Event;
use crate::net::{self, NetEvent, Outlet, SessId, WsId};
use crate::text_input::TextInput;
use crate::ui::{FormHit, Hits, TextTarget};

mod session;
mod settings;
mod workspace;

pub use session::{Dir, Session};
pub use settings::{Applied, CertKind, Field, SettingsForm};
pub use workspace::{ListenState, ListenerConfig, Status, Workspace};

/// Sidebar width limits, matching herdr's defaults.
pub const DEFAULT_SIDEBAR_WIDTH: u16 = 26;
const MIN_SIDEBAR_WIDTH: u16 = 18;
const MAX_SIDEBAR_WIDTH: u16 = 36;
/// Share of the sidebar given to workspaces; sessions get the rest.
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
    Workspace(WsId),
    /// A tab.
    Session(SessId),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuItem {
    Rename,
    Close,
}

impl MenuItem {
    /// In display order.
    pub const ALL: [MenuItem; 2] = [MenuItem::Rename, MenuItem::Close];

    pub fn label(self) -> &'static str {
        match self {
            MenuItem::Rename => "Rename",
            MenuItem::Close => "Close",
        }
    }
}

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
    /// Asks before an action that would drop live connections.
    Confirm(Confirm),
}

impl Mode {
    /// The workspace or tab the open overlay acts on, highlighted while it's open.
    pub fn target(&self) -> Option<Target> {
        match self {
            Mode::Menu(menu) => Some(menu.target),
            Mode::Rename(form) => Some(form.target),
            Mode::Confirm(Confirm::Close(target)) => Some(*target),
            Mode::Confirm(Confirm::Restart(form, _)) => form.target.map(Target::Workspace),
            Mode::Normal
            | Mode::Prefix
            | Mode::Help
            | Mode::Settings(_)
            | Mode::Confirm(Confirm::Quit) => None,
        }
    }
}

/// An action waiting on a confirmation, because it drops live connections.
// Only one exists at a time, and `Mode::Settings` holds a form unboxed too.
#[expect(clippy::large_enum_variant)]
pub enum Confirm {
    /// Closing a workspace or tab.
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

    pub fn workspace(&self, id: WsId) -> Option<&Workspace> {
        self.workspaces.iter().find(|ws| ws.id == id)
    }

    pub fn session(&self, id: SessId) -> Option<&Session> {
        self.workspaces
            .iter()
            .flat_map(|ws| &ws.sessions)
            .find(|s| s.id == id)
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
            KeyCode::Char('N') => self.open_new_form(),
            KeyCode::Char('s') => self.open_settings_form(),
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
            KeyCode::Char('q') => self.request_quit(),
            _ => {}
        }
    }

    fn settings_key(&mut self, mut form: SettingsForm, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => match form.check(&self.workspaces) {
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
        let len = MenuItem::ALL.len();
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => {
                self.activate_menu_item(menu.target, menu.highlighted);
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
                    self.activate_menu_item(menu.target, item);
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
                    Some(target) => Mode::Menu(ContextMenu {
                        target,
                        x: pos.x,
                        y: pos.y,
                        highlighted: 0,
                    }),
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

    /// What right-clicking at `pos` opens a menu for. Workspace rows and tabs
    /// have menus; the sessions list doesn't.
    fn menu_target(&self, pos: Position) -> Option<Target> {
        if let Some(ws) = hit(&self.hits.workspaces, pos) {
            return self.workspaces.get(ws).map(|ws| Target::Workspace(ws.id));
        }
        let tab = hit(&self.hits.tabs, pos)?;
        let s = self.workspaces.get(self.active)?.sessions.get(tab)?;
        Some(Target::Session(s.id))
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
        } else if self.hits.new_workspace.contains(pos) {
            self.open_new_form();
        } else if self.hits.settings.contains(pos) {
            self.open_settings_form();
        } else if let Some(ws) = hit(&self.hits.workspaces, pos) {
            if ws < self.workspaces.len() {
                self.active = ws;
            }
        } else if let Some((ws, tab)) = hit(&self.hits.sessions, pos) {
            if let Some(w) = self.workspaces.get_mut(ws)
                && tab < w.sessions.len()
            {
                w.active_tab = tab;
                self.active = ws;
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

    /// Puts the workspaces/sessions divider at `row`.
    fn set_sidebar_split(&mut self, row: u16) {
        let sidebar = self.hits.sidebar_divider;
        if sidebar.height > 0 {
            let offset = row.saturating_sub(sidebar.y) as f32;
            self.sidebar_split = (offset / sidebar.height as f32).clamp(0.1, 0.9);
        }
    }

    /// `index` is into [`MenuItem::ALL`].
    fn activate_menu_item(&mut self, target: Target, index: usize) {
        match MenuItem::ALL.get(index) {
            Some(MenuItem::Rename) => self.open_rename(target),
            Some(MenuItem::Close) => self.request_close(target),
            None => {}
        }
    }

    fn open_rename(&mut self, target: Target) {
        let label = match target {
            Target::Workspace(id) => self.workspace(id).map(Workspace::label),
            Target::Session(id) => self.session(id).map(Session::label),
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
        self.mode = Mode::Settings(SettingsForm::new(&self.workspaces));
    }

    fn open_settings_form(&mut self) {
        let form = match self.workspaces.get(self.active) {
            Some(ws) => SettingsForm::edit(ws),
            None => SettingsForm::new(&self.workspaces),
        };
        self.mode = Mode::Settings(form);
    }

    /// Whether applying `config` to workspace `target` would drop open sessions.
    fn restart_drops_sessions(&self, target: Option<WsId>, config: &ListenerConfig) -> bool {
        target
            .and_then(|id| self.workspace(id))
            .is_some_and(|ws| ws.restart_drops_sessions(config))
    }

    /// Creates a workspace (`target` is `None`) or applies new settings to one,
    /// restarting its listener if anything changed.
    fn apply(&mut self, target: Option<WsId>, Applied { config, tls }: Applied) {
        match target {
            None => {
                let mut ws = Workspace::new(config, tls);
                ws.start(&self.events);
                self.workspaces.push(ws);
                self.active = self.workspaces.len() - 1;
            }
            Some(id) => {
                let events = self.events.clone();
                let Some(ws) = self.workspace_mut(id) else {
                    return;
                };
                if ws.restarts_for(&config) {
                    ws.stop();
                    ws.config = config;
                    ws.tls = tls;
                    ws.start(&events);
                }
            }
        }
    }

    fn cycle_tab(&mut self, delta: isize) {
        if let Some(ws) = self.workspaces.get_mut(self.active)
            && !ws.sessions.is_empty()
        {
            ws.active_tab = wrap(ws.active_tab, delta, ws.sessions.len());
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
        if !self.workspaces.is_empty() {
            self.active = wrap(self.active, delta, self.workspaces.len());
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
            Target::Workspace(id) => self.workspace(id).is_some_and(|w| w.open_count() > 0),
            Target::Session(id) => self.session(id).is_some_and(Session::is_open),
        };
        if live {
            self.mode = Mode::Confirm(Confirm::Close(target));
        } else {
            self.close_target(target);
        }
    }

    /// Quits, asking first if that would drop live connections.
    fn request_quit(&mut self) {
        if self.workspaces.iter().any(|ws| ws.open_count() > 0) {
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
    pub fn add_test_workspace(&mut self, protocol: crate::net::Protocol, addr: &str) -> WsId {
        let config = ListenerConfig {
            protocol,
            addr: addr.parse().expect("test address"),
            tls: crate::net::tls::TlsConfig::default(),
        };
        let mut ws = Workspace::new(config, None);
        ws.state = ListenState::Listening;
        let id = ws.id;
        self.workspaces.push(ws);
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

    /// Like [`App::add_test_session`] for a QUIC connection: the receiver gets
    /// each write and finish the app asks for.
    pub fn add_test_quic_session(
        &mut self,
        ws: WsId,
        peer: &str,
    ) -> (
        SessId,
        tokio::sync::mpsc::UnboundedReceiver<crate::net::quic::QuicOut>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let sess = net::next_id();
        self.handle(Event::Net(NetEvent::Accepted {
            ws,
            sess,
            peer: peer.parse().expect("test peer"),
            outlet: Outlet::Quic(tx),
        }));
        (sess, rx)
    }
}
