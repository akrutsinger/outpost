//! The sidebar: endpoints over sessions, with draggable dividers.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::{Hits, human, put, put_right, status_glyph};
use crate::app::{App, ListenState, Target};
use crate::theme::*;

const ENDPOINT_HEADER_ROWS: u16 = 2;

fn header_style() -> Style {
    Style::new().fg(OVERLAY0).add_modifier(Modifier::BOLD)
}

pub(super) fn render_sidebar(buf: &mut Buffer, area: Rect, app: &App, hits: &mut Hits) {
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
    render_endpoints(buf, top, app, hits);
    render_sessions(buf, bottom, app, hits);
}

fn render_endpoints(buf: &mut Buffer, area: Rect, app: &App, hits: &mut Hits) {
    put(
        buf,
        area.x,
        area.y,
        area.right(),
        " listeners",
        header_style(),
    );
    let body = Rect::new(
        area.x,
        area.y + ENDPOINT_HEADER_ROWS,
        area.width,
        area.height.saturating_sub(ENDPOINT_HEADER_ROWS + 1),
    );
    if app.endpoints.is_empty() && !body.is_empty() {
        put(
            buf,
            body.x,
            body.y,
            body.right(),
            " no listeners",
            Style::new().fg(OVERLAY0).add_modifier(Modifier::DIM),
        );
    }

    let target = app.mode.target();
    let mut y = body.y;
    for (index, endpoint) in app.endpoints.iter().enumerate() {
        if y + 2 > body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, body.width, 2);
        let focused = index == app.active;
        // The endpoint an overlay is acting on stands out like a selection.
        if target == Some(Target::Endpoint(endpoint.id)) {
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
        let (icon, color) = status_glyph(endpoint.status());
        let mut x = put(
            buf,
            rect.x + 1,
            y,
            right,
            &format!("{} ", index + 1),
            Style::new().fg(OVERLAY0),
        );
        x = put(buf, x, y, right, &endpoint.label(), name_style);
        put(
            buf,
            x,
            y,
            right,
            &format!(" {icon}"),
            Style::new().fg(color),
        );

        let secondary = Style::new().fg(if focused { MAUVE } else { OVERLAY0 });
        let (detail, style) = match &endpoint.state {
            ListenState::Failed(error) => (error.clone(), Style::new().fg(RED)),
            ListenState::Binding => ("binding…".to_string(), secondary),
            // A custom name hides the port, so show it here instead of the IP.
            ListenState::Listening if endpoint.name.is_some() => (
                format!("{} · {} open", endpoint.addr_label(), endpoint.open_count()),
                secondary,
            ),
            ListenState::Listening => (
                format!(
                    "{} · {} open",
                    endpoint.config.addr.ip(),
                    endpoint.open_count()
                ),
                secondary,
            ),
        };
        put(buf, rect.x + 3, y + 1, right, &detail, style);
        hits.endpoints.push((rect, index));
        y += 3;
    }

    if area.height > ENDPOINT_HEADER_ROWS {
        let footer_y = area.bottom() - 1;
        hits.new_listener = Rect::new(area.x, footer_y, 4.min(area.width), 1);
        put(
            buf,
            area.x,
            footer_y,
            area.right(),
            " new",
            Style::new().fg(OVERLAY0),
        );
        if !app.endpoints.is_empty() {
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
    let open: usize = app
        .endpoints
        .iter()
        .map(|endpoint| endpoint.open_count())
        .sum();
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
    for (listener_index, endpoint) in app.endpoints.iter().enumerate() {
        for (tab, s) in endpoint
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_open())
        {
            if y + 2 > body.bottom() {
                return;
            }
            let rect = Rect::new(body.x, y, body.width, 2);
            let focused = listener_index == app.active && tab == endpoint.active_tab;
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
            // Shorten the endpoint label first so the byte count stays visible.
            let bytes = format!(" · ↓{}", human(s.bytes_in));
            let room = detail_right.saturating_sub(rect.x + 3) as usize;
            let label = shorten(
                &endpoint.label(),
                room.saturating_sub(bytes.chars().count()),
            );
            put(
                buf,
                rect.x + 3,
                y + 1,
                detail_right,
                &format!("{label}{bytes}"),
                Style::new().fg(OVERLAY0),
            );
            put(buf, state_x, y + 1, right, state, Style::new().fg(color));
            hits.sessions.push((rect, (listener_index, tab)));
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
