//! Overlays drawn over everything: settings, listener settings form, menus,
//! rename, confirmation, help.

use ratatui::buffer::Buffer;
use ratatui::layout::{Margin, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use super::{FormHit, Hits, TextTarget, put, render_input};
use crate::app::{
    App, CertKind, Confirm, ContextMenu, Endpoint, Field, Kind, RenameForm, SettingsForm, Tab,
    Target,
};
use crate::detect::Integration;
use crate::net::{EndpointId, Protocol};
use crate::theme::*;

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

/// `fingerprint` is the running certificate's, shown for TLS listeners.
pub(super) fn render_settings(
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
        "listener settings"
    } else {
        "new listener"
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
        let value = Rect::new(value_x, y, right.saturating_sub(value_x), 1);
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
                let mut row = value;
                for protocol in Protocol::ALL {
                    let hit = FormHit::Protocol(protocol);
                    let selected = protocol == form.protocol;
                    render_choice(buf, &mut row, protocol.label(), selected, hit, hits);
                }
                None
            }
            Field::Cert => {
                let mut row = value;
                for cert in CertKind::ALL {
                    let hit = FormHit::Cert(cert);
                    let selected = cert == form.cert;
                    render_choice(buf, &mut row, cert.label(), selected, hit, hits);
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
            let at = render_input(buf, value, input, Style::new().fg(TEXT), target, hits);
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

/// Draws a form button at the start of `row`, highlighted when selected, and
/// moves `row` past it (and a gap) for the next one.
fn render_choice(
    buf: &mut Buffer,
    row: &mut Rect,
    label: &str,
    selected: bool,
    hit: FormHit,
    hits: &mut Hits,
) {
    let style = if selected {
        Style::new().fg(PANEL_BG).bg(ACCENT)
    } else {
        Style::new().fg(OVERLAY0).bg(SURFACE0)
    };
    let end = put(buf, row.x, row.y, row.right(), &format!(" {label} "), style);
    hits.form
        .push((Rect::new(row.x, row.y, end - row.x, 1), hit));
    let next = (end + 1).min(row.right());
    *row = Rect::new(next, row.y, row.right() - next, 1);
}

/// herdr-style menu: square accent border, opened where asked and kept on screen.
pub(super) fn render_menu(buf: &mut Buffer, area: Rect, menu: &ContextMenu, hits: &mut Hits) {
    let (width, height) = menu.size();
    let (width, height) = (width.min(area.width), height.min(area.height));
    let x = menu.x.min(area.right().saturating_sub(width));
    let y = menu.y.min(area.bottom().saturating_sub(height));
    let rect = Rect::new(x, y, width, height);
    Clear.render(rect, buf);
    let block = Block::bordered()
        .border_style(Style::new().fg(ACCENT))
        .style(Style::new().bg(PANEL_BG));
    let inner = block.inner(rect);
    block.render(rect, buf);

    for (index, item) in menu.items.iter().enumerate() {
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
        put(
            buf,
            row.x,
            row.y,
            row.right(),
            &format!(" {}", item.label()),
            style,
        );
        hits.menu_items.push((row, index));
    }
}

/// settings overlay: a square panel with section tabs. Its only
/// section so far lists the integrations, which always run.
pub(super) fn render_app_settings(buf: &mut Buffer, area: Rect, hits: &mut Hits) {
    let rect = centered(area, 76.min(area.width.saturating_sub(4)), 14);
    Clear.render(rect, buf);
    let block = Block::bordered()
        .border_style(Style::new().fg(ACCENT))
        .style(Style::new().bg(PANEL_BG));
    let inner = block.inner(rect);
    block.render(rect, buf);
    if inner.width < 20 || inner.height < 12 {
        return;
    }
    let bold = Style::new()
        .fg(TEXT)
        .bg(PANEL_BG)
        .add_modifier(Modifier::BOLD);
    let quiet = Style::new().fg(OVERLAY1).bg(PANEL_BG);
    put(buf, inner.x, inner.y, inner.right(), " settings", bold);
    // The section tabs; integrations is the only section so far.
    put(
        buf,
        inner.x,
        inner.y + 1,
        inner.right(),
        " integrations ",
        Style::new()
            .fg(PANEL_BG)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
    );
    put(
        buf,
        inner.x,
        inner.y + 2,
        inner.right(),
        &"─".repeat(inner.width as usize),
        Style::new().fg(SURFACE0).bg(PANEL_BG),
    );

    let top = inner.y + 4;
    put(buf, inner.x, top, inner.right(), "integrations", bold);
    put(
        buf,
        inner.x,
        top + 1,
        inner.right(),
        "a shell running one of these is filed under the host it connects to",
        quiet,
    );
    for (index, integration) in Integration::ALL.iter().enumerate() {
        let y = top + 3 + index as u16;
        let (marker, color, status) = if integration.active() {
            ("✓", GREEN, "active")
        } else {
            ("–", OVERLAY0, "planned")
        };
        put(
            buf,
            inner.x,
            y,
            inner.x + 3,
            &format!(" {marker}"),
            Style::new().fg(color).bg(PANEL_BG),
        );
        put(
            buf,
            inner.x + 3,
            y,
            inner.x + 14,
            integration.name(),
            Style::new().fg(SUBTEXT0).bg(PANEL_BG),
        );
        put(buf, inner.x + 14, y, inner.right(), status, quiet);
    }

    // lone close button, centered on the bottom row.
    let label = " esc close ";
    let width = label.chars().count() as u16;
    let button = Rect::new(
        inner.x + inner.width.saturating_sub(width) / 2,
        inner.bottom() - 1,
        width,
        1,
    );
    put(
        buf,
        button.x,
        button.y,
        button.right(),
        label,
        Style::new()
            .fg(TEXT)
            .bg(SURFACE0)
            .add_modifier(Modifier::BOLD),
    );
    hits.cancel = button;
}

pub(super) fn render_rename(
    buf: &mut Buffer,
    area: Rect,
    form: &RenameForm,
    hits: &mut Hits,
) -> Option<Position> {
    let rect = centered(area, 46, 7);
    Clear.render(rect, buf);
    let block = overlay_block(match form.target {
        Target::Endpoint(_) => "rename endpoint",
        Target::Tab(_) => "rename tab",
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
        input.inner(Margin::new(1, 0)),
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
pub(super) fn render_confirm(
    buf: &mut Buffer,
    area: Rect,
    app: &App,
    confirm: &Confirm,
    hits: &mut Hits,
) {
    let count = |n: usize, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
    // What an endpoint's open tabs are: connections, or shells.
    let noun = |endpoint: &Endpoint| match endpoint.kind {
        Kind::Listener(_) => "open connection",
        Kind::Local => "running shell",
        Kind::Ssh(_) => "ssh session",
    };
    // e.g. `web (tcp :4444)` and `2 open connections`.
    let describe = |endpoint: &Endpoint| {
        let who = match &endpoint.name {
            Some(name) => format!("{name} ({})", endpoint.addr_label()),
            None => endpoint.addr_label(),
        };
        (who, count(endpoint.open_count(), noun(endpoint)))
    };
    let endpoint = |id: EndpointId| app.endpoint(id).map(describe);
    let text = match *confirm {
        Confirm::Close(Target::Endpoint(id)) => {
            let title = match app.endpoint(id).map(|endpoint| &endpoint.kind) {
                Some(Kind::Local) => " Close all shells?",
                // Its tabs are local shells, which close with it.
                Some(Kind::Ssh(_)) => " Close host and its shells?",
                _ => " Close listener?",
            };
            endpoint(id).map(|(who, open)| (title, format!(" {who} has {open}.")))
        }
        Confirm::Restart(ref form, _) => form.target.and_then(endpoint).map(|(who, open)| {
            (
                " Apply settings?",
                format!(" Restarting {who} drops its {open}."),
            )
        }),
        Confirm::Quit => {
            let live: Vec<_> = app
                .endpoints
                .iter()
                .filter(|endpoint| endpoint.open_count() > 0)
                .collect();
            let detail = match live[..] {
                [endpoint] => {
                    let (who, open) = describe(endpoint);
                    format!(" {who} has {open}.")
                }
                _ => {
                    // e.g. `3 open connections and 1 running shell`.
                    let mut parts = Vec::new();
                    for what in ["open connection", "ssh session", "running shell"] {
                        let open: usize = live
                            .iter()
                            .filter(|endpoint| noun(endpoint) == what)
                            .map(|endpoint| endpoint.open_count())
                            .sum();
                        if open > 0 {
                            parts.push(count(open, what));
                        }
                    }
                    format!(
                        " {} across {}.",
                        parts.join(" and "),
                        count(live.len(), "endpoint")
                    )
                }
            };
            Some((" Quit outpost?", detail))
        }
        Confirm::Close(Target::Tab(id)) => app.tab(id).map(|tab| match tab {
            Tab::Session(s) => {
                let who = match &s.name {
                    Some(name) => format!("{name} ({})", s.peer),
                    None => s.peer.to_string(),
                };
                (" Close tab?", format!(" {who} is still connected."))
            }
            Tab::Terminal(t) => (" Close tab?", format!(" {} is still running.", t.label())),
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

pub(super) fn render_help(buf: &mut Buffer, area: Rect) {
    const KEYS: &[(&str, &str)] = &[
        ("ctrl+b", "prefix"),
        ("prefix c", "new shell"),
        ("prefix N", "new listener"),
        ("prefix s", "settings (integrations)"),
        ("prefix e", "listener settings"),
        ("prefix j / k", "next / previous endpoint"),
        ("prefix n / p", "next / previous tab"),
        ("prefix 1..9", "jump to tab"),
        ("prefix X", "close tab (disconnects)"),
        ("prefix D", "close endpoint"),
        ("prefix W", "rename endpoint"),
        ("prefix T", "rename tab"),
        ("right-click", "rename / close menu"),
        ("prefix q", "quit"),
        ("prefix ctrl+b", "send ctrl+b to the shell"),
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
