//! Rendering tests: draw to an off-screen buffer and check the text.

use super::tabs::wrap;
use super::*;
use crate::Event;
use crate::app::{Confirm, ContextMenu, MenuItem, RenameForm, SettingsForm, Target};
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
    assert!(text.contains(" endpoints"));
    assert!(text.contains("no endpoints"));
    assert!(text.contains("no connections"));
    assert!(text.contains("ctrl+b c for a shell · ctrl+b N for a listener"));
}

#[test]
fn listener_without_sessions_says_waiting() {
    let (mut app, _rx) = App::test_new();
    app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    let rows = render_rows(&mut app, W, H);
    assert!(row_with(&rows, "1 tcp :4444").contains('○'));
    assert!(row_with(&rows, "0.0.0.0 · 0 open").starts_with("   0.0.0.0"));
    row_with(&rows, "listening on tcp 0.0.0.0:4444");
    row_with(&rows, "waiting for connections…");
}

#[test]
fn session_shows_in_sidebar_tab_bar_and_pane() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    let (sess, _out) = app.add_test_session(listener, "10.0.0.5:51234");
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
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    app.add_test_session(listener, "255.255.255.255:65535");
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
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    app.handle(Event::Net(NetEvent::ListenFailed {
        listener,
        error: "Address already in use (os error 98)".into(),
    }));
    let rows = render_rows(&mut app, W, H);
    row_with(&rows, "   Address already in us");
    row_with(&rows, "could not listen on tcp 0.0.0.0:4444");
    row_with(&rows, "ctrl+b e to change settings");
}

#[test]
fn closed_session_replaces_input_with_hint() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    let (sess, _out) = app.add_test_session(listener, "10.0.0.5:51234");
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
    assert!(last.contains("c shell"));
    assert!(last.contains("N listener"));
    assert!(last.contains("D close endpoint"));
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
    row_with(&rows, "╭ new listener ");
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
    let listener = app.add_test_listener(Protocol::Tls, "0.0.0.0:8443");
    let server = tls::build(&TlsConfig::default()).unwrap();
    let fingerprint = server.fingerprint.clone();
    app.test_listener_mut(0).tls = Some(server);
    app.test_listener_mut(0).config.tls.cert = CertSource::Files {
        cert: "/etc/outpost/server.pem".into(),
        key: "/etc/outpost/server.key".into(),
    };
    app.mode = Mode::Settings(SettingsForm {
        target: Some(listener),
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
    row_with(&rows, "╭ listener settings ");
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
    let listener = app.add_test_listener(Protocol::Quic, "0.0.0.0:4433");
    let (sess, _out) = app.add_test_quic_session(listener, "255.255.255.255:65535");
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
    // The session row keeps its byte count; the listener label gives way.
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
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    let (sess, _out) = app.add_test_session(listener, "10.0.0.5:51234");
    let bytes: String = (0..100).map(|i| format!("line {i:03}\n")).collect();
    app.handle(Event::Net(NetEvent::Data {
        sess,
        bytes: bytes.into_bytes(),
    }));

    let text = render_rows(&mut app, W, H).join("\n");
    assert!(text.contains("line 099"));
    assert!(!text.contains("line 000"));

    app.test_session_mut(0, 0).scroll = 1_000;
    let text = render_rows(&mut app, W, H).join("\n");
    assert!(text.contains("line 000"), "scrolled to the top");
    assert!(!text.contains("line 099"));
    assert!(text.contains("more "), "scroll indicator shown");
    // Overscroll is clamped to the top of the transcript.
    assert!(app.test_session(0, 0).scroll < 1_000);
}

#[test]
fn renamed_listener_shows_name_and_endpoint() {
    let (mut app, _rx) = App::test_new();
    app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    app.endpoints[0].name = Some("web".into());
    let rows = render_rows(&mut app, W, H);
    row_with(&rows, " 1 web ○");
    row_with(&rows, "   tcp :4444 · 0 open");
}

#[test]
fn context_menu_opens_at_the_click_and_stays_on_screen() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    app.mode = Mode::Menu(ContextMenu {
        target: Some(Target::Endpoint(listener)),
        items: vec![MenuItem::Rename, MenuItem::Close],
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
        target: Some(Target::Endpoint(listener)),
        items: vec![MenuItem::Rename, MenuItem::Close],
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
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    app.mode = Mode::Rename(RenameForm {
        target: Target::Endpoint(listener),
        input: "web".into(),
    });
    let rows = render_rows(&mut app, W, H);
    row_with(&rows, "╭ rename endpoint ");
    row_with(&rows, "│  web ");
    row_with(&rows, "enter save · esc cancel · empty resets");
}

#[test]
fn confirm_close_names_the_listener_and_its_connections() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    app.endpoints[0].name = Some("web".into());
    let _a = app.add_test_session(listener, "10.0.0.5:1000");
    let _b = app.add_test_session(listener, "10.0.0.6:1000");
    app.mode = Mode::Confirm(Confirm::Close(Target::Endpoint(listener)));
    let rows = render_rows(&mut app, W, H);
    row_with(&rows, "│ Close listener?");
    row_with(&rows, " web (tcp :4444) has 2 open connections.");
    let buttons = row_with(&rows, "↵ confirm");
    assert!(buttons.contains("esc cancel"));
}

#[test]
fn long_input_scrolls_to_keep_the_cursor_visible() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    app.add_test_session(listener, "10.0.0.5:51234");
    let text: String = ('a'..='z').cycle().take(200).collect();
    app.test_session_mut(0, 0).input = text.as_str().into();
    let rows = render_rows(&mut app, W, H);
    // The end is visible while typing at the end...
    assert!(row_with(&rows, "❯ ").contains(&text[190..]));

    // ...and the start once the cursor moves home.
    app.test_session_mut(0, 0).input.set_cursor(0);
    let rows = render_rows(&mut app, W, H);
    assert!(row_with(&rows, "❯ ").contains("❯ abcdefgh"));
}

#[test]
fn renamed_tab_shows_its_name_everywhere() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    app.add_test_session(listener, "10.0.0.5:51234");
    app.test_session_mut(0, 0).name = Some("db".into());
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
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    let (sess, _out) = app.add_test_session(listener, "10.0.0.5:51234");
    app.mode = Mode::Rename(RenameForm {
        target: Target::Tab(sess),
        input: "db".into(),
    });
    let rows = render_rows(&mut app, W, H);
    row_with(&rows, "╭ rename tab ");
}

#[test]
fn confirm_close_for_a_tab_names_the_peer() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    let (sess, _out) = app.add_test_session(listener, "255.255.255.255:65535");
    app.test_session_mut(0, 0).name = Some("db".into());
    app.mode = Mode::Confirm(Confirm::Close(Target::Tab(sess)));
    let rows = render_rows(&mut app, W, H);
    row_with(&rows, "│ Close tab?");
    row_with(&rows, " db (255.255.255.255:65535) is still connected.");
    row_with(&rows, "↵ confirm");
}

#[test]
fn confirm_quit_names_one_listener_or_counts_several() {
    let (mut app, _rx) = App::test_new();
    let web = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    app.endpoints[0].name = Some("web".into());
    let _a = app.add_test_session(web, "10.0.0.5:1000");
    let _b = app.add_test_session(web, "10.0.0.6:1000");
    // A listener with nothing open doesn't count.
    app.add_test_listener(Protocol::Tcp, "0.0.0.0:5555");
    app.mode = Mode::Confirm(Confirm::Quit);
    let rows = render_rows(&mut app, W, H);
    row_with(&rows, "│ Quit outpost?");
    row_with(&rows, " web (tcp :4444) has 2 open connections.");
    row_with(&rows, "↵ confirm");

    let dns = app.add_test_listener(Protocol::Udp, "0.0.0.0:5353");
    let _c = app.add_test_session(dns, "10.0.0.7:1000");
    let rows = render_rows(&mut app, W, H);
    row_with(&rows, " 3 open connections across 2 endpoints.");
}

#[test]
fn confirm_restart_says_what_applying_drops() {
    let (mut app, _rx) = App::test_new();
    let listener = app.add_test_listener(Protocol::Udp, "0.0.0.0:5555");
    app.endpoints[0].name = Some("web".into());
    let _a = app.add_test_session(listener, "10.0.0.5:1000");
    let _b = app.add_test_session(listener, "10.0.0.6:1000");
    app.mode = Mode::Settings(SettingsForm {
        target: Some(listener),
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
    let listener = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    app.add_test_session(listener, "10.0.0.5:51234");
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

fn shell_output(app: &mut App, tab: crate::net::SessId, bytes: &[u8]) {
    app.handle(Event::Pty(crate::pty::PtyEvent::Output {
        tab,
        bytes: bytes.to_vec(),
    }));
}

#[test]
fn shell_tab_draws_its_screen_under_the_local_endpoint() {
    let (mut app, _rx) = App::test_new();
    let (tab, _shell) = app.add_test_terminal();
    // Size the shell to the pane first, as the main loop does before drawing.
    render_rows(&mut app, W, H);
    shell_output(&mut app, tab, b"$ echo \x1b[31mred\x1b[0m\r\nred\r\n$ ");
    let rows = render_rows(&mut app, W, H);
    row_with(&rows, " endpoints");
    row_with(&rows, "local");
    row_with(&rows, "1 open");
    // Shells aren't connections.
    row_with(&rows, "no connections");
    row_with(&rows, "╭ sh ");
    row_with(&rows, " open ");
    row_with(&rows, "│$ echo red");
    // The tab bar has the shell's tab, then `+`.
    let bar = &rows[0];
    let sh = bar.find(" sh ").expect("shell tab");
    assert!(bar[sh..].contains(" + "), "{bar:?}");
}

#[test]
fn plus_in_the_tab_bar_is_clickable_with_no_endpoints() {
    let (mut app, _rx) = App::test_new();
    render_rows(&mut app, W, H);
    let plus = app.hits.new_tab;
    assert_eq!(plus.width, 3);
    let rows = render_rows(&mut app, W, H);
    let drawn: String = rows[0]
        .chars()
        .skip(plus.x as usize)
        .take(plus.width as usize)
        .collect();
    assert_eq!(drawn, " + ");
}

#[test]
fn exited_shell_says_how_and_how_to_close_it() {
    let (mut app, _rx) = App::test_new();
    let (tab, _shell) = app.add_test_terminal();
    app.handle(Event::Pty(crate::pty::PtyEvent::Exited {
        tab,
        status: "exited 130".into(),
    }));
    let rows = render_rows(&mut app, W, H);
    row_with(&rows, " exited 130 ");
    row_with(&rows, " ctrl+b X closes the tab ");
    row_with(&rows, "0 open");
}

#[test]
fn confirms_count_shells_apart_from_connections() {
    let (mut app, _rx) = App::test_new();
    let web = app.add_test_listener(Protocol::Tcp, "0.0.0.0:4444");
    let _a = app.add_test_session(web, "10.0.0.5:1000");
    let (tab, _shell) = app.add_test_terminal();

    app.mode = Mode::Confirm(Confirm::Quit);
    let rows = render_rows(&mut app, W, H);
    row_with(
        &rows,
        " 1 open connection and 1 running shell across 2 endpoints.",
    );

    let local = app.endpoints[0].id;
    app.mode = Mode::Confirm(Confirm::Close(Target::Endpoint(local)));
    let rows = render_rows(&mut app, W, H);
    row_with(&rows, "│ Close all shells?");
    row_with(&rows, " local has 1 running shell.");

    app.mode = Mode::Confirm(Confirm::Close(Target::Tab(tab)));
    let rows = render_rows(&mut app, W, H);
    row_with(&rows, " sh is still running.");
}

#[test]
fn settings_lists_the_integrations() {
    let (mut app, _rx) = App::test_new();
    app.mode = Mode::AppSettings;
    let rows = render_rows(&mut app, W, H);
    row_with(&rows, "│ settings");
    row_with(&rows, "│ integrations ");
    row_with(
        &rows,
        "a shell running one of these is filed under the host it connects to",
    );
    let ssh = row_with(&rows, " ✓ ssh");
    assert!(ssh.contains("active"), "{ssh:?}");
    let netcat = row_with(&rows, " – netcat");
    assert!(netcat.contains("planned"), "{netcat:?}");
    row_with(&rows, " esc close ");
    assert_ne!(app.hits.cancel.width, 0);
}

#[test]
fn the_footer_menu_opens_above_itself() {
    let (mut app, _rx) = App::test_new();
    let rows = render_rows(&mut app, W, H);
    let footer = app.hits.menu;
    let drawn: String = rows[footer.y as usize]
        .chars()
        .skip(footer.x as usize)
        .take(footer.width as usize)
        .collect();
    assert_eq!(drawn, "menu");

    app.handle(Event::Input(ratatui::crossterm::event::Event::Mouse(
        ratatui::crossterm::event::MouseEvent {
            kind: ratatui::crossterm::event::MouseEventKind::Down(
                ratatui::crossterm::event::MouseButton::Left,
            ),
            column: footer.x,
            row: footer.y,
            modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
        },
    )));
    let rows = render_rows(&mut app, W, H);
    // Three items in a bordered box, its bottom just above the footer and
    // its right edge at the footer's.
    let bottom = &rows[footer.y as usize - 1];
    assert_eq!(
        bottom.chars().nth(footer.right() as usize - 1),
        Some('┘'),
        "{bottom:?}"
    );
    row_with(&rows[..footer.y as usize], "│ settings");
    row_with(&rows[..footer.y as usize], "│ keybinds");
    row_with(&rows[..footer.y as usize], "│ quit");
}

/// Files the shell in `tab` under a host, as detection would: the shell
/// runs `ssh <alias>`, which resolves to `addr:port`.
fn ssh_to(app: &mut App, tab: crate::net::SessId, alias: &str, addr: &str, port: u16) {
    let argv = vec!["ssh".to_string(), alias.to_string()];
    app.test_observe(tab, argv.clone());
    app.handle(Event::Resolved(crate::detect::Resolved {
        tab,
        argv,
        dest: Some(crate::detect::Destination {
            alias: alias.into(),
            user: "root".into(),
            host: addr.into(),
            port,
        }),
    }));
}

#[test]
fn a_shell_on_a_host_shows_under_it_and_as_a_session() {
    let (mut app, _rx) = App::test_new();
    let (tab, _shell) = app.add_test_terminal();
    ssh_to(&mut app, tab, "proxmox", "192.168.12.69", 22);
    let rows = render_rows(&mut app, W, H);
    row_with(&rows, "proxmox");
    // A whole address leaves no room for the count.
    let detail = row_with(&rows, "   192.168.12.69");
    assert!(
        !detail.contains('…') && !detail.contains("1 open"),
        "{detail:?}"
    );
    // The local endpoint stays, now with nothing open.
    row_with(&rows, "0 open");
    // A shell on another machine is a connection.
    row_with(&rows, "1 open");
    row_with(&rows, "root");
    assert!(
        rows[0].contains(" root "),
        "tab named for the user: {:?}",
        rows[0]
    );
    assert!(rows[0].contains("ssh 192.168.12.69"), "{:?}", rows[0]);
    row_with(&rows, "╭ root ");
}

#[test]
fn a_host_row_keeps_the_whole_address_over_the_open_count() {
    let (mut app, _rx) = App::test_new();
    let (short, _a) = app.add_test_terminal();
    ssh_to(&mut app, short, "nas", "10.0.0.2", 22);
    let rows = render_rows(&mut app, W, H);
    // A short address has room for the count.
    row_with(&rows, "   10.0.0.2 · 1 open");

    // The longest IPv4 address and port fit exactly, without the count.
    let (long, _b) = app.add_test_terminal();
    ssh_to(
        &mut app,
        long,
        "edge-router-with-a-long-name",
        "255.255.255.255",
        65535,
    );
    let rows = render_rows(&mut app, W, H);
    row_with(&rows, " 3 edge-router-with-a-lo");
    let detail = row_with(&rows, "   255.255.255.255:65535");
    assert!(
        !detail.contains('…') && !detail.contains("open"),
        "{detail:?}"
    );
    assert!(
        rows[0].contains("ssh 255.255.255.255:65535"),
        "{:?}",
        rows[0]
    );
}

#[test]
fn closing_a_host_says_its_shells_close_too() {
    let (mut app, _rx) = App::test_new();
    let (tab, _shell) = app.add_test_terminal();
    ssh_to(&mut app, tab, "nas", "10.0.0.2", 22);
    let nas = app.endpoints[1].id;
    app.mode = Mode::Confirm(Confirm::Close(Target::Endpoint(nas)));
    let rows = render_rows(&mut app, W, H);
    row_with(&rows, "│ Close host and its shells?");
    row_with(&rows, " nas has 1 ssh session.");
}
