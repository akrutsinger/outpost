//! Rendering, laid out after herdr: sidebar (endpoints over sessions), tab bar, pane.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};

use crate::app::{App, CertKind, Field, Mode, Status};
use crate::net::Protocol;
use crate::text_input::TextInput;
use crate::theme::*;
use overlays::{render_confirm, render_help, render_menu, render_rename, render_settings};
use sidebar::render_sidebar;
use tabs::{render_pane, render_tab_bar};

mod overlays;
mod sidebar;
mod tabs;
#[cfg(test)]
mod tests;

/// A clickable part of the settings form.
#[derive(Clone, Copy)]
pub enum FormHit {
    Protocol(Protocol),
    Cert(CertKind),
    /// Focus a field.
    Field(Field),
}

/// A text field that a click can place the cursor in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextTarget {
    Session,
    Address,
    Port,
    CertFile,
    KeyFile,
    Alpn,
    Rename,
}

/// Clickable regions from the last frame.
#[derive(Default)]
pub struct Hits {
    pub endpoints: Vec<(Rect, usize)>,
    /// (row, (endpoint index, tab index))
    pub sessions: Vec<(Rect, (usize, usize))>,
    pub tabs: Vec<(Rect, usize)>,
    pub new_listener: Rect,
    pub settings: Rect,
    pub pane: Rect,
    /// The sidebar's vertical separator (full sidebar height).
    pub sidebar_divider: Rect,
    /// The line between the endpoints and sessions sections.
    pub section_divider: Rect,
    /// Right-click menu rows: (row, index into `MenuItem::ALL`).
    pub menu_items: Vec<(Rect, usize)>,
    /// Settings form targets; buttons come before the rows they sit in.
    pub form: Vec<(Rect, FormHit)>,
    /// Text fields: (area, field, index of the first visible char).
    pub text: Vec<(Rect, TextTarget, usize)>,
    /// QUIC sessions: the stream the input goes to (click to cycle), and the
    /// button that finishes it.
    pub send_stream: Rect,
    pub finish_stream: Rect,
    /// Close-confirmation buttons.
    pub confirm: Rect,
    pub cancel: Rect,
}

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let mut hits = Hits::default();
    let sidebar_width = app.sidebar_width.min(area.width / 2);
    let sidebar = Rect {
        width: sidebar_width,
        ..area
    };
    let main = Rect {
        x: area.x + sidebar_width,
        width: area.width - sidebar_width,
        ..area
    };

    let buf = frame.buffer_mut();
    render_sidebar(buf, sidebar, app, &mut hits);
    let mut cursor = None;
    if main.height > 1 {
        let tab_bar = Rect { height: 1, ..main };
        let pane = Rect {
            y: main.y + 1,
            height: main.height - 1,
            ..main
        };
        render_tab_bar(buf, tab_bar, app, &mut hits);
        cursor = render_pane(buf, pane, app, &mut hits);
        if matches!(app.mode, Mode::Prefix) {
            render_prefix_bar(buf, pane);
        }
    }
    match &app.mode {
        Mode::Settings(form) => {
            let fingerprint = form
                .target
                .and_then(|id| app.endpoint(id))
                .and_then(|endpoint| endpoint.tls.as_ref())
                .map(|tls| tls.fingerprint.as_str());
            cursor = render_settings(buf, area, form, fingerprint, &mut hits);
        }
        Mode::Rename(form) => cursor = render_rename(buf, area, form, &mut hits),
        Mode::Confirm(confirm) => {
            render_confirm(buf, area, app, confirm, &mut hits);
            cursor = None;
        }
        Mode::Menu(menu) => {
            render_menu(buf, area, menu, &mut hits);
            cursor = None;
        }
        Mode::Help => {
            render_help(buf, area);
            cursor = None;
        }
        Mode::Normal | Mode::Prefix => {}
    }
    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
    app.hits = hits;
}

/// Writes `text` from `x` up to (not including) `right`; returns the end column.
fn put(buf: &mut Buffer, x: u16, y: u16, right: u16, text: &str, style: Style) -> u16 {
    if x >= right || y >= buf.area.bottom() {
        return x;
    }
    buf.set_stringn(x, y, text, (right - x) as usize, style).0
}

fn put_right(buf: &mut Buffer, area: Rect, y: u16, text: &str, style: Style) {
    let width = (text.chars().count() as u16).min(area.width);
    put(buf, area.right() - width, y, area.right(), text, style);
}

/// Draws `input` on the one-row `area`, scrolled so its cursor is visible.
/// Records the field as a click target and returns where the cursor goes.
fn render_input(
    buf: &mut Buffer,
    area: Rect,
    input: &TextInput,
    style: Style,
    target: TextTarget,
    hits: &mut Hits,
) -> Position {
    let width = area.width as usize;
    let start = input.scroll(width);
    let visible: String = input.as_str().chars().skip(start).take(width).collect();
    put(buf, area.x, area.y, area.right(), &visible, style);
    hits.text.push((area, target, start));
    Position::new(area.x + (input.cursor() - start) as u16, area.y)
}

fn status_glyph(status: Status) -> (&'static str, Color) {
    match status {
        Status::Unread => ("●", YELLOW),
        Status::Open => ("●", GREEN),
        Status::Idle => ("○", GREEN),
        Status::Closed => ("·", OVERLAY0),
        Status::Failed => ("●", RED),
    }
}

fn human(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b}B"),
        b if b < 1024 * 1024 => format!("{:.1}K", b as f64 / 1024.0),
        b => format!("{:.1}M", b as f64 / (1024.0 * 1024.0)),
    }
}

fn render_prefix_bar(buf: &mut Buffer, pane: Rect) {
    let y = pane.bottom() - 1;
    let base = Style::new().fg(OVERLAY0).bg(PANEL_BG);
    let key = Style::new()
        .fg(ACCENT)
        .bg(PANEL_BG)
        .add_modifier(Modifier::BOLD);
    buf.set_style(
        Rect {
            y,
            height: 1,
            ..pane
        },
        base,
    );
    for x in pane.left()..pane.right() {
        buf[(x, y)].set_symbol(" ");
    }
    let mut x = put(
        buf,
        pane.x,
        y,
        pane.right(),
        " PREFIX ",
        Style::new()
            .fg(PANEL_BG)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
    );
    for (k, label) in [
        ("esc", "cancel"),
        ("N", "new"),
        ("s", "settings"),
        ("n/p", "tab"),
        ("j/k", "listener"),
        ("X", "close tab"),
        ("D", "close listener"),
        ("?", "keybinds"),
        ("q", "quit"),
    ] {
        x = put(buf, x, y, pane.right(), " ", base);
        x = put(buf, x, y, pane.right(), k, key);
        x = put(buf, x, y, pane.right(), &format!(" {label} "), base);
    }
}

/// Renders `app` into an off-screen buffer and returns each row as text.
/// Also refreshes `app.hits`, so tests can click what was drawn.
#[cfg(test)]
pub fn render_rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal.draw(|frame| render(frame, app)).expect("draw");
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect())
        .collect()
}
