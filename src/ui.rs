//! Rendering, laid out after herdr: sidebar (workspaces over sessions), tab bar, pane.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use crate::app::{
    App, CertKind, Confirm, ContextMenu, Dir, Field, ListenState, MENU_ITEMS, Mode, RenameForm,
    Session, SettingsForm, Status, Target,
};
use crate::net::{Protocol, WsId};
use crate::text_input::TextInput;
use crate::theme::*;

const MIN_TAB_WIDTH: u16 = 8;
const WORKSPACE_HEADER_ROWS: u16 = 2;

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
    pub workspaces: Vec<(Rect, usize)>,
    /// (row, workspace index, tab index)
    pub sessions: Vec<(Rect, usize, usize)>,
    pub tabs: Vec<(Rect, usize)>,
    pub new_workspace: Rect,
    pub settings: Rect,
    pub pane: Rect,
    /// The sidebar's vertical separator (full sidebar height).
    pub sidebar_divider: Rect,
    /// The line between the workspaces and sessions sections.
    pub section_divider: Rect,
    /// Right-click menu rows: (row, index into `MENU_ITEMS`).
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
                .and_then(|id| app.workspaces.iter().find(|ws| ws.id == id))
                .and_then(|ws| ws.tls.as_ref())
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

/// Draws `input` between `x` and `right`, scrolled so its cursor is visible.
/// Records the field as a click target and returns where the cursor goes.
#[allow(clippy::too_many_arguments)]
fn render_input(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    input: &TextInput,
    style: Style,
    target: TextTarget,
    hits: &mut Hits,
) -> Position {
    let width = right.saturating_sub(x);
    let start = input.scroll(width as usize);
    let visible: String = input
        .as_str()
        .chars()
        .skip(start)
        .take(width as usize)
        .collect();
    put(buf, x, y, right, &visible, style);
    hits.text.push((Rect::new(x, y, width, 1), target, start));
    Position::new(x + (input.cursor() - start) as u16, y)
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

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn header_style() -> Style {
    Style::new().fg(OVERLAY0).add_modifier(Modifier::BOLD)
}

fn render_sidebar(buf: &mut Buffer, area: Rect, app: &App, hits: &mut Hits) {
    if area.width < 2 || area.height == 0 {
        return;
    }
    let separator_x = area.right() - 1;
    hits.sidebar_divider = Rect::new(separator_x, area.y, 1, area.height);
    for y in area.top()..area.bottom() {
        buf[(separator_x, y)]
            .set_symbol("│")
            .set_style(Style::new().fg(SURFACE_DIM));
    }
    let content = Rect {
        width: area.width - 1,
        ..area
    };
    let top_height = if content.height < 6 {
        content.height.div_ceil(2)
    } else {
        ((content.height as f32 * app.sidebar_split).round() as u16).clamp(3, content.height - 3)
    };
    let top = Rect {
        height: top_height,
        ..content
    };
    let bottom = Rect {
        y: content.y + top_height,
        height: content.height - top_height,
        ..content
    };
    if content.height >= 6 {
        hits.section_divider = Rect::new(content.x, bottom.y, content.width, 1);
    }
    render_workspaces(buf, top, app, hits);
    render_sessions(buf, bottom, app, hits);
}

fn render_workspaces(buf: &mut Buffer, area: Rect, app: &App, hits: &mut Hits) {
    put(
        buf,
        area.x,
        area.y,
        area.right(),
        " workspaces",
        header_style(),
    );
    let body = Rect::new(
        area.x,
        area.y + WORKSPACE_HEADER_ROWS,
        area.width,
        area.height.saturating_sub(WORKSPACE_HEADER_ROWS + 1),
    );
    if app.workspaces.is_empty() && !body.is_empty() {
        put(
            buf,
            body.x,
            body.y,
            body.right(),
            " no listeners",
            Style::new().fg(OVERLAY0).add_modifier(Modifier::DIM),
        );
    }

    let mut y = body.y;
    for (index, ws) in app.workspaces.iter().enumerate() {
        if y + 2 > body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, body.width, 2);
        let focused = index == app.active;
        // The workspace a menu or rename is acting on stands out like a selection.
        let targeted = match &app.mode {
            Mode::Menu(menu) => menu.target == Target::Workspace(ws.id),
            Mode::Rename(form) => form.target == Target::Workspace(ws.id),
            Mode::Confirm(Confirm::Close(target)) => *target == Target::Workspace(ws.id),
            Mode::Confirm(Confirm::Restart(form, _)) => form.target == Some(ws.id),
            _ => false,
        };
        if targeted {
            buf.set_style(rect, Style::new().bg(SURFACE0));
        } else if focused {
            buf.set_style(rect, Style::new().bg(ACTIVE_ROW_BG));
        }
        let right = rect.right().saturating_sub(1);
        let name_style = if focused {
            Style::new().fg(TEXT).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(SUBTEXT0)
        };
        let (icon, color) = status_glyph(ws.status());
        let mut x = put(
            buf,
            rect.x + 1,
            y,
            right,
            &format!("{} ", index + 1),
            Style::new().fg(OVERLAY0),
        );
        x = put(buf, x, y, right, &ws.label(), name_style);
        put(
            buf,
            x,
            y,
            right,
            &format!(" {icon}"),
            Style::new().fg(color),
        );

        let secondary = Style::new().fg(if focused { MAUVE } else { OVERLAY0 });
        let (detail, style) = match &ws.state {
            ListenState::Failed(error) => (error.clone(), Style::new().fg(RED)),
            ListenState::Binding => ("binding…".to_string(), secondary),
            // A custom name hides the port, so show it here instead of the IP.
            ListenState::Listening if ws.name.is_some() => (
                format!("{} · {} open", ws.endpoint(), ws.open_count()),
                secondary,
            ),
            ListenState::Listening => (
                format!("{} · {} open", ws.config.addr.ip(), ws.open_count()),
                secondary,
            ),
        };
        put(buf, rect.x + 3, y + 1, right, &detail, style);
        hits.workspaces.push((rect, index));
        y += 3;
    }

    if area.height > WORKSPACE_HEADER_ROWS {
        let footer_y = area.bottom() - 1;
        hits.new_workspace = Rect::new(area.x, footer_y, 4.min(area.width), 1);
        put(
            buf,
            area.x,
            footer_y,
            area.right(),
            " new",
            Style::new().fg(OVERLAY0),
        );
        if !app.workspaces.is_empty() {
            let label = "settings";
            let width = (label.len() as u16).min(area.width);
            hits.settings = Rect::new(area.right() - width, footer_y, width, 1);
            put_right(buf, area, footer_y, label, Style::new().fg(OVERLAY0));
        }
    }
}

fn render_sessions(buf: &mut Buffer, area: Rect, app: &App, hits: &mut Hits) {
    if area.height == 0 {
        return;
    }
    put(
        buf,
        area.x,
        area.y,
        area.right(),
        &"─".repeat(area.width as usize),
        Style::new().fg(SURFACE_DIM),
    );
    if area.height < 2 {
        return;
    }
    put(
        buf,
        area.x,
        area.y + 1,
        area.right(),
        " sessions",
        header_style(),
    );
    let open: usize = app.workspaces.iter().map(|ws| ws.open_count()).sum();
    put_right(
        buf,
        area,
        area.y + 1,
        &format!("{open} open"),
        header_style(),
    );

    let body = Rect::new(
        area.x,
        area.y + 3,
        area.width,
        area.height.saturating_sub(3),
    );
    if body.is_empty() {
        return;
    }
    if open == 0 {
        put(
            buf,
            body.x,
            body.y,
            body.right(),
            " no connections",
            Style::new().fg(OVERLAY0).add_modifier(Modifier::DIM),
        );
        return;
    }

    let mut y = body.y;
    for (ws_index, ws) in app.workspaces.iter().enumerate() {
        for (tab, s) in ws.sessions.iter().enumerate().filter(|(_, s)| s.is_open()) {
            if y + 2 > body.bottom() {
                return;
            }
            let rect = Rect::new(body.x, y, body.width, 2);
            let focused = ws_index == app.active && tab == ws.active_tab;
            if focused {
                buf.set_style(rect, Style::new().bg(ACTIVE_ROW_BG));
            }
            let status = s.status();
            let (icon, color) = status_glyph(status);
            let state = status.text();
            let right = rect.right().saturating_sub(1);
            let name_style = Style::new()
                .fg(if focused { TEXT } else { SUBTEXT0 })
                .add_modifier(Modifier::BOLD);
            let x = put(
                buf,
                rect.x + 1,
                y,
                right,
                &format!("{icon} "),
                Style::new().fg(color),
            );
            put(buf, x, y, right, &s.label(), name_style);
            // Status sits on the second row: a full IPv4 peer barely fits the first.
            let state_x = right.saturating_sub(state.len() as u16);
            let detail_right = state_x.saturating_sub(1);
            // Shorten the workspace label first so the byte count stays visible.
            let bytes = format!(" · ↓{}", human(s.bytes_in));
            let room = detail_right.saturating_sub(rect.x + 3) as usize;
            let label = shorten(&ws.label(), room.saturating_sub(bytes.chars().count()));
            put(
                buf,
                rect.x + 3,
                y + 1,
                detail_right,
                &format!("{label}{bytes}"),
                Style::new().fg(OVERLAY0),
            );
            put(buf, state_x, y + 1, right, state, Style::new().fg(color));
            hits.sessions.push((rect, ws_index, tab));
            y += 2;
        }
    }
}

/// `text` cut to `width` chars, ending in `…` when cut.
fn shorten(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut short: String = text.chars().take(width.saturating_sub(1)).collect();
    if width > 0 {
        short.push('…');
    }
    short
}

fn tab_label(s: &Session) -> String {
    if s.unread {
        format!("● {}", s.label())
    } else {
        s.label()
    }
}

fn render_tab_bar(buf: &mut Buffer, area: Rect, app: &App, hits: &mut Hits) {
    buf.set_style(area, Style::new().bg(PANEL_BG));
    let Some(ws) = app.workspaces.get(app.active) else {
        return;
    };

    let status = format!("{} {}", ws.config.protocol.label(), ws.config.addr);
    let status_width = status.chars().count() as u16;
    let mut tabs_right = area.right();
    if area.width >= status_width + 1 + MIN_TAB_WIDTH * 2 {
        put_right(
            buf,
            area,
            area.y,
            &status,
            Style::new().fg(OVERLAY0).bg(PANEL_BG),
        );
        tabs_right = area.right() - status_width - 1;
    }

    let labels: Vec<String> = ws.sessions.iter().map(tab_label).collect();
    let widths: Vec<u16> = labels
        .iter()
        .map(|label| (label.chars().count() as u16 + 4).max(MIN_TAB_WIDTH))
        .collect();
    // Scroll the strip just enough to keep the focused tab visible.
    let available = tabs_right.saturating_sub(area.x);
    let span = |from: usize, to: usize| -> u16 {
        widths[from..=to].iter().sum::<u16>() + (to - from) as u16
    };
    let mut start = 0;
    while start < ws.active_tab && span(start, ws.active_tab) > available {
        start += 1;
    }

    let mut x = area.x;
    for (index, label) in labels.iter().enumerate().skip(start) {
        let desired = widths[index];
        let width = desired.min(tabs_right.saturating_sub(x));
        if width == 0 {
            break;
        }
        let s = &ws.sessions[index];
        // The tab a menu or rename is acting on stands out like a selection.
        let targeted = match &app.mode {
            Mode::Menu(menu) => menu.target == Target::Session(s.id),
            Mode::Rename(form) => form.target == Target::Session(s.id),
            Mode::Confirm(Confirm::Close(target)) => *target == Target::Session(s.id),
            _ => false,
        };
        let style = if index == ws.active_tab {
            Style::new().fg(PANEL_BG).bg(ACCENT)
        } else if targeted {
            Style::new()
                .fg(TEXT)
                .bg(SURFACE1)
                .add_modifier(Modifier::BOLD)
        } else if !s.is_open() {
            Style::new()
                .fg(OVERLAY0)
                .bg(SURFACE0)
                .add_modifier(Modifier::DIM)
        } else if s.unread {
            Style::new().fg(YELLOW).bg(SURFACE0)
        } else {
            Style::new().fg(OVERLAY1).bg(SURFACE0)
        };
        let padding = width.saturating_sub(label.chars().count() as u16);
        let left = padding / 2;
        let text = format!(
            "{:left$}{label}{:right$}",
            "",
            "",
            left = left as usize,
            right = (padding - left) as usize
        );
        let rect = Rect::new(x, area.y, width, 1);
        put(buf, x, area.y, x + width, &text, style);
        hits.tabs.push((rect, index));
        x += width + 1;
        if width < desired {
            break;
        }
    }
    if start > 0 {
        put(
            buf,
            area.x,
            area.y,
            area.x + 1,
            "…",
            Style::new().fg(OVERLAY0).bg(PANEL_BG),
        );
    }
}

fn render_empty(buf: &mut Buffer, area: Rect, lines: &[String]) {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(SURFACE0))
        .render(area, buf);
    let top = area.y + area.height.saturating_sub(lines.len() as u16) / 2;
    for (i, line) in lines.iter().enumerate() {
        let style = Style::new().fg(if i == 0 { SUBTEXT0 } else { OVERLAY0 });
        let width = (line.chars().count() as u16).min(area.width.saturating_sub(2));
        let x = area.x + (area.width - width) / 2;
        put(buf, x, top + i as u16, area.right() - 1, line, style);
    }
}

fn render_pane(buf: &mut Buffer, area: Rect, app: &mut App, hits: &mut Hits) -> Option<Position> {
    hits.pane = area;
    if area.width < 4 || area.height < 3 {
        return None;
    }
    let Some(ws) = app.workspaces.get_mut(app.active) else {
        render_empty(
            buf,
            area,
            &[
                "no workspaces".into(),
                "ctrl+b N or click new to open a listener".into(),
            ],
        );
        return None;
    };
    let listen = format!("{} {}", ws.config.protocol.label(), ws.config.addr);
    let Some(s) = ws.sessions.get_mut(ws.active_tab) else {
        let lines = match &ws.state {
            ListenState::Listening => vec![
                format!("listening on {listen}"),
                "waiting for connections…".into(),
            ],
            ListenState::Binding => vec![format!("binding {listen}…")],
            ListenState::Failed(error) => vec![
                format!("could not listen on {listen}"),
                error.clone(),
                "ctrl+b s to change settings".into(),
            ],
        };
        render_empty(buf, area, &lines);
        return None;
    };

    let status = s.status();
    let (_, color) = status_glyph(status);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(
            match &s.name {
                Some(name) => format!(" {name} · {} ", s.peer),
                None => format!(" {} ", s.peer),
            },
            Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
        ))
        .title_top(
            Line::from(Span::styled(
                format!(
                    " {} · ↓{} ↑{} ",
                    status.text(),
                    human(s.bytes_in),
                    human(s.bytes_out)
                ),
                Style::new().fg(color),
            ))
            .right_aligned(),
        );
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height < 3 {
        return None;
    }

    let log = Rect {
        height: inner.height - 2,
        ..inner
    };
    render_transcript(buf, log, s);
    let separator_y = inner.bottom() - 2;
    put(
        buf,
        inner.x,
        separator_y,
        inner.right(),
        &"─".repeat(inner.width as usize),
        Style::new().fg(SURFACE0),
    );

    let input_y = inner.bottom() - 1;
    if !s.is_open() {
        put(
            buf,
            inner.x,
            input_y,
            inner.right(),
            "connection closed · ctrl+b X closes the tab",
            Style::new().fg(OVERLAY0),
        );
        return None;
    }
    let mut x = put(
        buf,
        inner.x,
        input_y,
        inner.right(),
        "❯ ",
        Style::new().fg(ACCENT),
    );
    let mut right = inner.right();
    if let Some(streams) = &s.streams {
        let label = match streams.send_to {
            Some(stream) => format!(" s{stream} "),
            None => " new ".into(),
        };
        let end = put(
            buf,
            x,
            input_y,
            right,
            &label,
            Style::new().fg(ACCENT).bg(SURFACE0),
        );
        hits.send_stream = Rect::new(x, input_y, end - x, 1);
        x = end + 1;
        if streams.send_to.is_some() && right >= x + 6 {
            let start = right - 5;
            put(
                buf,
                start,
                input_y,
                right,
                " fin ",
                Style::new().fg(OVERLAY0).bg(SURFACE0),
            );
            hits.finish_stream = Rect::new(start, input_y, 5, 1);
            right = start - 1;
        }
    }
    let cursor = render_input(
        buf,
        x,
        input_y,
        right,
        &s.input,
        Style::new().fg(TEXT),
        TextTarget::Session,
        hits,
    );
    matches!(app.mode, Mode::Normal).then_some(cursor)
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() || width == 0 {
        return vec![String::new()];
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn render_transcript(buf: &mut Buffer, area: Rect, s: &mut Session) {
    let height = area.height as usize;
    let needed = height + s.scroll;
    let tagged = s.streams.as_ref().is_some_and(|streams| streams.tagged());
    // Wrap from the newest line backwards, stopping once enough rows exist.
    let mut rows: Vec<(String, Style)> = Vec::new();
    'lines: for line in s.lines.iter().rev() {
        let (prefix, style) = match line.dir {
            Dir::In => ("", Style::new().fg(TEXT)),
            Dir::Out => ("❯ ", Style::new().fg(ACCENT)),
            Dir::Info => (
                "── ",
                Style::new().fg(OVERLAY0).add_modifier(Modifier::ITALIC),
            ),
        };
        let tag = match line.stream {
            Some(stream) if tagged => format!("[s{stream}] "),
            _ => String::new(),
        };
        for row in wrap(&format!("{prefix}{tag}{}", line.text), area.width as usize)
            .into_iter()
            .rev()
        {
            rows.push((row, style));
            if rows.len() >= needed {
                break 'lines;
            }
        }
    }
    s.scroll = s.scroll.min(rows.len().saturating_sub(height));

    let visible: Vec<_> = rows.iter().skip(s.scroll).take(height).collect();
    for (i, (text, style)) in visible.iter().rev().enumerate() {
        put(buf, area.x, area.y + i as u16, area.right(), text, *style);
    }
    if s.scroll > 0 {
        let label = format!(" ↓ {} more ", s.scroll);
        put_right(
            buf,
            area,
            area.bottom() - 1,
            &label,
            Style::new().fg(PANEL_BG).bg(ACCENT),
        );
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
        ("j/k", "workspace"),
        ("X", "close tab"),
        ("D", "close workspace"),
        ("?", "keybinds"),
        ("q", "quit"),
    ] {
        x = put(buf, x, y, pane.right(), " ", base);
        x = put(buf, x, y, pane.right(), k, key);
        x = put(buf, x, y, pane.right(), &format!(" {label} "), base);
    }
}

fn overlay_block(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .style(Style::new().bg(PANEL_BG))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
        ))
}

/// `fingerprint` is the running certificate's, shown for TLS workspaces.
fn render_settings(
    buf: &mut Buffer,
    area: Rect,
    form: &SettingsForm,
    fingerprint: Option<&str>,
    hits: &mut Hits,
) -> Option<Position> {
    let fields = form.fields();
    let fingerprint = fingerprint.filter(|_| form.protocol.uses_tls());
    let rows = fields.len() as u16 + if fingerprint.is_some() { 2 } else { 0 };
    let rect = centered(area, 58, rows + 6);
    Clear.render(rect, buf);
    let title = if form.target.is_some() {
        "workspace settings"
    } else {
        "new workspace"
    };
    let block = overlay_block(title);
    let inner = block.inner(rect);
    block.render(rect, buf);
    if inner.height < rows + 4 {
        return None;
    }

    let label_style = |field: Field| {
        if form.field == field {
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(SUBTEXT0)
        }
    };
    let value_x = inner.x + 12;
    let right = inner.right();
    let mut cursor = None;

    for (i, &field) in fields.iter().enumerate() {
        let y = inner.y + 1 + i as u16;
        put(
            buf,
            inner.x + 1,
            y,
            right,
            field.label(),
            label_style(field),
        );
        let text = match field {
            Field::Protocol => {
                let mut x = value_x;
                for protocol in Protocol::ALL {
                    let hit = FormHit::Protocol(protocol);
                    let selected = protocol == form.protocol;
                    x = render_choice(buf, x, y, right, protocol.label(), selected, hit, hits);
                }
                None
            }
            Field::Cert => {
                let mut x = value_x;
                for cert in CertKind::ALL {
                    let hit = FormHit::Cert(cert);
                    let selected = cert == form.cert;
                    x = render_choice(buf, x, y, right, cert.label(), selected, hit, hits);
                }
                None
            }
            Field::Address => Some((&form.address, TextTarget::Address, "")),
            Field::Port => Some((&form.port, TextTarget::Port, "")),
            Field::CertFile => Some((&form.cert_file, TextTarget::CertFile, "PEM file")),
            Field::KeyFile => Some((&form.key_file, TextTarget::KeyFile, "PEM file")),
            Field::Alpn => {
                let placeholder = match form.protocol {
                    Protocol::Quic => "required, e.g. h3",
                    _ => "none",
                };
                Some((&form.alpn, TextTarget::Alpn, placeholder))
            }
        };
        if let Some((input, target, placeholder)) = text {
            let at = render_input(
                buf,
                value_x,
                y,
                right,
                input,
                Style::new().fg(TEXT),
                target,
                hits,
            );
            if form.field == field {
                cursor = Some(at);
            } else if input.len() == 0 {
                put(
                    buf,
                    value_x,
                    y,
                    right,
                    placeholder,
                    Style::new().fg(OVERLAY0),
                );
            }
        }
        hits.form
            .push((Rect::new(inner.x, y, inner.width, 1), FormHit::Field(field)));
    }

    if let Some(fingerprint) = fingerprint {
        let y = inner.y + 1 + fields.len() as u16;
        put(
            buf,
            inner.x + 1,
            y,
            right,
            "sha256",
            Style::new().fg(SUBTEXT0),
        );
        // Two rows of four 8-digit groups.
        let groups: Vec<&str> = fingerprint
            .as_bytes()
            .chunks(8)
            .map(|group| std::str::from_utf8(group).unwrap_or(""))
            .collect();
        for (row, line) in groups.chunks(4).take(2).enumerate() {
            put(
                buf,
                value_x,
                y + row as u16,
                right,
                &line.join(" "),
                Style::new().fg(OVERLAY0),
            );
        }
    }

    if let Some(error) = &form.error {
        put(
            buf,
            inner.x + 1,
            inner.y + rows + 2,
            right,
            error,
            Style::new().fg(RED),
        );
    }
    put(
        buf,
        inner.x + 1,
        inner.bottom() - 1,
        right,
        "tab field · ←/→ choose · enter apply · esc cancel",
        Style::new().fg(OVERLAY0),
    );
    cursor
}

/// A form button, highlighted when selected. Returns where the next one goes.
#[allow(clippy::too_many_arguments)]
fn render_choice(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    label: &str,
    selected: bool,
    hit: FormHit,
    hits: &mut Hits,
) -> u16 {
    let style = if selected {
        Style::new().fg(PANEL_BG).bg(ACCENT)
    } else {
        Style::new().fg(OVERLAY0).bg(SURFACE0)
    };
    let end = put(buf, x, y, right, &format!(" {label} "), style);
    hits.form.push((Rect::new(x, y, end - x, 1), hit));
    end + 1
}

/// herdr-style menu: square accent border, opened at the click and kept on screen.
fn render_menu(buf: &mut Buffer, area: Rect, menu: &ContextMenu, hits: &mut Hits) {
    let label_width = MENU_ITEMS.iter().map(|l| l.len()).max().unwrap_or(0) as u16;
    let width = (label_width + 4).max(14).min(area.width);
    let height = (MENU_ITEMS.len() as u16 + 2).min(area.height);
    let x = menu.x.min(area.right().saturating_sub(width));
    let y = menu.y.min(area.bottom().saturating_sub(height));
    let rect = Rect::new(x, y, width, height);
    Clear.render(rect, buf);
    let block = Block::bordered()
        .border_style(Style::new().fg(ACCENT))
        .style(Style::new().bg(PANEL_BG));
    let inner = block.inner(rect);
    block.render(rect, buf);

    for (index, label) in MENU_ITEMS.iter().enumerate() {
        let row_y = inner.y + index as u16;
        if row_y >= inner.bottom() {
            break;
        }
        let row = Rect::new(inner.x, row_y, inner.width, 1);
        let style = if index == menu.highlighted {
            Style::new()
                .fg(PANEL_BG)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(TEXT).bg(PANEL_BG)
        };
        buf.set_style(row, style);
        put(buf, row.x, row.y, row.right(), &format!(" {label}"), style);
        hits.menu_items.push((row, index));
    }
}

fn render_rename(
    buf: &mut Buffer,
    area: Rect,
    form: &RenameForm,
    hits: &mut Hits,
) -> Option<Position> {
    let rect = centered(area, 46, 7);
    Clear.render(rect, buf);
    let block = overlay_block(match form.target {
        Target::Workspace(_) => "rename workspace",
        Target::Session(_) => "rename tab",
    });
    let inner = block.inner(rect);
    block.render(rect, buf);
    if inner.height < 4 || inner.width < 4 {
        return None;
    }

    let input = Rect::new(inner.x + 1, inner.y + 1, inner.width - 2, 1);
    buf.set_style(input, Style::new().fg(TEXT).bg(SURFACE0));
    let cursor = render_input(
        buf,
        input.x + 1,
        input.y,
        input.right() - 1,
        &form.input,
        Style::new().fg(TEXT).bg(SURFACE0),
        TextTarget::Rename,
        hits,
    );
    put(
        buf,
        inner.x + 1,
        inner.bottom() - 1,
        inner.right(),
        "enter save · esc cancel · empty resets",
        Style::new().fg(OVERLAY0),
    );
    Some(cursor)
}

/// herdr-style confirmation: red square panel, title, detail, two buttons.
fn render_confirm(buf: &mut Buffer, area: Rect, app: &App, confirm: &Confirm, hits: &mut Hits) {
    // e.g. `web (tcp :4444)` and `2 open connections`.
    let workspace = |id: WsId| {
        app.workspaces.iter().find(|w| w.id == id).map(|ws| {
            let who = match &ws.name {
                Some(name) => format!("{name} ({})", ws.endpoint()),
                None => ws.endpoint(),
            };
            let open = ws.open_count();
            let plural = if open == 1 { "" } else { "s" };
            (who, format!("{open} open connection{plural}"))
        })
    };
    let text = match *confirm {
        Confirm::Close(Target::Workspace(id)) => {
            workspace(id).map(|(who, open)| (" Close workspace?", format!(" {who} has {open}.")))
        }
        Confirm::Restart(ref form, _) => form.target.and_then(workspace).map(|(who, open)| {
            (
                " Apply settings?",
                format!(" Restarting {who} drops its {open}."),
            )
        }),
        Confirm::Close(Target::Session(id)) => app
            .workspaces
            .iter()
            .flat_map(|ws| &ws.sessions)
            .find(|s| s.id == id)
            .map(|s| {
                let who = match &s.name {
                    Some(name) => format!("{name} ({})", s.peer),
                    None => s.peer.to_string(),
                };
                (" Close tab?", format!(" {who} is still connected."))
            }),
    };
    let Some((title, detail)) = text else {
        return;
    };
    let rect = centered(area, 64, 6);
    Clear.render(rect, buf);
    let block = Block::bordered()
        .border_style(Style::new().fg(RED))
        .style(Style::new().bg(PANEL_BG));
    let inner = block.inner(rect);
    block.render(rect, buf);
    if inner.height < 4 {
        return;
    }

    put(
        buf,
        inner.x,
        inner.y,
        inner.right(),
        title,
        Style::new().fg(RED).add_modifier(Modifier::BOLD),
    );
    put(
        buf,
        inner.x,
        inner.y + 1,
        inner.right(),
        &detail,
        Style::new().fg(TEXT),
    );

    let (confirm, cancel) = (" ↵ confirm ", " esc cancel ");
    let (confirm_w, cancel_w) = (13, 12);
    let gap = 2;
    let x = inner.x + inner.width.saturating_sub(confirm_w + gap + cancel_w) / 2;
    let y = inner.y + 3;
    hits.confirm = Rect::new(x, y, confirm_w, 1);
    hits.cancel = Rect::new(x + confirm_w + gap, y, cancel_w, 1);
    for (rect, label, style) in [
        (
            hits.confirm,
            confirm,
            Style::new()
                .fg(PANEL_BG)
                .bg(RED)
                .add_modifier(Modifier::BOLD),
        ),
        (
            hits.cancel,
            cancel,
            Style::new()
                .fg(TEXT)
                .bg(SURFACE0)
                .add_modifier(Modifier::BOLD),
        ),
    ] {
        buf.set_style(rect, style);
        let width = (label.chars().count() as u16).min(rect.width);
        put(
            buf,
            rect.x + (rect.width - width) / 2,
            y,
            rect.right(),
            label,
            style,
        );
    }
}

fn render_help(buf: &mut Buffer, area: Rect) {
    const KEYS: &[(&str, &str)] = &[
        ("ctrl+b", "prefix"),
        ("prefix N", "new workspace (listener)"),
        ("prefix s", "workspace settings"),
        ("prefix j / k", "next / previous workspace"),
        ("prefix n / p", "next / previous tab"),
        ("prefix 1..9", "jump to tab"),
        ("prefix X", "close tab (disconnects)"),
        ("prefix D", "close workspace"),
        ("prefix W", "rename workspace"),
        ("prefix T", "rename tab"),
        ("right-click", "rename / close menu"),
        ("prefix q", "quit"),
        ("enter", "send input line"),
        ("prefix (⇧)tab", "quic: cycle send stream"),
        ("ctrl+d", "quic: finish send stream"),
        ("pgup / pgdn", "scroll transcript"),
    ];
    let rect = centered(area, 46, KEYS.len() as u16 + 4);
    Clear.render(rect, buf);
    let block = overlay_block("keybinds");
    let inner = block.inner(rect);
    block.render(rect, buf);
    for (i, (key, action)) in KEYS.iter().enumerate() {
        let y = inner.y + 1 + i as u16;
        if y >= inner.bottom() {
            break;
        }
        put(
            buf,
            inner.x + 1,
            y,
            inner.right(),
            key,
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        );
        put(
            buf,
            inner.x + 16,
            y,
            inner.right(),
            action,
            Style::new().fg(SUBTEXT0),
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Event;
    use crate::net::NetEvent;

    const W: u16 = 120;
    const H: u16 = 32;

    fn screen(app: &mut App) -> String {
        render_rows(app, W, H).join("\n")
    }

    fn row_with<'a>(rows: &'a [String], needle: &str) -> &'a str {
        rows.iter()
            .find(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("no row contains {needle:?}:\n{}", rows.join("\n")))
    }

    #[test]
    fn empty_app_explains_how_to_start() {
        let (mut app, _rx) = App::test_new();
        let text = screen(&mut app);
        assert!(text.contains(" workspaces"));
        assert!(text.contains("no listeners"));
        assert!(text.contains("no connections"));
        assert!(text.contains("ctrl+b N or click new to open a listener"));
    }

    #[test]
    fn listening_workspace_without_sessions_says_waiting() {
        let (mut app, _rx) = App::test_new();
        app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
        let rows = render_rows(&mut app, W, H);
        assert!(row_with(&rows, "1 tcp :4444").contains('○'));
        assert!(row_with(&rows, "0.0.0.0 · 0 open").starts_with("   0.0.0.0"));
        row_with(&rows, "listening on tcp 0.0.0.0:4444");
        row_with(&rows, "waiting for connections…");
    }

    #[test]
    fn session_shows_in_sidebar_tab_bar_and_pane() {
        let (mut app, _rx) = App::test_new();
        let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
        let (sess, _out) = app.add_test_session(ws, "10.0.0.5:51234");
        app.handle(Event::Net(NetEvent::Data {
            sess,
            bytes: b"hello there\n".to_vec(),
        }));
        let rows = render_rows(&mut app, W, H);

        // Tab bar (row 0) carries the peer and the listener address.
        assert!(rows[0].contains("10.0.0.5:51234"));
        assert!(rows[0].contains("tcp 0.0.0.0:4444"));
        // Pane border: peer on the left, status and byte counts on the right.
        assert!(rows[1].contains("╭ 10.0.0.5:51234 "));
        assert!(rows[1].contains(" open · ↓12B ↑0B ╮"));
        row_with(&rows, "── connected from 10.0.0.5:51234");
        row_with(&rows, "hello there");
        // Sessions panel.
        row_with(&rows, " sessions          1 open");
        assert!(row_with(&rows, "● 10.0.0.5:51234").starts_with(" ●"));
        assert!(row_with(&rows, "tcp :4444 · ↓12B").contains("open"));
        assert!(row_with(&rows, "❯").contains('❯'));
    }

    #[test]
    fn full_ipv4_peer_is_not_truncated_in_sidebar() {
        let (mut app, _rx) = App::test_new();
        let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
        app.add_test_session(ws, "255.255.255.255:65535");
        let rows = render_rows(&mut app, W, H);
        let sidebar: Vec<String> = rows
            .iter()
            .map(|row| {
                row.chars()
                    .take(crate::app::DEFAULT_SIDEBAR_WIDTH as usize)
                    .collect()
            })
            .collect();
        row_with(&sidebar, "255.255.255.255:65535");
    }

    #[test]
    fn failed_listener_shows_error_in_sidebar_and_pane() {
        let (mut app, _rx) = App::test_new();
        let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
        app.handle(Event::Net(NetEvent::ListenFailed {
            ws,
            error: "Address already in use (os error 98)".into(),
        }));
        let rows = render_rows(&mut app, W, H);
        row_with(&rows, "   Address already in us");
        row_with(&rows, "could not listen on tcp 0.0.0.0:4444");
        row_with(&rows, "ctrl+b s to change settings");
    }

    #[test]
    fn closed_session_replaces_input_with_hint() {
        let (mut app, _rx) = App::test_new();
        let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
        let (sess, _out) = app.add_test_session(ws, "10.0.0.5:51234");
        app.handle(Event::Net(NetEvent::Closed { sess, error: None }));
        let rows = render_rows(&mut app, W, H);
        assert!(rows[1].contains(" closed · "));
        row_with(&rows, "── disconnected");
        row_with(&rows, "connection closed · ctrl+b X closes the tab");
        // Closed sessions leave the sidebar list.
        row_with(&rows, "no connections");
    }

    #[test]
    fn prefix_mode_shows_mode_bar_on_last_row() {
        let (mut app, _rx) = App::test_new();
        app.mode = Mode::Prefix;
        let rows = render_rows(&mut app, W, H);
        let last = rows.last().unwrap();
        assert!(last.contains(" PREFIX "));
        assert!(last.contains("N new"));
        assert!(last.contains("D close workspace"));
    }

    #[test]
    fn settings_modal_fits_its_hint_line() {
        let (mut app, _rx) = App::test_new();
        app.mode = Mode::Settings(SettingsForm {
            target: None,
            protocol: Protocol::Udp,
            address: "0.0.0.0".into(),
            port: "4444".into(),
            cert: CertKind::SelfSigned,
            cert_file: "".into(),
            key_file: "".into(),
            alpn: "".into(),
            field: Field::Port,
            error: Some("invalid port".into()),
        });
        let rows = render_rows(&mut app, W, H);
        row_with(&rows, "╭ new workspace ");
        row_with(&rows, "address    0.0.0.0");
        row_with(&rows, "port       4444");
        row_with(&rows, "invalid port");
        let hint = row_with(&rows, "tab field");
        assert!(hint.contains("esc cancel"), "hint clipped: {hint:?}");
    }

    #[test]
    fn tls_settings_show_every_row_and_the_fingerprint() {
        use crate::net::tls::{self, CertSource, TlsConfig};
        let (mut app, _rx) = App::test_new();
        let ws = app.add_test_workspace(Protocol::Tls, "0.0.0.0:8443");
        let server = tls::build(&TlsConfig::default()).unwrap();
        let fingerprint = server.fingerprint.clone();
        app.workspaces[0].tls = Some(server);
        app.workspaces[0].config.tls.cert = CertSource::Files {
            cert: "/etc/outpost/server.pem".into(),
            key: "/etc/outpost/server.key".into(),
        };
        app.mode = Mode::Settings(SettingsForm {
            target: Some(ws),
            protocol: Protocol::Tls,
            address: "255.255.255.255".into(),
            port: "65535".into(),
            cert: CertKind::Files,
            cert_file: "/etc/outpost/server.pem".into(),
            key_file: "/etc/outpost/server.key".into(),
            alpn: "".into(),
            field: Field::Protocol,
            error: Some("key file: no private key found".into()),
        });
        let rows = render_rows(&mut app, W, H);
        row_with(&rows, "╭ workspace settings ");
        row_with(&rows, "protocol    tcp   udp   tls ");
        row_with(&rows, "cert        self-signed   files ");
        row_with(&rows, "cert file  /etc/outpost/server.pem");
        row_with(&rows, "key file   /etc/outpost/server.key");
        row_with(&rows, "alpn       none");
        let groups: Vec<&str> = (0..8).map(|i| &fingerprint[i * 8..i * 8 + 8]).collect();
        row_with(&rows, &format!("sha256     {}", groups[..4].join(" ")));
        row_with(&rows, &format!("            {}", groups[4..].join(" ")));
        row_with(&rows, "key file: no private key found");
        let hint = row_with(&rows, "tab field");
        assert!(hint.contains("esc cancel"), "hint clipped: {hint:?}");

        // Order: fields, then the fingerprint, then the error, then the hint.
        let at = |needle: &str| rows.iter().position(|r| r.contains(needle)).unwrap();
        assert!(at("protocol") < at("alpn"));
        assert!(at("alpn") < at("sha256"));
        assert!(at("sha256") < at("no private key"));
        assert!(at("no private key") < at("tab field"));
    }

    #[test]
    fn quic_tab_tags_streams_and_shows_the_send_stream() {
        let (mut app, _rx) = App::test_new();
        let ws = app.add_test_workspace(Protocol::Quic, "0.0.0.0:4433");
        let (sess, _out) = app.add_test_quic_session(ws, "255.255.255.255:65535");
        let data = |app: &mut App, stream, bytes: &[u8]| {
            app.handle(Event::Net(NetEvent::StreamData {
                sess,
                stream,
                bytes: bytes.to_vec(),
            }))
        };

        // One stream: no tags yet.
        data(&mut app, 0, b"GET /index.html\r\n");
        let rows = render_rows(&mut app, W, H);
        row_with(&rows, "│GET /index.html");
        let input = row_with(&rows, "❯  s0  ");
        assert!(input.contains(" fin │"), "{input:?}");

        // A second stream tags every stream line.
        data(&mut app, 4, b"GET /other\r\n");
        let rows = render_rows(&mut app, W, H);
        row_with(&rows, "│[s0] GET /index.html");
        row_with(&rows, "│[s4] GET /other");
        row_with(&rows, "❯  s4  ");
        row_with(&rows, "── connected from 255.255.255.255:65535");
        // The session row keeps its byte count; the workspace label gives way.
        row_with(&rows, "   quic :44… · ↓29B open ");

        app.handle(Event::Input(ratatui::crossterm::event::Event::Key(
            ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char('d'),
                ratatui::crossterm::event::KeyModifiers::CONTROL,
            ),
        )));
        let rows = render_rows(&mut app, W, H);
        row_with(&rows, "── [s4] finished sending");
        row_with(&rows, "❯  s0  ");
    }

    #[test]
    fn quic_form_asks_for_alpn_and_help_lists_stream_keys() {
        let (mut app, _rx) = App::test_new();
        app.mode = Mode::Settings(SettingsForm {
            target: None,
            protocol: Protocol::Quic,
            address: "0.0.0.0".into(),
            port: "4433".into(),
            cert: CertKind::SelfSigned,
            cert_file: "".into(),
            key_file: "".into(),
            alpn: "".into(),
            field: Field::Protocol,
            error: None,
        });
        let rows = render_rows(&mut app, W, H);
        row_with(&rows, "protocol    tcp   udp   tls   quic ");
        row_with(&rows, "alpn       required, e.g. h3");

        app.mode = Mode::Help;
        let rows = render_rows(&mut app, W, H);
        row_with(&rows, "prefix (⇧)tab  quic: cycle send stream");
        row_with(&rows, "ctrl+d         quic: finish send stream");
    }

    #[test]
    fn transcript_follows_tail_and_scrolls_back() {
        let (mut app, _rx) = App::test_new();
        let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
        let (sess, _out) = app.add_test_session(ws, "10.0.0.5:51234");
        let bytes: String = (0..100).map(|i| format!("line {i:03}\n")).collect();
        app.handle(Event::Net(NetEvent::Data {
            sess,
            bytes: bytes.into_bytes(),
        }));

        let text = render_rows(&mut app, W, H).join("\n");
        assert!(text.contains("line 099"));
        assert!(!text.contains("line 000"));

        app.workspaces[0].sessions[0].scroll = 1_000;
        let text = render_rows(&mut app, W, H).join("\n");
        assert!(text.contains("line 000"), "scrolled to the top");
        assert!(!text.contains("line 099"));
        assert!(text.contains("more "), "scroll indicator shown");
        // Overscroll is clamped to the top of the transcript.
        assert!(app.workspaces[0].sessions[0].scroll < 1_000);
    }

    #[test]
    fn renamed_workspace_shows_name_and_endpoint() {
        let (mut app, _rx) = App::test_new();
        app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
        app.workspaces[0].name = Some("web".into());
        let rows = render_rows(&mut app, W, H);
        row_with(&rows, " 1 web ○");
        row_with(&rows, "   tcp :4444 · 0 open");
    }

    #[test]
    fn context_menu_opens_at_the_click_and_stays_on_screen() {
        let (mut app, _rx) = App::test_new();
        let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
        app.mode = Mode::Menu(ContextMenu {
            target: Target::Workspace(ws),
            x: 3,
            y: 2,
            highlighted: 0,
        });
        let rows = render_rows(&mut app, W, H);
        assert_eq!(
            rows[2].chars().nth(3),
            Some('┌'),
            "top-left at the click: {:?}",
            rows[2]
        );
        assert!(rows[3].contains("│ Rename"));
        assert!(rows[4].contains("│ Close"));

        // Near the bottom-right corner it shifts up and left to fit.
        app.mode = Mode::Menu(ContextMenu {
            target: Target::Workspace(ws),
            x: W - 1,
            y: H - 1,
            highlighted: 1,
        });
        let rows = render_rows(&mut app, W, H);
        let last = rows.last().unwrap();
        assert!(
            last.ends_with("┘"),
            "bottom-right corner on screen: {last:?}"
        );
        assert!(rows[rows.len() - 2].contains("│ Close"));
    }

    #[test]
    fn rename_overlay_shows_the_input() {
        let (mut app, _rx) = App::test_new();
        let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
        app.mode = Mode::Rename(RenameForm {
            target: Target::Workspace(ws),
            input: "web".into(),
        });
        let rows = render_rows(&mut app, W, H);
        row_with(&rows, "╭ rename workspace ");
        row_with(&rows, "│  web ");
        row_with(&rows, "enter save · esc cancel · empty resets");
    }

    #[test]
    fn confirm_close_names_the_workspace_and_its_connections() {
        let (mut app, _rx) = App::test_new();
        let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
        app.workspaces[0].name = Some("web".into());
        let _a = app.add_test_session(ws, "10.0.0.5:1000");
        let _b = app.add_test_session(ws, "10.0.0.6:1000");
        app.mode = Mode::Confirm(Confirm::Close(Target::Workspace(ws)));
        let rows = render_rows(&mut app, W, H);
        row_with(&rows, "│ Close workspace?");
        row_with(&rows, " web (tcp :4444) has 2 open connections.");
        let buttons = row_with(&rows, "↵ confirm");
        assert!(buttons.contains("esc cancel"));
    }

    #[test]
    fn long_input_scrolls_to_keep_the_cursor_visible() {
        let (mut app, _rx) = App::test_new();
        let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
        app.add_test_session(ws, "10.0.0.5:51234");
        let text: String = ('a'..='z').cycle().take(200).collect();
        app.workspaces[0].sessions[0].input = text.as_str().into();
        let rows = render_rows(&mut app, W, H);
        // The end is visible while typing at the end...
        assert!(row_with(&rows, "❯ ").contains(&text[190..]));

        // ...and the start once the cursor moves home.
        app.workspaces[0].sessions[0].input.set_cursor(0);
        let rows = render_rows(&mut app, W, H);
        assert!(row_with(&rows, "❯ ").contains("❯ abcdefgh"));
    }

    #[test]
    fn renamed_tab_shows_its_name_everywhere() {
        let (mut app, _rx) = App::test_new();
        let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
        app.add_test_session(ws, "10.0.0.5:51234");
        app.workspaces[0].sessions[0].name = Some("db".into());
        let rows = render_rows(&mut app, W, H);
        assert!(rows[0].contains(" db "), "tab bar: {:?}", rows[0]);
        assert!(
            rows[1].contains("╭ db · 10.0.0.5:51234 "),
            "pane title keeps the peer"
        );
        assert!(
            row_with(&rows, " ● db").starts_with(" ● db"),
            "sessions list"
        );
    }

    #[test]
    fn rename_overlay_title_matches_its_target() {
        let (mut app, _rx) = App::test_new();
        let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
        let (sess, _out) = app.add_test_session(ws, "10.0.0.5:51234");
        app.mode = Mode::Rename(RenameForm {
            target: Target::Session(sess),
            input: "db".into(),
        });
        let rows = render_rows(&mut app, W, H);
        row_with(&rows, "╭ rename tab ");
    }

    #[test]
    fn confirm_close_for_a_tab_names_the_peer() {
        let (mut app, _rx) = App::test_new();
        let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
        let (sess, _out) = app.add_test_session(ws, "255.255.255.255:65535");
        app.workspaces[0].sessions[0].name = Some("db".into());
        app.mode = Mode::Confirm(Confirm::Close(Target::Session(sess)));
        let rows = render_rows(&mut app, W, H);
        row_with(&rows, "│ Close tab?");
        row_with(&rows, " db (255.255.255.255:65535) is still connected.");
        row_with(&rows, "↵ confirm");
    }

    #[test]
    fn confirm_restart_says_what_applying_drops() {
        let (mut app, _rx) = App::test_new();
        let ws = app.add_test_workspace(Protocol::Udp, "0.0.0.0:5555");
        app.workspaces[0].name = Some("web".into());
        let _a = app.add_test_session(ws, "10.0.0.5:1000");
        let _b = app.add_test_session(ws, "10.0.0.6:1000");
        app.mode = Mode::Settings(SettingsForm {
            target: Some(ws),
            protocol: Protocol::Udp,
            address: "0.0.0.0".into(),
            port: "5556".into(),
            cert: CertKind::SelfSigned,
            cert_file: "".into(),
            key_file: "".into(),
            alpn: "".into(),
            field: Field::Port,
            error: None,
        });
        app.handle(Event::Input(ratatui::crossterm::event::Event::Key(
            ratatui::crossterm::event::KeyCode::Enter.into(),
        )));
        let rows = render_rows(&mut app, W, H);
        row_with(&rows, "│ Apply settings?");
        row_with(
            &rows,
            " Restarting web (udp :5555) drops its 2 open connections.",
        );
        row_with(&rows, "↵ confirm");
    }

    #[test]
    fn sidebar_follows_its_width_and_split() {
        let (mut app, _rx) = App::test_new();
        let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
        app.add_test_session(ws, "10.0.0.5:51234");
        app.sidebar_width = 34;
        app.sidebar_split = 0.75;
        let rows = render_rows(&mut app, W, H);

        for row in &rows {
            assert_eq!(row.chars().nth(33), Some('│'), "separator column: {row:?}");
        }
        let tab_bar: String = rows[0].chars().skip(34).collect();
        assert!(
            tab_bar.starts_with("  10.0.0.5:51234"),
            "tabs start after it"
        );
        assert!(rows[24].starts_with(&"─".repeat(33)), "section line moved");
        assert!(rows[25].starts_with(" sessions"));
    }

    #[test]
    fn sidebar_never_takes_more_than_half_the_screen() {
        let (mut app, _rx) = App::test_new();
        app.sidebar_width = 36;
        render_rows(&mut app, 50, 20);
        assert_eq!(app.hits.sidebar_divider.x, 24);
    }

    #[test]
    fn wrap_splits_on_width_and_keeps_empty_lines() {
        assert_eq!(wrap("abcdef", 4), vec!["abcd", "ef"]);
        assert_eq!(wrap("", 4), vec![""]);
        assert_eq!(wrap("abcd", 4), vec!["abcd"]);
    }

    #[test]
    fn human_formats_byte_counts() {
        assert_eq!(human(0), "0B");
        assert_eq!(human(1023), "1023B");
        assert_eq!(human(1536), "1.5K");
        assert_eq!(human(3 * 1024 * 1024), "3.0M");
    }
}
