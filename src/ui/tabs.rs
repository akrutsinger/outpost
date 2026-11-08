//! The tab bar and the focused tab: transcript and input line.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Widget};

use super::{Hits, TextTarget, human, put, put_right, render_input, status_glyph};
use crate::app::{App, Dir, ListenState, Mode, Session, Target};
use crate::theme::*;

const MIN_TAB_WIDTH: u16 = 8;

fn tab_label(s: &Session) -> String {
    if s.unread {
        format!("● {}", s.label())
    } else {
        s.label()
    }
}

pub(super) fn render_tab_bar(buf: &mut Buffer, area: Rect, app: &App, hits: &mut Hits) {
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

    let target = app.mode.target();
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
        } else if target == Some(Target::Session(s.id)) {
            // The tab an overlay is acting on stands out like a selection.
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

pub(super) fn render_pane(
    buf: &mut Buffer,
    area: Rect,
    app: &mut App,
    hits: &mut Hits,
) -> Option<Position> {
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
        Rect::new(x, input_y, right.saturating_sub(x), 1),
        &s.input,
        Style::new().fg(TEXT),
        TextTarget::Session,
        hits,
    );
    matches!(app.mode, Mode::Normal).then_some(cursor)
}

pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
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
