//! Behavior tests: drive `App::handle` with keys, clicks, and network events,
//! then check state and what a session's peer would receive.

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use tokio::sync::mpsc::error::TryRecvError;

use super::*;
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
    assert!(matches!(app.mode, Mode::ConfirmClose(t) if t == Target::Session(b)));
    assert_eq!(
        out_b.try_recv(),
        Err(TryRecvError::Empty),
        "still connected"
    );
    key(&mut app, KeyCode::Enter);
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
    assert!(matches!(app.mode, Mode::ConfirmClose(t) if t == Target::Workspace(ws)));
    assert!(
        out.try_recv().is_err_and(|e| e == TryRecvError::Empty),
        "still connected"
    );
    key(&mut app, KeyCode::Enter);
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

// ---- workspace right-click menu ----

fn three_workspaces() -> (App, [WsId; 3]) {
    let (mut app, _rx) = App::test_new();
    let ids = [
        app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444"),
        app.add_test_workspace(Protocol::Tcp, "0.0.0.0:5555"),
        app.add_test_workspace(Protocol::Udp, "0.0.0.0:6666"),
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

/// Right-clicks workspace `index`, renders the menu, and clicks `item`.
fn menu_pick(app: &mut App, index: usize, item: &str) {
    let (row, _) = app.hits.workspaces[index];
    right_click(app, row);
    render_rows(app, 120, 32);
    let pos = MENU_ITEMS.iter().position(|i| *i == item).unwrap();
    let (rect, _) = app.hits.menu_items[pos];
    click(app, rect);
}

#[test]
fn right_click_opens_menu_for_that_workspace_without_focusing_it() {
    let (mut app, ids) = three_workspaces();
    let (row, _) = app.hits.workspaces[1];
    right_click(&mut app, row);
    assert_eq!(menu(&app).target, Target::Workspace(ids[1]));
    assert_eq!((menu(&app).x, menu(&app).y), (row.x, row.y));
    assert_eq!(app.active, 0);
}

#[test]
fn right_click_elsewhere_does_nothing() {
    let (mut app, _ids) = three_workspaces();
    let pane = app.hits.pane;
    right_click(&mut app, pane);
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn rename_from_menu_sets_a_custom_name() {
    let (mut app, _ids) = three_workspaces();
    menu_pick(&mut app, 1, "Rename");
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
    assert_eq!(app.workspaces[1].name.as_deref(), Some("web server"));
    assert_eq!(app.workspaces[1].label(), "web server");
    assert_eq!(app.active, 0);
}

#[test]
fn renaming_to_empty_or_the_default_clears_the_name() {
    let (mut app, _ids) = three_workspaces();
    app.workspaces[0].name = Some("old".into());

    prefix(&mut app, 'W');
    assert_eq!(rename_input(&app), "old");
    for _ in 0..3 {
        key(&mut app, KeyCode::Backspace);
    }
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.workspaces[0].name, None);

    prefix(&mut app, 'W');
    key(&mut app, KeyCode::Enter); // Unchanged default label.
    assert_eq!(app.workspaces[0].name, None);
}

#[test]
fn escape_cancels_a_rename() {
    let (mut app, _ids) = three_workspaces();
    prefix(&mut app, 'W');
    type_str(&mut app, "!!!");
    key(&mut app, KeyCode::Esc);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.workspaces[0].name, None);
}

#[test]
fn close_from_menu_removes_that_workspace_and_keeps_focus() {
    let (mut app, ids) = three_workspaces();
    let (_sess, mut out) = app.add_test_session(ids[0], "10.0.0.5:1000");
    prefix(&mut app, 'j');
    prefix(&mut app, 'j');
    render_rows(&mut app, 120, 32);
    assert_eq!(app.workspaces[app.active].id, ids[2]);

    menu_pick(&mut app, 0, "Close");
    assert!(matches!(app.mode, Mode::ConfirmClose(t) if t == Target::Workspace(ids[0])));
    render_rows(&mut app, 120, 32);
    let confirm = app.hits.confirm;
    click(&mut app, confirm);
    assert!(matches!(app.mode, Mode::Normal));
    let remaining: Vec<WsId> = app.workspaces.iter().map(|w| w.id).collect();
    assert_eq!(remaining, [ids[1], ids[2]]);
    assert_eq!(app.workspaces[app.active].id, ids[2], "focus stays put");
    assert_eq!(out.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn closing_the_focused_workspace_from_the_menu() {
    let (mut app, ids) = three_workspaces();
    prefix(&mut app, 'j');
    render_rows(&mut app, 120, 32);
    menu_pick(&mut app, 1, "Close");
    assert_eq!(app.workspaces.len(), 2);
    assert_eq!(app.workspaces[app.active].id, ids[2]);
}

#[test]
fn menu_works_from_the_keyboard() {
    let (mut app, ids) = three_workspaces();
    let (row, _) = app.hits.workspaces[2];
    right_click(&mut app, row);
    key(&mut app, KeyCode::Down);
    assert_eq!(menu(&app).highlighted, 1);
    key(&mut app, KeyCode::Down);
    assert_eq!(menu(&app).highlighted, 0, "wraps");
    key(&mut app, KeyCode::Up);
    key(&mut app, KeyCode::Enter); // Close
    assert!(app.workspaces.iter().all(|w| w.id != ids[2]));

    let (row, _) = app.hits.workspaces[0];
    right_click(&mut app, row);
    key(&mut app, KeyCode::Esc);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.workspaces.len(), 2);
}

#[test]
fn hovering_highlights_menu_items() {
    let (mut app, _ids) = three_workspaces();
    let (row, _) = app.hits.workspaces[0];
    right_click(&mut app, row);
    render_rows(&mut app, 120, 32);
    let (close, _) = app.hits.menu_items[1];
    mouse(&mut app, MouseEventKind::Moved, close);
    assert_eq!(menu(&app).highlighted, 1);
}

#[test]
fn clicking_outside_the_menu_only_dismisses_it() {
    let (mut app, _ids) = three_workspaces();
    let (row, _) = app.hits.workspaces[0];
    right_click(&mut app, row);
    render_rows(&mut app, 120, 32);
    // The click lands on another workspace row but must not focus it.
    let (other, _) = app.hits.workspaces[2];
    click(&mut app, other);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.active, 0);
    assert_eq!(app.workspaces.len(), 3);
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
    assert_eq!(form(&app).field, 0);

    let tcp = form_target(&app, |h| matches!(h, FormHit::Protocol(Protocol::Tcp)));
    click(&mut app, tcp);
    assert_eq!(form(&app).protocol, Protocol::Tcp);
}

#[test]
fn clicking_a_field_lets_you_type_into_it() {
    let (mut app, _rx) = App::test_new();
    prefix(&mut app, 'N');
    render_rows(&mut app, 120, 32);

    let address = form_target(&app, |h| matches!(h, FormHit::Field(1)));
    // Anywhere on the row works, including well right of the text.
    click(
        &mut app,
        Rect {
            x: address.right() - 2,
            ..address
        },
    );
    assert_eq!(form(&app).field, 1);
    for _ in 0..7 {
        key(&mut app, KeyCode::Backspace);
    }
    type_str(&mut app, "127.0.0.1");
    assert_eq!(form(&app).address, "127.0.0.1");

    let port = form_target(&app, |h| matches!(h, FormHit::Field(2)));
    click(&mut app, port);
    assert_eq!(form(&app).field, 2);
    key(&mut app, KeyCode::Backspace);
    type_str(&mut app, "9");
    assert_eq!(form(&app).port, "4449");
    assert_eq!(form(&app).address, "127.0.0.1", "address untouched");
}

#[test]
fn clicks_outside_the_form_are_ignored() {
    let (mut app, _rx) = App::test_new();
    app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
    render_rows(&mut app, 120, 32);
    prefix(&mut app, 's');
    render_rows(&mut app, 120, 32);
    let new = app.hits.new_workspace;
    click(&mut app, new);
    assert_eq!(form(&app).target, Some(app.workspaces[0].id));
}

// ---- close confirmation ----

#[test]
fn closing_a_workspace_without_open_connections_skips_confirmation() {
    let (mut app, ids) = three_workspaces();
    let (sess, _out) = app.add_test_session(ids[0], "10.0.0.5:1000");
    app.handle(Event::Net(NetEvent::Closed { sess, error: None }));
    prefix(&mut app, 'D');
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.workspaces.len(), 2);
}

#[test]
fn cancelling_the_confirmation_keeps_everything() {
    let (mut app, ids) = three_workspaces();
    let (_sess, mut out) = app.add_test_session(ids[0], "10.0.0.5:1000");

    // Escape, other keys, and clicks off the buttons don't close anything.
    prefix(&mut app, 'D');
    type_str(&mut app, "yq");
    assert!(
        matches!(app.mode, Mode::ConfirmClose(_)),
        "other keys are ignored"
    );
    render_rows(&mut app, 120, 32);
    let pane = app.hits.pane;
    click(&mut app, pane);
    assert!(
        matches!(app.mode, Mode::ConfirmClose(_)),
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

    assert_eq!(app.workspaces.len(), 3);
    assert_eq!(out.try_recv(), Err(TryRecvError::Empty));
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
    assert_eq!(form(&app).field, 1);
    key(&mut app, KeyCode::Backspace);
    type_str(&mut app, "10");
    assert_eq!(form(&app).address, "10.0.0.0");
    assert_eq!(form(&app).port, "4444", "port untouched");
}

#[test]
fn rename_supports_cursor_movement_and_clicks() {
    let (mut app, _ids) = three_workspaces();
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
    assert_eq!(app.workspaces[0].name.as_deref(), Some("My tcp :4444"));
}

// ---- tab right-click menu ----

/// One workspace with three sessions; returns their ids and peer receivers.
fn three_tabs() -> (
    App,
    Vec<SessId>,
    Vec<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
) {
    let (mut app, _rx) = App::test_new();
    let ws = app.add_test_workspace(Protocol::Tcp, "0.0.0.0:4444");
    let (ids, outs) = (1..=3)
        .map(|i| app.add_test_session(ws, &format!("10.0.0.{i}:1000")))
        .unzip();
    render_rows(&mut app, 120, 32);
    (app, ids, outs)
}

/// Right-clicks tab `index`, renders the menu, and clicks `item`.
fn tab_menu_pick(app: &mut App, index: usize, item: &str) {
    let (tab, _) = app.hits.tabs[index];
    right_click(app, tab);
    render_rows(app, 120, 32);
    let pos = MENU_ITEMS.iter().position(|i| *i == item).unwrap();
    let (rect, _) = app.hits.menu_items[pos];
    click(app, rect);
}

#[test]
fn right_click_opens_menu_for_that_tab_without_focusing_it() {
    let (mut app, ids, _outs) = three_tabs();
    let (tab, _) = app.hits.tabs[2];
    right_click(&mut app, tab);
    assert_eq!(menu(&app).target, Target::Session(ids[2]));
    assert_eq!(app.workspaces[0].active_tab, 0);
}

#[test]
fn close_tab_from_menu_disconnects_that_session_and_keeps_focus() {
    let (mut app, ids, mut outs) = three_tabs();
    prefix(&mut app, '3');
    render_rows(&mut app, 120, 32);

    tab_menu_pick(&mut app, 0, "Close");
    assert!(matches!(app.mode, Mode::ConfirmClose(t) if t == Target::Session(ids[0])));
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
    let remaining: Vec<SessId> = app.workspaces[0].sessions.iter().map(|s| s.id).collect();
    assert_eq!(remaining, [ids[1], ids[2]]);
    let ws = &app.workspaces[0];
    assert_eq!(ws.sessions[ws.active_tab].id, ids[2], "focus stays put");
    assert_eq!(outs[2].try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn closing_the_last_tab_from_the_menu_moves_focus_left() {
    let (mut app, ids, _outs) = three_tabs();
    prefix(&mut app, '3');
    render_rows(&mut app, 120, 32);
    tab_menu_pick(&mut app, 2, "Close");
    key(&mut app, KeyCode::Enter);
    let ws = &app.workspaces[0];
    assert_eq!(ws.sessions.len(), 2);
    assert_eq!(ws.sessions[ws.active_tab].id, ids[1]);
}

#[test]
fn rename_tab_from_menu_sets_a_custom_name() {
    let (mut app, _ids, _outs) = three_tabs();
    tab_menu_pick(&mut app, 1, "Rename");
    assert_eq!(
        rename_input(&app),
        "10.0.0.2:1000",
        "prefilled with the peer"
    );

    key(&mut app, KeyCode::Home);
    type_str(&mut app, "db ");
    key(&mut app, KeyCode::Enter);
    assert_eq!(
        app.workspaces[0].sessions[1].name.as_deref(),
        Some("db 10.0.0.2:1000")
    );
    assert_eq!(app.workspaces[0].active_tab, 0);
}

#[test]
fn renaming_a_tab_to_empty_or_its_peer_clears_the_name() {
    let (mut app, _ids, _outs) = three_tabs();
    app.workspaces[0].sessions[0].name = Some("db".into());

    prefix(&mut app, 'T');
    assert_eq!(rename_input(&app), "db");
    key(&mut app, KeyCode::Backspace);
    key(&mut app, KeyCode::Backspace);
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.workspaces[0].sessions[0].name, None);

    prefix(&mut app, 'T');
    key(&mut app, KeyCode::Enter); // Unchanged peer address.
    assert_eq!(app.workspaces[0].sessions[0].name, None);
}

#[test]
fn sessions_list_has_no_right_click_menu() {
    let (mut app, _ids, _outs) = three_tabs();
    let (row, ..) = app.hits.sessions[1];
    right_click(&mut app, row);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.workspaces[0].active_tab, 0, "right-click doesn't focus");

    click(&mut app, row);
    assert_eq!(app.workspaces[0].active_tab, 1, "left-click does");
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
    assert_eq!(app.workspaces[0].sessions.len(), 2);

    app.handle(Event::Net(NetEvent::Closed {
        sess: ids[2],
        error: None,
    }));
    render_rows(&mut app, 120, 32);
    tab_menu_pick(&mut app, 1, "Close");
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.workspaces[0].sessions.len(), 1);
}

#[test]
fn cancelling_a_tab_close_keeps_the_connection() {
    let (mut app, ids, mut outs) = three_tabs();
    prefix(&mut app, 'X');
    key(&mut app, KeyCode::Esc);
    tab_menu_pick(&mut app, 1, "Close");
    render_rows(&mut app, 120, 32);
    let cancel = app.hits.cancel;
    click(&mut app, cancel);

    assert!(matches!(app.mode, Mode::Normal));
    let remaining: Vec<SessId> = app.workspaces[0].sessions.iter().map(|s| s.id).collect();
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
    assert_eq!(app.workspaces[0].sessions.len(), 2);
}
