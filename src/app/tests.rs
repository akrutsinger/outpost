//! Behavior tests: drive `App::handle` with keys, clicks, and network events,
//! then check state and what a session's peer would receive.

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use tokio::sync::mpsc::error::TryRecvError;

use super::*;
use crate::ui::render_rows;

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

fn click(app: &mut App, rect: Rect) {
    app.handle(Event::Input(TermEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x,
        row: rect.y,
        modifiers: KeyModifiers::NONE,
    })));
}

fn data(app: &mut App, sess: SessId, bytes: &[u8]) {
    app.handle(Event::Net(NetEvent::Data {
        sess,
        bytes: bytes.to_vec(),
    }));
}

fn transcript(app: &App, ws: usize, tab: usize) -> Vec<(Dir, String)> {
    app.workspaces[ws].sessions[tab]
        .lines
        .iter()
        .map(|line| (line.dir, line.text.clone()))
        .collect()
}

fn incoming(app: &App, ws: usize, tab: usize) -> Vec<String> {
    transcript(app, ws, tab)
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

/// One TCP workspace on :4444 with a single focused session.
fn one_session() -> (App, SessId, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
    let (mut app, _rx) = App::test_new();
    let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
    let (sess, out) = app.add_test_session(ws, "10.0.0.5:51234");
    (app, sess, out)
}

#[test]
fn incoming_data_splits_lines_and_drops_carriage_returns() {
    let (mut app, sess, _out) = one_session();
    data(&mut app, sess, b"one\r\ntwo\n\nthree\n");
    assert_eq!(incoming(&app, 0, 0), ["one", "two", "", "three"]);
    assert_eq!(app.workspaces[0].sessions[0].bytes_in, 16);
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
    assert_eq!(app.workspaces[0].sessions[0].lines.len(), MAX_LINES);
}

#[test]
fn enter_sends_input_line_to_focused_session() {
    let (mut app, _sess, mut out) = one_session();
    type_str(&mut app, "hi there");
    key(&mut app, KeyCode::Backspace);
    key(&mut app, KeyCode::Enter);

    assert_eq!(out.try_recv().unwrap(), b"hi ther\n");
    let s = &app.workspaces[0].sessions[0];
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

    let s = &app.workspaces[0].sessions[0];
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
    let s = &app.workspaces[0].sessions[0];
    assert!(!s.is_open());
    assert_eq!(s.bytes_out, 0);
}

#[test]
fn background_activity_is_unread_until_focused() {
    let (mut app, _rx) = App::test_new();
    let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
    let (first, _a) = app.add_test_session(ws, "10.0.0.5:1000");
    let (second, _b) = app.add_test_session(ws, "10.0.0.6:2000");

    // A brand-new background connection counts as unread; the focused one doesn't.
    assert!(!app.workspaces[0].sessions[0].unread);
    assert!(app.workspaces[0].sessions[1].unread);
    assert_eq!(app.workspaces[0].status(), Status::Unread);

    prefix(&mut app, 'n');
    assert!(!app.workspaces[0].sessions[1].unread);

    data(&mut app, first, b"ping\n");
    data(&mut app, second, b"pong\n");
    assert!(app.workspaces[0].sessions[0].unread);
    assert!(!app.workspaces[0].sessions[1].unread);
}

#[test]
fn prefix_keys_switch_tabs() {
    let (mut app, _rx) = App::test_new();
    let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
    let _sessions: Vec<_> = (0..3)
        .map(|i| app.add_test_session(ws, &format!("10.0.0.{i}:1000")))
        .collect();

    prefix(&mut app, 'n');
    assert_eq!(app.workspaces[0].active_tab, 1);
    prefix(&mut app, 'p');
    prefix(&mut app, 'p');
    assert_eq!(app.workspaces[0].active_tab, 2, "wraps backwards");
    prefix(&mut app, '1');
    assert_eq!(app.workspaces[0].active_tab, 0);
    prefix(&mut app, '9');
    assert_eq!(app.workspaces[0].active_tab, 0, "out of range is ignored");
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn prefix_keys_switch_workspaces() {
    let (mut app, _rx) = App::test_new();
    app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
    app.add_test_workspace(Protocol::Udp, "0.0.0.0:5555");
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
    let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
    let (_a, _out_a) = app.add_test_session(ws, "10.0.0.5:1000");
    let (b, mut out_b) = app.add_test_session(ws, "10.0.0.6:2000");
    prefix(&mut app, 'n');

    prefix(&mut app, 'X');
    // Dropping the outlet is what tells the connection task to hang up.
    assert_eq!(out_b.try_recv(), Err(TryRecvError::Disconnected));
    assert_eq!(app.workspaces[0].sessions.len(), 1);
    assert_eq!(app.workspaces[0].active_tab, 0);

    // A late event for the removed session is ignored.
    data(&mut app, b, b"late\n");
    assert_eq!(incoming(&app, 0, 0), Vec::<String>::new());
}

#[test]
fn close_workspace_drops_its_sessions() {
    let (mut app, _rx) = App::test_new();
    app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
    let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:5555");
    let (_sess, mut out) = app.add_test_session(ws, "10.0.0.5:1000");
    prefix(&mut app, 'j');

    prefix(&mut app, 'D');
    assert_eq!(out.try_recv(), Err(TryRecvError::Disconnected));
    assert_eq!(app.workspaces.len(), 1);
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
fn datagrams_map_to_one_session_per_peer() {
    let (mut app, _rx) = App::test_new();
    let ws = app.add_test_workspace(Protocol::Udp, "0.0.0.0:5555");
    let (udp_tx, mut udp_rx) = tokio::sync::mpsc::unbounded_channel();
    app.handle(Event::Net(NetEvent::Listening {
        ws,
        local: "0.0.0.0:5555".parse().unwrap(),
        udp: Some(udp_tx),
    }));

    let peer_a: SocketAddr = "10.0.0.5:1000".parse().unwrap();
    let peer_b: SocketAddr = "10.0.0.6:2000".parse().unwrap();
    for (peer, bytes) in [(peer_a, "one\n"), (peer_b, "two\n"), (peer_a, "three\n")] {
        app.handle(Event::Net(NetEvent::Datagram {
            ws,
            peer,
            bytes: bytes.as_bytes().to_vec(),
        }));
    }
    assert_eq!(app.workspaces[0].sessions.len(), 2);
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
    let ws = app.add_test_workspace(Protocol::Udp, "0.0.0.0:5555");
    app.handle(Event::Net(NetEvent::Datagram {
        ws,
        peer: "10.0.0.5:1000".parse().unwrap(),
        bytes: b"early".to_vec(),
    }));
    assert!(app.workspaces[0].sessions.is_empty());
}

#[test]
fn new_workspace_form_suggests_the_next_free_port() {
    let (mut app, _rx) = App::test_new();
    prefix(&mut app, 'N');
    assert_eq!(form(&app).port, "4444");
    key(&mut app, KeyCode::Esc);

    app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
    app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4445");
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
    assert!(app.workspaces.is_empty());
}

#[test]
fn duplicate_listener_is_rejected() {
    let (mut app, _rx) = App::test_new();
    app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
    prefix(&mut app, 'N');
    key(&mut app, KeyCode::Backspace);
    type_str(&mut app, "4");
    key(&mut app, KeyCode::Enter);
    assert_eq!(
        form(&app).error.as_deref(),
        Some("a workspace already uses that address")
    );
    assert_eq!(app.workspaces.len(), 1);
}

#[test]
fn settings_without_changes_leave_the_workspace_alone() {
    let (mut app, _rx) = App::test_new();
    app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
    prefix(&mut app, 's');
    assert_eq!(form(&app).port, "4444");
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.mode, Mode::Normal));
    assert!(matches!(app.workspaces[0].state, ListenState::Listening));
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
    assert!(matches!(app.workspaces[0].state, ListenState::Binding));

    let ev = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("listener reported in time")
        .expect("event");
    app.handle(ev);
    let ws = &app.workspaces[0];
    assert!(matches!(ws.state, ListenState::Listening));
    assert_ne!(ws.config.addr.port(), 0, "real port recorded");
}

#[test]
fn clicking_a_session_row_focuses_its_workspace_and_tab() {
    let (mut app, _rx) = App::test_new();
    let first = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
    let second = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:5555");
    let (_a, _out_a) = app.add_test_session(first, "10.0.0.5:1000");
    let (_b, _out_b) = app.add_test_session(second, "10.0.0.6:2000");
    let (_c, mut out_c) = app.add_test_session(second, "10.0.0.7:3000");

    render_rows(&mut app, 120, 32);
    let (rect, ..) = app.hits.sessions[2];
    click(&mut app, rect);
    assert_eq!(app.active, 1);
    assert_eq!(app.workspaces[1].active_tab, 1);

    type_str(&mut app, "x");
    key(&mut app, KeyCode::Enter);
    assert_eq!(out_c.try_recv().unwrap(), b"x\n");
}

#[test]
fn clicking_tabs_and_workspace_rows() {
    let (mut app, _rx) = App::test_new();
    let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
    app.add_test_workspace(Protocol::Tcp, "0.0.0.0:5555");
    let _sessions: Vec<_> = (0..3)
        .map(|i| app.add_test_session(ws, &format!("10.0.0.{i}:1000")))
        .collect();

    render_rows(&mut app, 120, 32);
    let (rect, _) = app.hits.tabs[2];
    click(&mut app, rect);
    assert_eq!(app.workspaces[0].active_tab, 2);

    let (rect, _) = app.hits.workspaces[1];
    click(&mut app, rect);
    assert_eq!(app.active, 1);
}

#[test]
fn clicking_new_and_settings_open_the_form() {
    let (mut app, _rx) = App::test_new();
    app.add_test_workspace(Protocol::Udp, "0.0.0.0:5555");
    render_rows(&mut app, 120, 32);

    let new = app.hits.new_workspace;
    click(&mut app, new);
    assert_eq!(form(&app).target, None);
    key(&mut app, KeyCode::Esc);

    let settings = app.hits.settings;
    click(&mut app, settings);
    assert_eq!(form(&app).target, Some(app.workspaces[0].id));
    assert_eq!(form(&app).protocol, Protocol::Udp);
}

#[test]
fn clicks_are_ignored_while_a_modal_is_open() {
    let (mut app, _rx) = App::test_new();
    app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
    app.add_test_workspace(Protocol::Tcp, "0.0.0.0:5555");
    render_rows(&mut app, 120, 32);
    prefix(&mut app, '?');
    let (rect, _) = app.hits.workspaces[1];
    click(&mut app, rect);
    assert_eq!(app.active, 0);
    assert!(matches!(app.mode, Mode::Help));
}
