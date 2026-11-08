//! Behavior tests: drive `App::handle` with keys, clicks, and network events,
//! then check state and what a session's peer would receive.

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::error::TryRecvError;

use super::session::{MAX_LINES, Streams};
use super::settings::form_path;
use super::*;
use crate::net::Protocol;
use crate::net::quic::{QuicOut, StreamEnd, StreamId};
use crate::net::tls::{CertSource, TlsConfig};
use crate::ui::{TextTarget, render_rows};

fn key(app: &mut App, code: KeyCode) {
    app.handle(Event::Input(TermEvent::Key(KeyEvent::new(
        code,
        KeyModifiers::NONE,
    ))));
}

fn type_str(app: &mut App, text: &str) {
    for c in text.chars() {
        key(app, KeyCode::Char(c));
    }
}

/// `ctrl+b` followed by `c`.
fn prefix(app: &mut App, c: char) {
    app.handle(Event::Input(TermEvent::Key(KeyEvent::new(
        KeyCode::Char('b'),
        KeyModifiers::CONTROL,
    ))));
    key(app, KeyCode::Char(c));
}

fn mouse(app: &mut App, kind: MouseEventKind, rect: Rect) {
    app.handle(Event::Input(TermEvent::Mouse(MouseEvent {
        kind,
        column: rect.x,
        row: rect.y,
        modifiers: KeyModifiers::NONE,
    })));
}

fn click(app: &mut App, rect: Rect) {
    mouse(app, MouseEventKind::Down(MouseButton::Left), rect);
}

fn right_click(app: &mut App, rect: Rect) {
    mouse(app, MouseEventKind::Down(MouseButton::Right), rect);
}

fn data(app: &mut App, sess: SessId, bytes: &[u8]) {
    app.handle(Event::Net(NetEvent::Data {
        sess,
        bytes: bytes.to_vec(),
    }));
}

fn transcript(app: &App, listener: usize, tab: usize) -> Vec<(Dir, String)> {
    app.listeners[listener].sessions[tab]
        .lines
        .iter()
        .map(|line| (line.dir, line.text.clone()))
        .collect()
}

fn incoming(app: &App, listener: usize, tab: usize) -> Vec<String> {
    transcript(app, listener, tab)
        .into_iter()
        .filter(|(dir, _)| *dir == Dir::In)
        .map(|(_, text)| text)
        .collect()
}

fn form(app: &App) -> &SettingsForm {
    match &app.mode {
        Mode::Settings(form) => form,
        _ => panic!("settings form is not open"),
    }
}

/// One TCP listener on :4444 with a single focused session.
fn one_session() -> (App, SessId, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    let (sess, out) = app.add_test_session(listener, "10.0.0.5:51234");
    (app, sess, out)
}

#[test]
fn incoming_data_splits_lines_and_drops_carriage_returns() {
    let (mut app, sess, _out) = one_session();
    data(&mut app, sess, b"one\r\ntwo\n\nthree\n");
    assert_eq!(incoming(&app, 0, 0), ["one", "two", "", "three"]);
    assert_eq!(app.listeners[0].sessions[0].bytes_in, 16);
}

#[test]
fn partial_line_continues_across_reads() {
    let (mut app, sess, _out) = one_session();
    data(&mut app, sess, b"$ ");
    data(&mut app, sess, b"ls\nfile");
    data(&mut app, sess, b".txt\n");
    assert_eq!(incoming(&app, 0, 0), ["$ ls", "file.txt"]);
}

#[test]
fn control_bytes_are_escaped_and_tabs_expanded() {
    let (mut app, sess, _out) = one_session();
    data(&mut app, sess, b"a\tb\x1b[0m\x00\n");
    assert_eq!(incoming(&app, 0, 0), ["a    b\\x1b[0m\\x00"]);
}

#[test]
fn transcript_is_capped() {
    let (mut app, sess, _out) = one_session();
    let flood = "x\n".repeat(MAX_LINES + 50);
    data(&mut app, sess, flood.as_bytes());
    assert_eq!(app.listeners[0].sessions[0].lines.len(), MAX_LINES);
}

#[test]
fn enter_sends_input_line_to_focused_session() {
    let (mut app, _sess, mut out) = one_session();
    type_str(&mut app, "hi there");
    key(&mut app, KeyCode::Backspace);
    key(&mut app, KeyCode::Enter);

    assert_eq!(out.try_recv().unwrap(), b"hi ther\n");
    let s = &app.listeners[0].sessions[0];
    assert_eq!(s.input, "");
    assert_eq!(s.bytes_out, 8);
    assert_eq!(
        transcript(&app, 0, 0).last().unwrap(),
        &(Dir::Out, "hi ther".to_string())
    );
}

#[test]
fn outgoing_line_ends_a_partial_incoming_line() {
    let (mut app, sess, _out) = one_session();
    data(&mut app, sess, b"$ ");
    type_str(&mut app, "ls");
    key(&mut app, KeyCode::Enter);
    data(&mut app, sess, b"file.txt\n");
    assert_eq!(incoming(&app, 0, 0), ["$ ", "file.txt"]);
}

#[test]
fn typing_is_ignored_once_the_peer_disconnects() {
    let (mut app, sess, mut out) = one_session();
    app.handle(Event::Net(NetEvent::Closed {
        sess,
        error: Some("connection reset by peer".into()),
    }));
    type_str(&mut app, "hello");
    key(&mut app, KeyCode::Enter);

    let s = &app.listeners[0].sessions[0];
    assert!(!s.is_open());
    assert_eq!(s.input, "");
    assert!(out.try_recv().is_err());
    assert_eq!(
        transcript(&app, 0, 0).last().unwrap(),
        &(
            Dir::Info,
            "disconnected: connection reset by peer".to_string()
        )
    );
}

#[test]
fn send_failure_closes_the_session() {
    let (mut app, _sess, out) = one_session();
    drop(out); // The connection task is gone.
    type_str(&mut app, "hello");
    key(&mut app, KeyCode::Enter);
    let s = &app.listeners[0].sessions[0];
    assert!(!s.is_open());
    assert_eq!(s.bytes_out, 0);
}

#[test]
fn background_activity_is_unread_until_focused() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    let (first, _a) = app.add_test_session(listener, "10.0.0.5:1000");
    let (second, _b) = app.add_test_session(listener, "10.0.0.6:2000");

    // A brand-new background connection counts as unread; the focused one doesn't.
    assert!(!app.listeners[0].sessions[0].unread);
    assert!(app.listeners[0].sessions[1].unread);
    assert_eq!(app.listeners[0].status(), Status::Unread);

    prefix(&mut app, 'n');
    assert!(!app.listeners[0].sessions[1].unread);

    data(&mut app, first, b"ping\n");
    data(&mut app, second, b"pong\n");
    assert!(app.listeners[0].sessions[0].unread);
    assert!(!app.listeners[0].sessions[1].unread);
}

#[test]
fn prefix_keys_switch_tabs() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    let _sessions: Vec<_> = (0..3)
        .map(|i| app.add_test_session(listener, &format!("10.0.0.{i}:1000")))
        .collect();

    prefix(&mut app, 'n');
    assert_eq!(app.listeners[0].active_tab, 1);
    prefix(&mut app, 'p');
    prefix(&mut app, 'p');
    assert_eq!(app.listeners[0].active_tab, 2, "wraps backwards");
    prefix(&mut app, '1');
    assert_eq!(app.listeners[0].active_tab, 0);
    prefix(&mut app, '9');
    assert_eq!(app.listeners[0].active_tab, 0, "out of range is ignored");
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn prefix_keys_switch_listeners() {
    let (mut app, _rx) = App::test_new();
    app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    app.add_test_listener(Protocol::Udp, "0.0.0.0:5555");
    prefix(&mut app, 'j');
    assert_eq!(app.active, 1);
    prefix(&mut app, 'j');
    assert_eq!(app.active, 0, "wraps forwards");
    prefix(&mut app, 'k');
    assert_eq!(app.active, 1);
}

#[test]
fn close_tab_disconnects_and_keeps_a_valid_focus() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    let (_a, _out_a) = app.add_test_session(listener, "10.0.0.5:1000");
    let (b, mut out_b) = app.add_test_session(listener, "10.0.0.6:2000");
    prefix(&mut app, 'n');

    prefix(&mut app, 'X');
    assert!(matches!(app.mode, Mode::Confirm(Confirm::Close(t)) if t == Target::Session(b)));
    assert_eq!(
        out_b.try_recv(),
        Err(TryRecvError::Empty),
        "still connected"
    );
    key(&mut app, KeyCode::Enter);
    // Dropping the outlet is what tells the connection task to hang up.
    assert_eq!(out_b.try_recv(), Err(TryRecvError::Disconnected));
    assert_eq!(app.listeners[0].sessions.len(), 1);
    assert_eq!(app.listeners[0].active_tab, 0);

    // A late event for the removed session is ignored.
    data(&mut app, b, b"late\n");
    assert_eq!(incoming(&app, 0, 0), Vec::<String>::new());
}

#[test]
fn close_listener_drops_its_sessions() {
    let (mut app, _rx) = App::test_new();
    app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:5555");
    let (_sess, mut out) = app.add_test_session(listener, "10.0.0.5:1000");
    prefix(&mut app, 'j');

    prefix(&mut app, 'D');
    assert!(
        matches!(app.mode, Mode::Confirm(Confirm::Close(t)) if t == Target::Listener(listener))
    );
    assert!(
        out.try_recv().is_err_and(|e| e == TryRecvError::Empty),
        "still connected"
    );
    key(&mut app, KeyCode::Enter);
    assert_eq!(out.try_recv(), Err(TryRecvError::Disconnected));
    assert_eq!(app.listeners.len(), 1);
    assert_eq!(app.active, 0);
}

#[test]
fn quit_and_help() {
    let (mut app, _rx) = App::test_new();
    prefix(&mut app, '?');
    assert!(matches!(app.mode, Mode::Help));
    key(&mut app, KeyCode::Esc);
    assert!(matches!(app.mode, Mode::Normal));
    prefix(&mut app, 'q');
    assert!(app.quit);
}

#[test]
fn quitting_with_live_connections_asks_first() {
    let (mut app, _sess, _out) = one_session();
    prefix(&mut app, 'q');
    assert!(matches!(app.mode, Mode::Confirm(Confirm::Quit)));
    type_str(&mut app, "y");
    assert!(
        matches!(app.mode, Mode::Confirm(_)),
        "other keys are ignored"
    );
    key(&mut app, KeyCode::Esc);
    assert!(matches!(app.mode, Mode::Normal));
    assert!(!app.quit);

    prefix(&mut app, 'q');
    render_rows(&mut app, 120, 32);
    let confirm = app.hits.confirm;
    click(&mut app, confirm);
    assert!(app.quit);
}

#[test]
fn quitting_after_connections_close_skips_confirmation() {
    let (mut app, sess, _out) = one_session();
    app.handle(Event::Net(NetEvent::Closed { sess, error: None }));
    prefix(&mut app, 'q');
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.quit);
}

#[test]
fn datagrams_map_to_one_session_per_peer() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Udp, "0.0.0.0:5555");
    let (udp_tx, mut udp_rx) = tokio::sync::mpsc::unbounded_channel();
    app.handle(Event::Net(NetEvent::Listening {
        listener,
        local: "0.0.0.0:5555".parse().unwrap(),
        udp: Some(udp_tx),
    }));

    let peer_a: SocketAddr = "10.0.0.5:1000".parse().unwrap();
    let peer_b: SocketAddr = "10.0.0.6:2000".parse().unwrap();
    for (peer, bytes) in [(peer_a, "one\n"), (peer_b, "two\n"), (peer_a, "three\n")] {
        app.handle(Event::Net(NetEvent::Datagram {
            listener,
            peer,
            bytes: bytes.as_bytes().to_vec(),
        }));
    }
    assert_eq!(app.listeners[0].sessions.len(), 2);
    assert_eq!(incoming(&app, 0, 0), ["one", "three"]);
    assert_eq!(incoming(&app, 0, 1), ["two"]);

    // Replies go out through the shared socket, addressed to the focused peer.
    prefix(&mut app, 'n');
    type_str(&mut app, "hey");
    key(&mut app, KeyCode::Enter);
    assert_eq!(udp_rx.try_recv().unwrap(), (peer_b, b"hey\n".to_vec()));
}

#[test]
fn datagrams_are_ignored_until_the_socket_is_bound() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Udp, "0.0.0.0:5555");
    app.handle(Event::Net(NetEvent::Datagram {
        listener,
        peer: "10.0.0.5:1000".parse().unwrap(),
        bytes: b"early".to_vec(),
    }));
    assert!(app.listeners[0].sessions.is_empty());
}

#[test]
fn new_listener_form_suggests_the_next_free_port() {
    let (mut app, _rx) = App::test_new();
    prefix(&mut app, 'N');
    assert_eq!(form(&app).port, "4444");
    key(&mut app, KeyCode::Esc);

    app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    app.add_test_listener(Protocol::Tcp, "0.0.0.0:4445");
    prefix(&mut app, 'N');
    assert_eq!(form(&app).port, "4446");
    assert_eq!(form(&app).target, None);
}

#[test]
fn form_editing_filters_keys_per_field() {
    let (mut app, _rx) = App::test_new();
    prefix(&mut app, 'N');
    // Starts on the port field.
    for _ in 0..4 {
        key(&mut app, KeyCode::Backspace);
    }
    type_str(&mut app, "8x0801");
    assert_eq!(form(&app).port, "80801", "non-digits dropped, max 5 digits");

    key(&mut app, KeyCode::Tab); // -> protocol
    key(&mut app, KeyCode::Right);
    assert_eq!(form(&app).protocol, Protocol::Udp);

    key(&mut app, KeyCode::Tab); // -> address
    for _ in 0..7 {
        key(&mut app, KeyCode::Backspace);
    }
    type_str(&mut app, "::1 zz");
    assert_eq!(form(&app).address, "::1", "only IP characters accepted");
}

#[test]
fn invalid_form_stays_open_with_an_error() {
    let (mut app, _rx) = App::test_new();
    prefix(&mut app, 'N');
    for _ in 0..4 {
        key(&mut app, KeyCode::Backspace);
    }
    type_str(&mut app, "70000"); // Doesn't fit in a u16.
    key(&mut app, KeyCode::Enter);
    assert_eq!(form(&app).error.as_deref(), Some("invalid port"));

    key(&mut app, KeyCode::Tab); // -> protocol
    key(&mut app, KeyCode::Tab); // -> address
    type_str(&mut app, "."); // "0.0.0.0."
    key(&mut app, KeyCode::Tab); // -> port
    for _ in 0..5 {
        key(&mut app, KeyCode::Backspace);
    }
    type_str(&mut app, "4444");
    key(&mut app, KeyCode::Enter);
    assert_eq!(form(&app).error.as_deref(), Some("invalid address"));
    assert!(app.listeners.is_empty());
}

#[test]
fn duplicate_listener_is_rejected() {
    let (mut app, _rx) = App::test_new();
    app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    prefix(&mut app, 'N');
    key(&mut app, KeyCode::Backspace);
    type_str(&mut app, "4");
    key(&mut app, KeyCode::Enter);
    assert_eq!(
        form(&app).error.as_deref(),
        Some("a listener already uses that address")
    );
    assert_eq!(app.listeners.len(), 1);
}

#[test]
fn settings_without_changes_leave_the_listener_alone() {
    let (mut app, _rx) = App::test_new();
    app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    prefix(&mut app, 's');
    assert_eq!(form(&app).port, "4444");
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.mode, Mode::Normal));
    assert!(matches!(app.listeners[0].state, ListenState::Listening));
}

#[tokio::test]
async fn submitting_the_form_starts_a_real_listener() {
    let (mut app, mut rx) = App::test_new();
    prefix(&mut app, 'N');
    key(&mut app, KeyCode::Tab); // -> protocol
    key(&mut app, KeyCode::Tab); // -> address
    for _ in 0..7 {
        key(&mut app, KeyCode::Backspace);
    }
    type_str(&mut app, "127.0.0.1");
    key(&mut app, KeyCode::Tab); // -> port
    for _ in 0..4 {
        key(&mut app, KeyCode::Backspace);
    }
    type_str(&mut app, "0"); // Let the OS pick a free port.
    key(&mut app, KeyCode::Enter);

    assert!(matches!(app.mode, Mode::Normal));
    assert!(matches!(app.listeners[0].state, ListenState::Binding));

    let ev = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("listener reported in time")
        .expect("event");
    app.handle(ev);
    let listener = &app.listeners[0];
    assert!(matches!(listener.state, ListenState::Listening));
    assert_ne!(listener.config.addr.port(), 0, "real port recorded");
}

#[test]
fn clicking_a_session_row_focuses_its_listener_and_tab() {
    let (mut app, _rx) = App::test_new();
    let first = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    let second = app.add_test_listener(Protocol::Tcp, "0.0.0.0:5555");
    let (_a, _out_a) = app.add_test_session(first, "10.0.0.5:1000");
    let (_b, _out_b) = app.add_test_session(second, "10.0.0.6:2000");
    let (_c, mut out_c) = app.add_test_session(second, "10.0.0.7:3000");

    render_rows(&mut app, 120, 32);
    let (rect, ..) = app.hits.sessions[2];
    click(&mut app, rect);
    assert_eq!(app.active, 1);
    assert_eq!(app.listeners[1].active_tab, 1);

    type_str(&mut app, "x");
    key(&mut app, KeyCode::Enter);
    assert_eq!(out_c.try_recv().unwrap(), b"x\n");
}

#[test]
fn clicking_tabs_and_listener_rows() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    app.add_test_listener(Protocol::Tcp, "0.0.0.0:5555");
    let _sessions: Vec<_> = (0..3)
        .map(|i| app.add_test_session(listener, &format!("10.0.0.{i}:1000")))
        .collect();

    render_rows(&mut app, 120, 32);
    let (rect, _) = app.hits.tabs[2];
    click(&mut app, rect);
    assert_eq!(app.listeners[0].active_tab, 2);

    let (rect, _) = app.hits.listeners[1];
    click(&mut app, rect);
    assert_eq!(app.active, 1);
}

/// Input arriving in one burst is handled before the next redraw, so a click
/// can land on a row that an earlier key in the burst removed.
#[test]
fn clicks_on_rows_removed_since_the_last_frame_are_ignored() {
    let (mut app, _rx) = App::test_new();
    app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:5555");
    let _sessions: Vec<_> = (0..2)
        .map(|i| app.add_test_session(listener, &format!("10.0.0.{i}:1000")))
        .collect();
    prefix(&mut app, 'j');
    render_rows(&mut app, 120, 32);
    let (last_row, _) = app.hits.listeners[1];
    let (last_session, ..) = app.hits.sessions[1];

    app.close_target(Target::Listener(listener));
    click(&mut app, last_row);
    right_click(&mut app, last_row);
    click(&mut app, last_session);

    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.active, 0);
}

#[test]
fn clicking_new_and_settings_open_the_form() {
    let (mut app, _rx) = App::test_new();
    app.add_test_listener(Protocol::Udp, "0.0.0.0:5555");
    render_rows(&mut app, 120, 32);

    let new = app.hits.new_listener;
    click(&mut app, new);
    assert_eq!(form(&app).target, None);
    key(&mut app, KeyCode::Esc);

    let settings = app.hits.settings;
    click(&mut app, settings);
    assert_eq!(form(&app).target, Some(app.listeners[0].id));
    assert_eq!(form(&app).protocol, Protocol::Udp);
}

#[test]
fn clicks_are_ignored_while_a_modal_is_open() {
    let (mut app, _rx) = App::test_new();
    app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    app.add_test_listener(Protocol::Tcp, "0.0.0.0:5555");
    render_rows(&mut app, 120, 32);
    prefix(&mut app, '?');
    let (rect, _) = app.hits.listeners[1];
    click(&mut app, rect);
    assert_eq!(app.active, 0);
    assert!(matches!(app.mode, Mode::Help));
}

// ---- listener right-click menu ----

fn three_listeners() -> (App, [ListenerId; 3]) {
    let (mut app, _rx) = App::test_new();
    let ids = [
        app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444"),
        app.add_test_listener(Protocol::Tcp, "0.0.0.0:5555"),
        app.add_test_listener(Protocol::Udp, "0.0.0.0:6666"),
    ];
    render_rows(&mut app, 120, 32);
    (app, ids)
}

fn menu(app: &App) -> &ContextMenu {
    match &app.mode {
        Mode::Menu(menu) => menu,
        _ => panic!("context menu is not open"),
    }
}

fn rename_input(app: &App) -> &str {
    match &app.mode {
        Mode::Rename(form) => form.input.as_str(),
        _ => panic!("rename overlay is not open"),
    }
}

/// Right-clicks listener `index`, renders the menu, and clicks `item`.
fn menu_pick(app: &mut App, index: usize, item: MenuItem) {
    let (row, _) = app.hits.listeners[index];
    right_click(app, row);
    render_rows(app, 120, 32);
    let pos = MenuItem::ALL.iter().position(|&i| i == item).unwrap();
    let (rect, _) = app.hits.menu_items[pos];
    click(app, rect);
}

#[test]
fn right_click_opens_menu_for_that_listener_without_focusing_it() {
    let (mut app, ids) = three_listeners();
    let (row, _) = app.hits.listeners[1];
    right_click(&mut app, row);
    assert_eq!(menu(&app).target, Target::Listener(ids[1]));
    assert_eq!((menu(&app).x, menu(&app).y), (row.x, row.y));
    assert_eq!(app.active, 0);
}

#[test]
fn right_click_elsewhere_does_nothing() {
    let (mut app, _ids) = three_listeners();
    let pane = app.hits.pane;
    right_click(&mut app, pane);
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn rename_from_menu_sets_a_custom_name() {
    let (mut app, _ids) = three_listeners();
    menu_pick(&mut app, 1, MenuItem::Rename);
    assert_eq!(
        rename_input(&app),
        "tcp :5555",
        "prefilled with the current label"
    );

    for _ in 0.."tcp :5555".len() {
        key(&mut app, KeyCode::Backspace);
    }
    type_str(&mut app, "  web server ");
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.listeners[1].name.as_deref(), Some("web server"));
    assert_eq!(app.listeners[1].label(), "web server");
    assert_eq!(app.active, 0);
}

#[test]
fn renaming_to_empty_or_the_default_clears_the_name() {
    let (mut app, _ids) = three_listeners();
    app.listeners[0].name = Some("old".into());

    prefix(&mut app, 'W');
    assert_eq!(rename_input(&app), "old");
    for _ in 0..3 {
        key(&mut app, KeyCode::Backspace);
    }
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.listeners[0].name, None);

    prefix(&mut app, 'W');
    key(&mut app, KeyCode::Enter); // Unchanged default label.
    assert_eq!(app.listeners[0].name, None);
}

#[test]
fn escape_cancels_a_rename() {
    let (mut app, _ids) = three_listeners();
    prefix(&mut app, 'W');
    type_str(&mut app, "!!!");
    key(&mut app, KeyCode::Esc);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.listeners[0].name, None);
}

#[test]
fn close_from_menu_removes_that_listener_and_keeps_focus() {
    let (mut app, ids) = three_listeners();
    let (_sess, mut out) = app.add_test_session(ids[0], "10.0.0.5:1000");
    prefix(&mut app, 'j');
    prefix(&mut app, 'j');
    render_rows(&mut app, 120, 32);
    assert_eq!(app.listeners[app.active].id, ids[2]);

    menu_pick(&mut app, 0, MenuItem::Close);
    assert!(matches!(app.mode, Mode::Confirm(Confirm::Close(t)) if t == Target::Listener(ids[0])));
    render_rows(&mut app, 120, 32);
    let confirm = app.hits.confirm;
    click(&mut app, confirm);
    assert!(matches!(app.mode, Mode::Normal));
    let remaining: Vec<ListenerId> = app.listeners.iter().map(|listener| listener.id).collect();
    assert_eq!(remaining, [ids[1], ids[2]]);
    assert_eq!(app.listeners[app.active].id, ids[2], "focus stays put");
    assert_eq!(out.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn closing_the_focused_listener_from_the_menu() {
    let (mut app, ids) = three_listeners();
    prefix(&mut app, 'j');
    render_rows(&mut app, 120, 32);
    menu_pick(&mut app, 1, MenuItem::Close);
    assert_eq!(app.listeners.len(), 2);
    assert_eq!(app.listeners[app.active].id, ids[2]);
}

#[test]
fn menu_works_from_the_keyboard() {
    let (mut app, ids) = three_listeners();
    let (row, _) = app.hits.listeners[2];
    right_click(&mut app, row);
    key(&mut app, KeyCode::Down);
    assert_eq!(menu(&app).highlighted, 1);
    key(&mut app, KeyCode::Down);
    assert_eq!(menu(&app).highlighted, 0, "wraps");
    key(&mut app, KeyCode::Up);
    key(&mut app, KeyCode::Enter); // Close
    assert!(app.listeners.iter().all(|listener| listener.id != ids[2]));

    let (row, _) = app.hits.listeners[0];
    right_click(&mut app, row);
    key(&mut app, KeyCode::Esc);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.listeners.len(), 2);
}

#[test]
fn hovering_highlights_menu_items() {
    let (mut app, _ids) = three_listeners();
    let (row, _) = app.hits.listeners[0];
    right_click(&mut app, row);
    render_rows(&mut app, 120, 32);
    let (close, _) = app.hits.menu_items[1];
    mouse(&mut app, MouseEventKind::Moved, close);
    assert_eq!(menu(&app).highlighted, 1);
}

#[test]
fn clicking_outside_the_menu_only_dismisses_it() {
    let (mut app, _ids) = three_listeners();
    let (row, _) = app.hits.listeners[0];
    right_click(&mut app, row);
    render_rows(&mut app, 120, 32);
    // The click lands on another listener row but must not focus it.
    let (other, _) = app.hits.listeners[2];
    click(&mut app, other);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.active, 0);
    assert_eq!(app.listeners.len(), 3);
}

// ---- clicking in the settings form ----

fn form_target(app: &App, want: impl Fn(FormHit) -> bool) -> Rect {
    app.hits
        .form
        .iter()
        .find(|(_, hit)| want(*hit))
        .map(|(rect, _)| *rect)
        .expect("form target rendered")
}

#[test]
fn clicking_protocol_buttons_selects_the_protocol() {
    let (mut app, _rx) = App::test_new();
    prefix(&mut app, 'N');
    render_rows(&mut app, 120, 32);

    let udp = form_target(&app, |h| matches!(h, FormHit::Protocol(Protocol::Udp)));
    // Click the last cell of the button, not just its first.
    click(
        &mut app,
        Rect {
            x: udp.right() - 1,
            ..udp
        },
    );
    assert_eq!(form(&app).protocol, Protocol::Udp);
    assert_eq!(form(&app).field, Field::Protocol);

    let tcp = form_target(&app, |h| matches!(h, FormHit::Protocol(Protocol::Tcp)));
    click(&mut app, tcp);
    assert_eq!(form(&app).protocol, Protocol::Tcp);
}

#[test]
fn clicking_a_field_lets_you_type_into_it() {
    let (mut app, _rx) = App::test_new();
    prefix(&mut app, 'N');
    render_rows(&mut app, 120, 32);

    let address = form_target(&app, |h| matches!(h, FormHit::Field(Field::Address)));
    // Anywhere on the row works, including well right of the text.
    click(
        &mut app,
        Rect {
            x: address.right() - 2,
            ..address
        },
    );
    assert_eq!(form(&app).field, Field::Address);
    for _ in 0..7 {
        key(&mut app, KeyCode::Backspace);
    }
    type_str(&mut app, "127.0.0.1");
    assert_eq!(form(&app).address, "127.0.0.1");

    let port = form_target(&app, |h| matches!(h, FormHit::Field(Field::Port)));
    click(&mut app, port);
    assert_eq!(form(&app).field, Field::Port);
    key(&mut app, KeyCode::Backspace);
    type_str(&mut app, "9");
    assert_eq!(form(&app).port, "4449");
    assert_eq!(form(&app).address, "127.0.0.1", "address untouched");
}

#[test]
fn clicks_outside_the_form_are_ignored() {
    let (mut app, _rx) = App::test_new();
    app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    render_rows(&mut app, 120, 32);
    prefix(&mut app, 's');
    render_rows(&mut app, 120, 32);
    let new = app.hits.new_listener;
    click(&mut app, new);
    assert_eq!(form(&app).target, Some(app.listeners[0].id));
}

// ---- close confirmation ----

#[test]
fn closing_a_listener_without_open_connections_skips_confirmation() {
    let (mut app, ids) = three_listeners();
    let (sess, _out) = app.add_test_session(ids[0], "10.0.0.5:1000");
    app.handle(Event::Net(NetEvent::Closed { sess, error: None }));
    prefix(&mut app, 'D');
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.listeners.len(), 2);
}

#[test]
fn cancelling_the_confirmation_keeps_everything() {
    let (mut app, ids) = three_listeners();
    let (_sess, mut out) = app.add_test_session(ids[0], "10.0.0.5:1000");

    // Escape, other keys, and clicks off the buttons don't close anything.
    prefix(&mut app, 'D');
    type_str(&mut app, "yq");
    assert!(
        matches!(app.mode, Mode::Confirm(_)),
        "other keys are ignored"
    );
    render_rows(&mut app, 120, 32);
    let pane = app.hits.pane;
    click(&mut app, pane);
    assert!(
        matches!(app.mode, Mode::Confirm(_)),
        "stray clicks are ignored"
    );
    key(&mut app, KeyCode::Esc);
    assert!(matches!(app.mode, Mode::Normal));
    assert!(!app.quit);

    // The cancel button too.
    prefix(&mut app, 'D');
    render_rows(&mut app, 120, 32);
    let cancel = app.hits.cancel;
    click(&mut app, cancel);
    assert!(matches!(app.mode, Mode::Normal));

    assert_eq!(app.listeners.len(), 3);
    assert_eq!(out.try_recv(), Err(TryRecvError::Empty));
}

/// Opens settings for the active listener and changes its port to `port`.
fn change_port(app: &mut App, port: &str) {
    prefix(app, 's');
    key(app, KeyCode::Tab); // -> address
    key(app, KeyCode::Tab); // -> port
    for _ in 0..5 {
        key(app, KeyCode::Backspace);
    }
    type_str(app, port);
}

#[tokio::test]
async fn restarting_a_udp_listener_with_sessions_asks_first() {
    let (mut app, mut rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Udp, "127.0.0.1:5555");
    let (_sess, _out) = app.add_test_session(listener, "10.0.0.5:1000");

    // Cancelling goes back to the form with the edit intact.
    change_port(&mut app, "0");
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.mode, Mode::Confirm(Confirm::Restart(..))));
    type_str(&mut app, "y");
    assert!(
        matches!(app.mode, Mode::Confirm(_)),
        "other keys are ignored"
    );
    key(&mut app, KeyCode::Esc);
    assert_eq!(form(&app).port, "0");
    assert_eq!(app.listeners[0].config.addr.port(), 5555);
    assert!(app.listeners[0].sessions[0].is_open());

    // Confirming applies it and drops the session.
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.listeners[0].config.addr.port(), 0);
    assert!(!app.listeners[0].sessions[0].is_open());
    let ev = next_event(&mut rx).await;
    app.handle(ev);
    assert!(matches!(app.listeners[0].state, ListenState::Listening));
}

#[tokio::test]
async fn restart_confirmation_buttons_work_with_the_mouse() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Quic, "127.0.0.1:4433");
    app.listeners[0].config.tls.alpn = vec!["h3".into()];
    let (_sess, _out) = app.add_test_quic_session(listener, "10.0.0.5:1000");
    change_port(&mut app, "0");
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.mode, Mode::Confirm(Confirm::Restart(..))));

    render_rows(&mut app, 120, 32);
    let cancel = app.hits.cancel;
    click(&mut app, cancel);
    assert!(matches!(app.mode, Mode::Settings(_)));

    key(&mut app, KeyCode::Enter);
    render_rows(&mut app, 120, 32);
    let confirm = app.hits.confirm;
    click(&mut app, confirm);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.listeners[0].config.addr.port(), 0);
    assert!(!app.listeners[0].sessions[0].is_open());
}

#[tokio::test]
async fn settings_that_keep_connections_apply_without_asking() {
    let (mut app, _rx) = App::test_new();
    // TCP connections outlive a listener restart.
    let tcp = app.add_test_listener(Protocol::Tcp, "127.0.0.1:4444");
    let (_a, _out_a) = app.add_test_session(tcp, "10.0.0.5:1000");
    change_port(&mut app, "0");
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.listeners[0].sessions[0].is_open());

    // An unchanged UDP listener doesn't restart.
    let udp = app.add_test_listener(Protocol::Udp, "127.0.0.1:5555");
    let (_b, _out_b) = app.add_test_session(udp, "10.0.0.5:1000");
    app.active = 1;
    prefix(&mut app, 's');
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.listeners[1].sessions[0].is_open());

    // A UDP listener whose sessions have all gone has nothing to drop.
    let idle = app.add_test_listener(Protocol::Udp, "127.0.0.1:6666");
    let (sess, _out_c) = app.add_test_session(idle, "10.0.0.6:1000");
    app.handle(Event::Net(NetEvent::Closed { sess, error: None }));
    app.active = 2;
    change_port(&mut app, "0");
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.listeners[2].config.addr.port(), 0);
}

// ---- cursor movement in text fields ----

#[test]
fn session_input_supports_cursor_movement() {
    let (mut app, _sess, mut out) = one_session();
    type_str(&mut app, "helo");
    key(&mut app, KeyCode::Left);
    type_str(&mut app, "l");
    key(&mut app, KeyCode::Home);
    key(&mut app, KeyCode::Delete);
    type_str(&mut app, "H");
    key(&mut app, KeyCode::End);
    type_str(&mut app, "!");
    key(&mut app, KeyCode::Enter);
    assert_eq!(out.try_recv().unwrap(), b"Hello!\n");
}

#[test]
fn clicking_in_the_session_input_moves_the_cursor() {
    let (mut app, _sess, mut out) = one_session();
    type_str(&mut app, "abcd");
    render_rows(&mut app, 120, 32);
    let (field, target, _) = app.hits.text[0];
    assert_eq!(target, TextTarget::Session);

    click(
        &mut app,
        Rect {
            x: field.x + 2,
            ..field
        },
    );
    type_str(&mut app, "X");
    // Past the end of the text clamps to the end.
    click(
        &mut app,
        Rect {
            x: field.right() - 1,
            ..field
        },
    );
    type_str(&mut app, "Y");
    key(&mut app, KeyCode::Enter);
    assert_eq!(out.try_recv().unwrap(), b"abXcdY\n");
}

#[test]
fn form_fields_edit_mid_text() {
    let (mut app, _rx) = App::test_new();
    prefix(&mut app, 'N');
    // Port starts focused at "4444": change the second digit.
    key(&mut app, KeyCode::Home);
    key(&mut app, KeyCode::Right);
    key(&mut app, KeyCode::Delete);
    type_str(&mut app, "3");
    assert_eq!(form(&app).port, "4344");

    // Left/Right still toggle the protocol on the protocol row.
    key(&mut app, KeyCode::Tab);
    key(&mut app, KeyCode::Right);
    assert_eq!(form(&app).protocol, Protocol::Udp);
    assert_eq!(form(&app).port, "4344");
}

#[test]
fn clicking_inside_the_address_places_the_cursor() {
    let (mut app, _rx) = App::test_new();
    prefix(&mut app, 'N');
    render_rows(&mut app, 120, 32);
    let (field, ..) = *app
        .hits
        .text
        .iter()
        .find(|(_, t, _)| *t == TextTarget::Address)
        .unwrap();

    // "0.0.0.0": click just after the first "0", swap it for "10".
    click(
        &mut app,
        Rect {
            x: field.x + 1,
            ..field
        },
    );
    assert_eq!(form(&app).field, Field::Address);
    key(&mut app, KeyCode::Backspace);
    type_str(&mut app, "10");
    assert_eq!(form(&app).address, "10.0.0.0");
    assert_eq!(form(&app).port, "4444", "port untouched");
}

#[test]
fn rename_supports_cursor_movement_and_clicks() {
    let (mut app, _ids) = three_listeners();
    prefix(&mut app, 'W');
    key(&mut app, KeyCode::Home);
    type_str(&mut app, "my ");
    assert_eq!(rename_input(&app), "my tcp :4444");

    render_rows(&mut app, 120, 32);
    let (field, target, _) = app.hits.text[0];
    assert_eq!(target, TextTarget::Rename);
    click(&mut app, field); // Start of the text.
    key(&mut app, KeyCode::Delete);
    type_str(&mut app, "M");
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.listeners[0].name.as_deref(), Some("My tcp :4444"));
}

// ---- tab right-click menu ----

/// One listener with three sessions; returns their ids and peer receivers.
fn three_tabs() -> (
    App,
    Vec<SessId>,
    Vec<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
) {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    let (ids, outs) = (1..=3)
        .map(|i| app.add_test_session(listener, &format!("10.0.0.{i}:1000")))
        .unzip();
    render_rows(&mut app, 120, 32);
    (app, ids, outs)
}

/// Right-clicks tab `index`, renders the menu, and clicks `item`.
fn tab_menu_pick(app: &mut App, index: usize, item: MenuItem) {
    let (tab, _) = app.hits.tabs[index];
    right_click(app, tab);
    render_rows(app, 120, 32);
    let pos = MenuItem::ALL.iter().position(|&i| i == item).unwrap();
    let (rect, _) = app.hits.menu_items[pos];
    click(app, rect);
}

#[test]
fn right_click_opens_menu_for_that_tab_without_focusing_it() {
    let (mut app, ids, _outs) = three_tabs();
    let (tab, _) = app.hits.tabs[2];
    right_click(&mut app, tab);
    assert_eq!(menu(&app).target, Target::Session(ids[2]));
    assert_eq!(app.listeners[0].active_tab, 0);
}

#[test]
fn close_tab_from_menu_disconnects_that_session_and_keeps_focus() {
    let (mut app, ids, mut outs) = three_tabs();
    prefix(&mut app, '3');
    render_rows(&mut app, 120, 32);

    tab_menu_pick(&mut app, 0, MenuItem::Close);
    assert!(matches!(app.mode, Mode::Confirm(Confirm::Close(t)) if t == Target::Session(ids[0])));
    assert_eq!(
        outs[0].try_recv(),
        Err(TryRecvError::Empty),
        "still connected"
    );
    render_rows(&mut app, 120, 32);
    let confirm = app.hits.confirm;
    click(&mut app, confirm);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(outs[0].try_recv(), Err(TryRecvError::Disconnected));
    let remaining: Vec<SessId> = app.listeners[0].sessions.iter().map(|s| s.id).collect();
    assert_eq!(remaining, [ids[1], ids[2]]);
    let listener = &app.listeners[0];
    assert_eq!(
        listener.sessions[listener.active_tab].id, ids[2],
        "focus stays put"
    );
    assert_eq!(outs[2].try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn closing_the_last_tab_from_the_menu_moves_focus_left() {
    let (mut app, ids, _outs) = three_tabs();
    prefix(&mut app, '3');
    render_rows(&mut app, 120, 32);
    tab_menu_pick(&mut app, 2, MenuItem::Close);
    key(&mut app, KeyCode::Enter);
    let listener = &app.listeners[0];
    assert_eq!(listener.sessions.len(), 2);
    assert_eq!(listener.sessions[listener.active_tab].id, ids[1]);
}

#[test]
fn rename_tab_from_menu_sets_a_custom_name() {
    let (mut app, _ids, _outs) = three_tabs();
    tab_menu_pick(&mut app, 1, MenuItem::Rename);
    assert_eq!(
        rename_input(&app),
        "10.0.0.2:1000",
        "prefilled with the peer"
    );

    key(&mut app, KeyCode::Home);
    type_str(&mut app, "db ");
    key(&mut app, KeyCode::Enter);
    assert_eq!(
        app.listeners[0].sessions[1].name.as_deref(),
        Some("db 10.0.0.2:1000")
    );
    assert_eq!(app.listeners[0].active_tab, 0);
}

#[test]
fn renaming_a_tab_to_empty_or_its_peer_clears_the_name() {
    let (mut app, _ids, _outs) = three_tabs();
    app.listeners[0].sessions[0].name = Some("db".into());

    prefix(&mut app, 'T');
    assert_eq!(rename_input(&app), "db");
    key(&mut app, KeyCode::Backspace);
    key(&mut app, KeyCode::Backspace);
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.listeners[0].sessions[0].name, None);

    prefix(&mut app, 'T');
    key(&mut app, KeyCode::Enter); // Unchanged peer address.
    assert_eq!(app.listeners[0].sessions[0].name, None);
}

#[test]
fn sessions_list_has_no_right_click_menu() {
    let (mut app, _ids, _outs) = three_tabs();
    let (row, ..) = app.hits.sessions[1];
    right_click(&mut app, row);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.listeners[0].active_tab, 0, "right-click doesn't focus");

    click(&mut app, row);
    assert_eq!(app.listeners[0].active_tab, 1, "left-click does");
}

#[test]
fn closing_a_disconnected_tab_skips_confirmation() {
    let (mut app, ids, _outs) = three_tabs();
    app.handle(Event::Net(NetEvent::Closed {
        sess: ids[0],
        error: None,
    }));
    prefix(&mut app, 'X');
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.listeners[0].sessions.len(), 2);

    app.handle(Event::Net(NetEvent::Closed {
        sess: ids[2],
        error: None,
    }));
    render_rows(&mut app, 120, 32);
    tab_menu_pick(&mut app, 1, MenuItem::Close);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.listeners[0].sessions.len(), 1);
}

#[test]
fn cancelling_a_tab_close_keeps_the_connection() {
    let (mut app, ids, mut outs) = three_tabs();
    prefix(&mut app, 'X');
    key(&mut app, KeyCode::Esc);
    tab_menu_pick(&mut app, 1, MenuItem::Close);
    render_rows(&mut app, 120, 32);
    let cancel = app.hits.cancel;
    click(&mut app, cancel);

    assert!(matches!(app.mode, Mode::Normal));
    let remaining: Vec<SessId> = app.listeners[0].sessions.iter().map(|s| s.id).collect();
    assert_eq!(remaining, ids);
    for out in &mut outs {
        assert_eq!(out.try_recv(), Err(TryRecvError::Empty));
    }
}

#[test]
fn a_tab_that_disconnects_while_asking_still_closes_on_confirm() {
    let (mut app, ids, _outs) = three_tabs();
    prefix(&mut app, 'X');
    app.handle(Event::Net(NetEvent::Closed {
        sess: ids[0],
        error: None,
    }));
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.listeners[0].sessions.len(), 2);
}

// ---- dragging the sidebar dividers ----

fn at(column: u16, row: u16) -> Rect {
    Rect::new(column, row, 1, 1)
}

fn drag_to(app: &mut App, column: u16, row: u16) {
    mouse(
        app,
        MouseEventKind::Drag(MouseButton::Left),
        at(column, row),
    );
}

fn release(app: &mut App, column: u16, row: u16) {
    mouse(app, MouseEventKind::Up(MouseButton::Left), at(column, row));
}

#[test]
fn dragging_the_vertical_divider_resizes_the_sidebar() {
    let (mut app, _ids) = three_listeners();
    let divider = app.hits.sidebar_divider;
    assert_eq!((divider.x, divider.height), (25, 32));

    click(&mut app, Rect { y: 10, ..divider });
    assert_eq!(app.sidebar_width, 26, "grabbing alone doesn't move it");
    drag_to(&mut app, 30, 12);
    assert_eq!(app.sidebar_width, 31);
    release(&mut app, 30, 12);

    render_rows(&mut app, 120, 32);
    assert_eq!(app.hits.sidebar_divider.x, 30);
    drag_to(&mut app, 20, 12);
    assert_eq!(app.sidebar_width, 31, "no drag after release");
}

#[test]
fn sidebar_width_is_clamped() {
    let (mut app, _ids) = three_listeners();
    let divider = app.hits.sidebar_divider;
    click(&mut app, divider);
    drag_to(&mut app, 2, 0);
    assert_eq!(app.sidebar_width, 18);
    drag_to(&mut app, 100, 0);
    assert_eq!(app.sidebar_width, 36);
}

#[test]
fn double_clicking_the_vertical_divider_restores_the_default_width() {
    let (mut app, _ids) = three_listeners();
    let divider = app.hits.sidebar_divider;
    click(&mut app, divider);
    drag_to(&mut app, 33, 5);
    release(&mut app, 33, 5);
    render_rows(&mut app, 120, 32);
    assert_eq!(app.sidebar_width, 34);

    // A click long after the last one only grabs the divider.
    app.last_divider_click = Some(Instant::now() - Duration::from_secs(1));
    let divider = app.hits.sidebar_divider;
    click(&mut app, divider);
    release(&mut app, 33, 5);
    assert_eq!(app.sidebar_width, 34);

    let divider = app.hits.sidebar_divider;

    click(&mut app, divider);
    assert_eq!(app.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
}

#[test]
fn dragging_the_horizontal_divider_moves_the_section_split() {
    let (mut app, _ids) = three_listeners();
    let divider = app.hits.section_divider;
    assert_eq!((divider.y, divider.width), (16, 25));

    click(&mut app, Rect { x: 8, ..divider });
    drag_to(&mut app, 8, 24);
    release(&mut app, 8, 24);
    assert_eq!(app.sidebar_split, 0.75);
    render_rows(&mut app, 120, 32);
    assert_eq!(app.hits.section_divider.y, 24);
}

#[test]
fn section_split_keeps_both_sections_visible() {
    let (mut app, _ids) = three_listeners();
    let divider = app.hits.section_divider;
    click(&mut app, divider);
    drag_to(&mut app, 5, 0);
    render_rows(&mut app, 120, 32);
    assert_eq!(app.hits.section_divider.y, 3, "listeners keep 3 rows");

    drag_to(&mut app, 5, 31);
    render_rows(&mut app, 120, 32);
    assert_eq!(app.hits.section_divider.y, 29, "sessions keep 3 rows");
}

#[test]
fn dragging_across_other_targets_does_not_click_them() {
    let (mut app, listener) = three_listeners();
    let (_sess, _out) = app.add_test_session(listener[0], "10.0.0.5:1000");
    render_rows(&mut app, 120, 32);
    let other = app.hits.listeners[2].0;
    let tab = app.hits.tabs[0].0;

    let divider = app.hits.section_divider;

    click(&mut app, divider);
    drag_to(&mut app, other.x, other.y);
    release(&mut app, tab.x, tab.y);
    assert_eq!(app.active, 0);
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn drag_events_without_a_grab_do_nothing() {
    let (mut app, _ids) = three_listeners();
    drag_to(&mut app, 30, 10);
    release(&mut app, 30, 10);
    assert_eq!(app.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
    assert_eq!(app.sidebar_split, 0.5);
}

// ---- TLS ----

/// Opens the new-listener form with TLS picked, bound to 127.0.0.1 on an
/// OS-chosen port. Leaves focus on the address field.
fn open_tls_form(app: &mut App) {
    prefix(app, 'N');
    for _ in 0..4 {
        key(app, KeyCode::Backspace);
    }
    type_str(app, "0");
    key(app, KeyCode::Tab); // -> protocol
    key(app, KeyCode::Right);
    key(app, KeyCode::Right); // -> tls
    key(app, KeyCode::Tab); // -> address
    for _ in 0..7 {
        key(app, KeyCode::Backspace);
    }
    type_str(app, "127.0.0.1");
}

async fn next_event(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("listener reported in time")
        .expect("event")
}

fn fingerprint(app: &App) -> String {
    app.listeners[0]
        .tls
        .as_ref()
        .expect("TLS listener has a certificate")
        .fingerprint
        .clone()
}

#[test]
fn protocol_arrows_cycle_through_every_protocol() {
    let (mut app, _rx) = App::test_new();
    prefix(&mut app, 'N');
    key(&mut app, KeyCode::Tab); // -> protocol
    for want in [Protocol::Udp, Protocol::Tls, Protocol::Quic, Protocol::Tcp] {
        key(&mut app, KeyCode::Right);
        assert_eq!(form(&app).protocol, want);
    }
    key(&mut app, KeyCode::Left);
    assert_eq!(form(&app).protocol, Protocol::Quic);
}

#[test]
fn tls_adds_cert_and_alpn_fields_to_the_tab_order() {
    let (mut app, _rx) = App::test_new();
    prefix(&mut app, 'N');
    key(&mut app, KeyCode::Tab); // -> protocol
    use Field::*;
    assert_eq!(form(&app).fields(), [Protocol, Address, Port]);
    key(&mut app, KeyCode::Right);
    key(&mut app, KeyCode::Right); // -> tls
    assert_eq!(form(&app).fields(), [Protocol, Address, Port, Cert, Alpn]);

    for _ in 0..3 {
        key(&mut app, KeyCode::Tab);
    }
    assert_eq!(form(&app).field, Cert);
    key(&mut app, KeyCode::Right);
    assert_eq!(form(&app).cert, CertKind::Files);
    assert_eq!(
        form(&app).fields(),
        [Protocol, Address, Port, Cert, CertFile, KeyFile, Alpn]
    );

    key(&mut app, KeyCode::Tab);
    type_str(&mut app, "~/certs/a b.pem");
    key(&mut app, KeyCode::Tab);
    type_str(&mut app, "key.pem");
    key(&mut app, KeyCode::Tab);
    type_str(&mut app, "h2, http/1.1");
    assert_eq!(
        form(&app).cert_file,
        "~/certs/a b.pem",
        "paths take any text"
    );
    assert_eq!(form(&app).key_file, "key.pem");
    assert_eq!(form(&app).alpn, "h2, http/1.1");

    key(&mut app, KeyCode::Tab);
    assert_eq!(form(&app).field, Protocol, "wraps to the top");
    key(&mut app, KeyCode::BackTab);
    assert_eq!(form(&app).field, Alpn);
}

#[test]
fn cert_files_are_required_and_must_exist() {
    let (mut app, _rx) = App::test_new();
    prefix(&mut app, 'N');
    key(&mut app, KeyCode::Tab); // -> protocol
    key(&mut app, KeyCode::Right);
    key(&mut app, KeyCode::Right); // -> tls
    for _ in 0..3 {
        key(&mut app, KeyCode::Tab); // -> cert
    }
    key(&mut app, KeyCode::Right); // -> files
    key(&mut app, KeyCode::Enter);
    assert_eq!(form(&app).error.as_deref(), Some("cert file is required"));

    key(&mut app, KeyCode::Tab);
    type_str(&mut app, "/nonexistent/outpost/cert.pem");
    key(&mut app, KeyCode::Enter);
    assert_eq!(form(&app).error.as_deref(), Some("key file is required"));

    key(&mut app, KeyCode::Tab);
    type_str(&mut app, "/nonexistent/outpost/key.pem");
    key(&mut app, KeyCode::Enter);
    let error = form(&app).error.clone().unwrap();
    assert!(error.starts_with("cert file: "), "{error}");
    assert!(app.listeners.is_empty());
}

#[tokio::test]
async fn submitting_a_tls_form_serves_a_self_signed_certificate() {
    let (mut app, mut rx) = App::test_new();
    open_tls_form(&mut app);
    key(&mut app, KeyCode::Tab); // -> port
    key(&mut app, KeyCode::Tab); // -> cert
    key(&mut app, KeyCode::Tab); // -> alpn
    type_str(&mut app, " h2 ,, http/1.1 ");
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.mode, Mode::Normal));

    let listener = &app.listeners[0];
    assert_eq!(listener.config.protocol, Protocol::Tls);
    assert_eq!(
        listener.config.tls,
        TlsConfig {
            cert: CertSource::SelfSigned,
            alpn: vec!["h2".into(), "http/1.1".into()],
        }
    );
    let tls = listener.tls.as_ref().unwrap();
    assert_eq!(
        tls.config.alpn_protocols,
        [b"h2".to_vec(), b"http/1.1".to_vec()]
    );
    assert_eq!(tls.fingerprint.len(), 64);

    let ev = next_event(&mut rx).await;
    app.handle(ev);
    let listener = &app.listeners[0];
    assert!(matches!(listener.state, ListenState::Listening));
    assert!(
        listener.endpoint().starts_with("tls :"),
        "{}",
        listener.endpoint()
    );
}

#[tokio::test]
async fn tls_edits_keep_the_certificate_unless_tls_settings_change() {
    let (mut app, mut rx) = App::test_new();
    open_tls_form(&mut app);
    key(&mut app, KeyCode::Enter);
    let ev = next_event(&mut rx).await;
    app.handle(ev);
    let original = fingerprint(&app);

    // A new port restarts the listener on the same certificate.
    prefix(&mut app, 's');
    key(&mut app, KeyCode::Tab); // -> address
    key(&mut app, KeyCode::Tab); // -> port
    for _ in 0..5 {
        key(&mut app, KeyCode::Backspace);
    }
    type_str(&mut app, "0");
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.listeners[0].state, ListenState::Binding));
    let ev = next_event(&mut rx).await;
    app.handle(ev);
    assert_eq!(fingerprint(&app), original);

    // Changing ALPN builds a new one.
    prefix(&mut app, 's');
    for _ in 0..4 {
        key(&mut app, KeyCode::Tab); // -> alpn
    }
    type_str(&mut app, "h2");
    key(&mut app, KeyCode::Enter);
    assert_ne!(fingerprint(&app), original);
}

#[test]
fn settings_show_a_tls_listener_as_configured() {
    let (mut app, _rx) = App::test_new();
    app.add_test_listener(Protocol::Tls, "0.0.0.0:8443");
    app.listeners[0].config.tls = TlsConfig {
        cert: CertSource::Files {
            cert: "/etc/outpost/cert.pem".into(),
            key: "/etc/outpost/key.pem".into(),
        },
        alpn: vec!["h2".into(), "http/1.1".into()],
    };
    prefix(&mut app, 's');
    let form = form(&app);
    assert_eq!(form.protocol, Protocol::Tls);
    assert_eq!(form.cert, CertKind::Files);
    assert_eq!(form.cert_file, "/etc/outpost/cert.pem");
    assert_eq!(form.key_file, "/etc/outpost/key.pem");
    assert_eq!(form.alpn, "h2, http/1.1");
}

#[test]
fn home_relative_cert_paths_expand() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let input = TextInput::from("~/certs/cert.pem".to_string());
    assert_eq!(
        form_path(&input),
        Some(PathBuf::from(home).join("certs/cert.pem"))
    );
    assert_eq!(
        form_path(&TextInput::from(" ./cert.pem ".to_string())),
        Some(PathBuf::from("./cert.pem"))
    );
    assert_eq!(form_path(&TextInput::default()), None);
}

#[test]
fn a_tls_handshake_adds_an_info_line() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Tls, "0.0.0.0:8443");
    let (sess, _out) = app.add_test_session(listener, "10.0.0.5:1000");
    app.handle(Event::Net(NetEvent::Secured {
        sess,
        summary: "tls1.3 · alpn h2".into(),
    }));
    assert_eq!(
        transcript(&app, 0, 0).last(),
        Some(&(Dir::Info, "tls1.3 · alpn h2".to_string()))
    );
}

#[test]
fn clicking_cert_buttons_and_path_fields() {
    let (mut app, _rx) = App::test_new();
    prefix(&mut app, 'N');
    key(&mut app, KeyCode::Tab); // -> protocol
    key(&mut app, KeyCode::Right);
    key(&mut app, KeyCode::Right); // -> tls
    render_rows(&mut app, 120, 32);

    let files = form_target(&app, |h| matches!(h, FormHit::Cert(CertKind::Files)));
    click(&mut app, files);
    assert_eq!(form(&app).cert, CertKind::Files);
    assert_eq!(form(&app).field, Field::Cert);

    render_rows(&mut app, 120, 32);
    let cert_file = form_target(&app, |h| matches!(h, FormHit::Field(Field::CertFile)));
    click(&mut app, cert_file);
    assert_eq!(form(&app).field, Field::CertFile);
    type_str(&mut app, "a.pem");
    assert_eq!(form(&app).cert_file, "a.pem");

    let self_signed = form_target(&app, |h| matches!(h, FormHit::Cert(CertKind::SelfSigned)));
    click(&mut app, self_signed);
    assert_eq!(form(&app).cert, CertKind::SelfSigned);
}

// ---- QUIC ----

fn stream_data(app: &mut App, sess: SessId, stream: StreamId, bytes: &[u8]) {
    app.handle(Event::Net(NetEvent::StreamData {
        sess,
        stream,
        bytes: bytes.to_vec(),
    }));
}

fn prefix_key(app: &mut App, code: KeyCode) {
    app.handle(Event::Input(TermEvent::Key(KeyEvent::new(
        KeyCode::Char('b'),
        KeyModifiers::CONTROL,
    ))));
    key(app, code);
}

fn ctrl_d(app: &mut App) {
    app.handle(Event::Input(TermEvent::Key(KeyEvent::new(
        KeyCode::Char('d'),
        KeyModifiers::CONTROL,
    ))));
}

fn streams(app: &App) -> &Streams {
    app.listeners[0].sessions[0]
        .streams
        .as_ref()
        .expect("QUIC session")
}

fn write(stream: StreamId, text: &str) -> QuicOut {
    QuicOut::Write {
        stream,
        bytes: text.as_bytes().to_vec(),
    }
}

/// Transcript lines as (dir, stream, text), skipping the connect line.
fn stream_lines(app: &App) -> Vec<(Dir, Option<StreamId>, String)> {
    app.listeners[0].sessions[0].lines[1..]
        .iter()
        .map(|line| (line.dir, line.stream, line.text.clone()))
        .collect()
}

#[test]
fn quic_replies_go_to_the_stream_that_last_sent() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Quic, "0.0.0.0:4433");
    let (sess, mut out) = app.add_test_quic_session(listener, "10.0.0.5:1000");
    assert_eq!(streams(&app).send_to, None, "nothing to reply to yet");

    stream_data(&mut app, sess, 0, b"GET /\r\n");
    assert_eq!(streams(&app).send_to, Some(0));
    type_str(&mut app, "hello");
    key(&mut app, KeyCode::Enter);
    assert_eq!(out.try_recv().unwrap(), write(0, "hello\n"));

    stream_data(&mut app, sess, 4, b"again\n");
    assert_eq!(streams(&app).send_to, Some(4));
    type_str(&mut app, "hi");
    key(&mut app, KeyCode::Enter);
    assert_eq!(out.try_recv().unwrap(), write(4, "hi\n"));

    assert_eq!(
        stream_lines(&app),
        [
            (Dir::In, Some(0), "GET /".into()),
            (Dir::Out, Some(0), "hello".into()),
            (Dir::In, Some(4), "again".into()),
            (Dir::Out, Some(4), "hi".into()),
        ]
    );
    assert!(streams(&app).tagged());
}

#[test]
fn quic_one_way_streams_are_not_reply_targets() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Quic, "0.0.0.0:4433");
    let (sess, _out) = app.add_test_quic_session(listener, "10.0.0.5:1000");
    stream_data(&mut app, sess, 0, b"a\n");
    stream_data(&mut app, sess, 2, b"b\n");
    assert_eq!(streams(&app).send_to, Some(0));
    assert_eq!(streams(&app).seen, [0, 2]);
    assert_eq!(streams(&app).writable, [0]);
}

#[test]
fn quic_new_streams_take_the_next_server_ids() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Quic, "0.0.0.0:4433");
    let (_sess, mut out) = app.add_test_quic_session(listener, "10.0.0.5:1000");

    type_str(&mut app, "first");
    key(&mut app, KeyCode::Enter);
    assert_eq!(out.try_recv().unwrap(), write(1, "first\n"));
    assert_eq!(streams(&app).send_to, Some(1), "then sticks to it");

    prefix_key(&mut app, KeyCode::Tab); // -> new
    assert_eq!(streams(&app).send_to, None);
    type_str(&mut app, "second");
    key(&mut app, KeyCode::Enter);
    assert_eq!(out.try_recv().unwrap(), write(5, "second\n"));
}

#[test]
fn quic_prefix_tab_cycles_streams_then_new() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Quic, "0.0.0.0:4433");
    let (sess, _out) = app.add_test_quic_session(listener, "10.0.0.5:1000");
    stream_data(&mut app, sess, 0, b"a\n");
    stream_data(&mut app, sess, 4, b"b\n");
    for want in [None, Some(0), Some(4)] {
        prefix_key(&mut app, KeyCode::Tab);
        assert_eq!(streams(&app).send_to, want);
    }
    prefix_key(&mut app, KeyCode::BackTab);
    assert_eq!(streams(&app).send_to, Some(0));
}

#[test]
fn quic_ctrl_d_finishes_the_send_stream() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Quic, "0.0.0.0:4433");
    let (sess, mut out) = app.add_test_quic_session(listener, "10.0.0.5:1000");
    stream_data(&mut app, sess, 0, b"a\n");
    stream_data(&mut app, sess, 4, b"b\n");

    type_str(&mut app, "x");
    ctrl_d(&mut app);
    assert!(out.try_recv().is_err(), "only on an empty line");

    key(&mut app, KeyCode::Backspace);
    ctrl_d(&mut app);
    assert_eq!(out.try_recv().unwrap(), QuicOut::Finish(4));
    assert_eq!(streams(&app).writable, [0]);
    assert_eq!(
        streams(&app).send_to,
        Some(0),
        "falls back to the newest left"
    );
    assert_eq!(
        stream_lines(&app).last().unwrap(),
        &(Dir::Info, Some(4), "finished sending".into())
    );

    // Data on a finished stream doesn't make it a reply target again.
    stream_data(&mut app, sess, 4, b"late\n");
    assert_eq!(streams(&app).send_to, Some(0));
}

#[test]
fn quic_stream_endings_are_noted() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Quic, "0.0.0.0:4433");
    let (sess, _out) = app.add_test_quic_session(listener, "10.0.0.5:1000");
    stream_data(&mut app, sess, 0, b"a\n");
    stream_data(&mut app, sess, 4, b"b\n");
    for (stream, end) in [
        (0, StreamEnd::PeerFinished),
        (4, StreamEnd::PeerReset(9)),
        (
            4,
            StreamEnd::WriteFailed("sending stopped by peer: error 5".into()),
        ),
    ] {
        app.handle(Event::Net(NetEvent::StreamEnded { sess, stream, end }));
    }
    let lines = stream_lines(&app);
    assert_eq!(
        lines[2..],
        [
            (Dir::Info, Some(0), "peer finished sending".into()),
            (Dir::Info, Some(4), "reset by peer (code 9)".into()),
            (
                Dir::Info,
                Some(4),
                "can't send: sending stopped by peer: error 5".into()
            ),
        ]
    );
    assert_eq!(streams(&app).writable, [0], "still writable after peer FIN");
    assert_eq!(streams(&app).send_to, Some(0));
}

#[test]
fn quic_interleaved_streams_keep_their_own_lines() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Quic, "0.0.0.0:4433");
    let (sess, _out) = app.add_test_quic_session(listener, "10.0.0.5:1000");
    stream_data(&mut app, sess, 0, b"GE");
    stream_data(&mut app, sess, 4, b"x");
    stream_data(&mut app, sess, 4, b"y\n");
    stream_data(&mut app, sess, 0, b"T\n");
    assert_eq!(
        stream_lines(&app),
        [
            (Dir::In, Some(0), "GE".into()),
            (Dir::In, Some(4), "xy".into()),
            (Dir::In, Some(0), "T".into()),
        ]
    );
}

#[test]
fn quic_stream_controls_work_with_the_mouse() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Quic, "0.0.0.0:4433");
    let (sess, mut out) = app.add_test_quic_session(listener, "10.0.0.5:1000");
    stream_data(&mut app, sess, 0, b"a\n");

    render_rows(&mut app, 120, 32);
    let finish = app.hits.finish_stream;
    click(&mut app, finish);
    assert_eq!(out.try_recv().unwrap(), QuicOut::Finish(0));
    assert_eq!(streams(&app).send_to, None);

    stream_data(&mut app, sess, 4, b"b\n");
    render_rows(&mut app, 120, 32);
    assert_eq!(app.hits.finish_stream.width, 5);
    let target = app.hits.send_stream;
    click(&mut app, target);
    assert_eq!(streams(&app).send_to, None, "s4 -> new");
    render_rows(&mut app, 120, 32);
    assert_eq!(app.hits.finish_stream.width, 0, "nothing to finish on new");
}

#[test]
fn quic_needs_alpn() {
    let (mut app, _rx) = App::test_new();
    prefix(&mut app, 'N');
    key(&mut app, KeyCode::Tab); // -> protocol
    key(&mut app, KeyCode::Left); // tcp wraps round to quic
    assert_eq!(form(&app).protocol, Protocol::Quic);
    assert_eq!(
        form(&app).fields(),
        [
            Field::Protocol,
            Field::Address,
            Field::Port,
            Field::Cert,
            Field::Alpn
        ]
    );
    key(&mut app, KeyCode::Enter);
    assert_eq!(
        form(&app).error.as_deref(),
        Some("quic needs an alpn protocol, e.g. h3")
    );
    assert!(app.listeners.is_empty());
}

#[tokio::test]
async fn switching_tls_to_quic_keeps_the_certificate() {
    let (mut app, mut rx) = App::test_new();
    open_tls_form(&mut app);
    key(&mut app, KeyCode::Tab); // -> port
    key(&mut app, KeyCode::Tab); // -> cert
    key(&mut app, KeyCode::Tab); // -> alpn
    type_str(&mut app, "h3");
    key(&mut app, KeyCode::Enter);
    let ev = next_event(&mut rx).await;
    app.handle(ev);
    let original = fingerprint(&app);

    prefix(&mut app, 's');
    key(&mut app, KeyCode::Right); // tls -> quic
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.mode, Mode::Normal));
    let ev = next_event(&mut rx).await;
    app.handle(ev);
    let listener = &app.listeners[0];
    assert_eq!(listener.config.protocol, Protocol::Quic);
    assert!(matches!(listener.state, ListenState::Listening));
    assert_eq!(fingerprint(&app), original);
}

#[test]
fn stopping_a_quic_listener_closes_its_sessions() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Quic, "0.0.0.0:4433");
    let (_sess, _out) = app.add_test_quic_session(listener, "10.0.0.5:1000");
    app.listeners[0].stop();
    assert!(!app.listeners[0].sessions[0].is_open());
}
