//! Rendering, laid out after herdr: sidebar (workspaces over sessions), tab bar, pane.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use crate::app::{App, Dir, ListenState, Mode, Session, SettingsForm, Status};
use crate::net::Protocol;
use crate::theme::*;

const SIDEBAR_WIDTH: u16 = 26;
const MIN_TAB_WIDTH: u16 = 8;
const WORKSPACE_HEADER_ROWS: u16 = 2;

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
}

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let mut hits = Hits::default();
    let sidebar_width = SIDEBAR_WIDTH.min(area.width / 2);
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
        Mode::Settings(form) => cursor = render_settings(buf, area, form),
        Mode::Help => {
            render_help(buf, area);
            cursor = None;
        }
        _ => {}
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
        (content.height / 2).clamp(3, content.height - 3)
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
        if focused {
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
            put(buf, x, y, right, &s.peer.to_string(), name_style);
            // Status sits on the second row: a full IPv4 peer barely fits the first.
            let state_x = right.saturating_sub(state.len() as u16);
            put(
                buf,
                rect.x + 3,
                y + 1,
                state_x.saturating_sub(1),
                &format!("{} · ↓{}", ws.label(), human(s.bytes_in)),
                Style::new().fg(OVERLAY0),
            );
            put(buf, state_x, y + 1, right, state, Style::new().fg(color));
            hits.sessions.push((rect, ws_index, tab));
            y += 2;
        }
    }
}

fn tab_label(s: &Session) -> String {
    if s.unread {
        format!("● {}", s.peer)
    } else {
        s.peer.to_string()
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
        let style = if index == ws.active_tab {
            Style::new().fg(PANEL_BG).bg(ACCENT)
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
            format!(" {} ", s.peer),
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
    let x = put(
        buf,
        inner.x,
        input_y,
        inner.right(),
        "❯ ",
        Style::new().fg(ACCENT),
    );
    // Show the tail of long input so the cursor stays visible.
    let room = inner.right().saturating_sub(x + 1) as usize;
    let count = s.input.chars().count();
    let visible: String = s.input.chars().skip(count.saturating_sub(room)).collect();
    let end = put(
        buf,
        x,
        input_y,
        inner.right(),
        &visible,
        Style::new().fg(TEXT),
    );
    matches!(app.mode, Mode::Normal).then_some(Position::new(end, input_y))
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
        for row in wrap(&format!("{prefix}{}", line.text), area.width as usize)
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

fn render_settings(buf: &mut Buffer, area: Rect, form: &SettingsForm) -> Option<Position> {
    let rect = centered(area, 58, 9);
    Clear.render(rect, buf);
    let title = if form.target.is_some() {
        "workspace settings"
    } else {
        "new workspace"
    };
    let block = overlay_block(title);
    let inner = block.inner(rect);
    block.render(rect, buf);
    if inner.height < 7 {
        return None;
    }

    let label_style = |field: usize| {
        if form.field == field {
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(SUBTEXT0)
        }
    };
    let value_x = inner.x + 12;
    let right = inner.right();
    let mut cursor = None;

    let y = inner.y + 1;
    put(buf, inner.x + 1, y, right, "protocol", label_style(0));
    let mut x = value_x;
    for protocol in [Protocol::Tcp, Protocol::Udp] {
        let style = if protocol == form.protocol {
            Style::new().fg(PANEL_BG).bg(ACCENT)
        } else {
            Style::new().fg(OVERLAY0).bg(SURFACE0)
        };
        x = put(buf, x, y, right, &format!(" {} ", protocol.label()), style) + 1;
    }

    for (field, label, value, y) in [
        (1, "address", &form.address, inner.y + 2),
        (2, "port", &form.port, inner.y + 3),
    ] {
        put(buf, inner.x + 1, y, right, label, label_style(field));
        let end = put(buf, value_x, y, right, value, Style::new().fg(TEXT));
        if form.field == field {
            cursor = Some(Position::new(end, y));
        }
    }

    if let Some(error) = &form.error {
        put(
            buf,
            inner.x + 1,
            inner.y + 5,
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
        "tab field · ←/→ protocol · enter apply · esc cancel",
        Style::new().fg(OVERLAY0),
    );
    cursor
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
        ("prefix q", "quit"),
        ("enter", "send input line"),
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
