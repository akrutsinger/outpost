//! End-to-end tests: the real binary in a pseudo-terminal, checked from the
//! screen and from the network side. Add a case here when a change depends
//! on how a real terminal delivers input; test behavior in-process otherwise.

mod clients;
mod harness;

use std::net::UdpSocket;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use harness::*;

/// Reads exactly `n` bytes as text.
async fn read_text(from: &mut (impl AsyncRead + Unpin), n: usize) -> String {
    let mut buf = vec![0; n];
    timeout(WAIT, from.read_exact(&mut buf))
        .await
        .expect("data arrived in time")
        .unwrap();
    String::from_utf8(buf).unwrap()
}

/// Reads until the peer closes, which must happen without an error.
async fn read_to_eof(from: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
    let mut rest = Vec::new();
    timeout(WAIT, from.read_to_end(&mut rest))
        .await
        .expect("closed in time")
        .unwrap();
    rest
}

fn udp_port_bound(port: u16) -> bool {
    UdpSocket::bind(("127.0.0.1", port)).is_err()
}

#[tokio::test]
async fn tcp_round_trip_from_the_keyboard() {
    let mut app = Outpost::start();
    let port = app.new_listener("tcp", "", None).await;

    let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    client.write_all(b"hello from client\n").await.unwrap();
    app.wait_for("hello from client").await;
    app.keys("reply from outpost\r");
    assert_eq!(read_text(&mut client, 19).await, "reply from outpost\n");

    // A live tab asks before closing; confirming hangs up.
    app.prefix("X");
    app.wait_for("Close tab?").await;
    app.keys(ENTER);
    app.wait_for("no connections").await;
    assert_eq!(read_to_eof(&mut client).await, b"");
    app.quit().await;
}

#[tokio::test]
async fn mouse_picks_tabs_closes_from_the_menu_drags_the_sidebar_and_quits() {
    let mut app = Outpost::start();
    app.click_text("new  ").await;
    let port = app.fill_form("tcp", "", None).await;

    let mut one = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut two = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let tab_one = format!("127.0.0.1:{}", one.local_addr().unwrap().port());
    let tab_two = format!("127.0.0.1:{}", two.local_addr().unwrap().port());
    app.wait_for(&tab_two).await;

    // The tab bar is the top row, so the first match is the tab.
    app.click_text(&tab_one).await;
    app.wait_for(&format!("╭ {tab_one}")).await;
    app.keys("to one\r");
    assert_eq!(read_text(&mut one, 7).await, "to one\n");

    // Right-click the other tab, close it from the menu, confirm by clicking.
    let at = app.wait_for(&tab_two).await;
    app.right_click(at);
    app.click_text("Close").await;
    app.click_text("↵ confirm").await;
    app.wait_gone(&tab_two).await;
    assert_eq!(read_to_eof(&mut two).await, b"");

    // Dragging the divider moves the sidebar edge.
    let edge = |screen: &Screen, col: usize| screen.0[5].chars().nth(col) == Some('│');
    assert!(
        edge(&app.screen(), 25),
        "sidebar edge at 25:\n{}",
        app.screen()
    );
    app.drag((25, 5), (35, 5));
    app.wait_until("the sidebar edge at 35", |s| {
        (edge(s, 35) && !edge(s, 25)).then_some(())
    })
    .await;

    // Quitting with a live connection asks; Escape keeps it running.
    app.prefix("q");
    app.wait_for("Quit outpost?").await;
    app.wait_for(&format!("tcp :{port} has 1 open connection."))
        .await;
    app.keys(ESC);
    app.wait_gone("Quit outpost?").await;
    assert!(app.is_running());
    app.prefix("q");
    app.click_text("↵ confirm").await;
    app.wait_exit().await;
    assert_eq!(read_to_eof(&mut one).await, b"");
}

#[tokio::test]
async fn tls_shows_the_handshake_and_the_fingerprint_the_client_sees() {
    let mut app = Outpost::start();
    let port = app
        .new_listener("tls", &RIGHT.repeat(2), Some("http/1.1"))
        .await;

    let (mut client, fingerprint) = clients::tls_connect(port, b"http/1.1").await;
    client.write_all(b"hello over tls\n").await.unwrap();
    app.wait_for("tls1.3 · TLS13_").await;
    app.wait_for("alpn http/1.1 · sni localhost").await;
    app.wait_for("hello over tls").await;
    app.keys("secure reply\r");
    assert_eq!(read_text(&mut client, 13).await, "secure reply\n");

    app.prefix("s");
    for group in 0..8 {
        app.wait_for(&fingerprint[group * 8..group * 8 + 8]).await;
    }
    app.keys(ESC);
    app.wait_gone("sha256").await;

    // A plaintext client fails the handshake, and its tab says so.
    let mut plain = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    plain.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    // Select its tab once it exists; keys sent earlier would find no tab 2.
    app.wait_for(&format!("127.0.0.1:{}", plain.local_addr().unwrap().port()))
        .await;
    app.prefix("2");
    app.wait_for("tls handshake failed").await;

    // Once the client hangs up, quitting doesn't need confirming.
    drop(client);
    app.wait_for("· 0 open").await;
    app.quit().await;
}

#[tokio::test]
async fn quic_replies_finishes_and_opens_streams() {
    let mut app = Outpost::start();
    let port = app.new_listener("quic", LEFT, Some("hq-interop")).await;

    let (_endpoint, conn) = clients::quic_connect(port, b"hq-interop").await;
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(b"GET /hello\r\n").await.unwrap();
    app.wait_for("quic · alpn hq-interop · sni localhost").await;
    app.wait_for("GET /hello").await;

    // The reply goes to the stream that sent last; ctrl+d finishes it.
    app.keys("hello back\r");
    app.keys(CTRL_D);
    app.wait_for("finished sending").await;
    assert_eq!(read_to_eof(&mut recv).await, b"hello back\n");

    // prefix tab moves past the finished stream to "new".
    app.prefix(TAB);
    app.wait_for("❯  new ").await;
    app.keys("pushed\r");
    let (_send, mut pushed) = timeout(WAIT, conn.accept_bi()).await.unwrap().unwrap();
    assert_eq!(read_text(&mut pushed, 7).await, "pushed\n");
    app.wait_for("[s0] GET /hello").await;
    app.wait_for("[s1] pushed").await;

    app.prefix("X");
    app.wait_for("Close tab?").await;
    app.keys(ENTER);
    let closed = timeout(WAIT, conn.closed()).await.unwrap();
    assert!(
        matches!(&closed, quinn::ConnectionError::ApplicationClosed(c) if c.error_code.into_inner() == 0),
        "{closed:?}"
    );
    app.quit().await;
}

#[tokio::test]
async fn udp_restart_and_close_ask_before_dropping_peers() {
    let mut app = Outpost::start();
    let old = app.new_listener("udp", RIGHT, None).await;
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.send_to(b"hello udp\n", ("127.0.0.1", old)).unwrap();
    app.wait_for("hello udp").await;

    // A port the OS just handed out and released is very likely still free.
    let new = UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    app.prefix("s");
    app.wait_for("protocol").await;
    app.keys(&DOWN.repeat(2)); // -> address, port
    app.keys(&BACKSPACE.repeat(5));
    app.keys(&format!("{new}{ENTER}"));
    app.wait_for("Apply settings?").await;

    // Escape goes back to the form, and nothing has restarted.
    app.keys(ESC);
    app.wait_gone("Apply settings?").await;
    app.wait_for("protocol").await;
    assert!(udp_port_bound(old));

    app.keys(ENTER);
    app.click_text("↵ confirm").await;
    app.wait_for("listener stopped").await;
    eventually("the new port to be bound", || udp_port_bound(new)).await;
    assert!(!udp_port_bound(old));

    // Closing a listener with a live peer asks too.
    peer.send_to(b"again\n", ("127.0.0.1", new)).unwrap();
    app.wait_for("· 1 open").await;
    app.prefix("D");
    app.wait_for("Close listener?").await;
    app.keys(ENTER);
    app.wait_for("no endpoints").await;
    eventually("the port to be freed", || !udp_port_bound(new)).await;
    app.quit().await;
}

/// A shell tab runs a real shell sized to the pane, takes typed commands,
/// and stays as a closed tab once the shell exits.
#[tokio::test]
async fn shell_tab_runs_commands_at_the_pane_size() {
    let mut app = Outpost::start();
    app.wait_for("no endpoints").await;
    app.prefix("c");
    app.wait_for("╭ sh ").await;
    // The typed line shows the expression; only the shell's output has its value.
    app.keys("echo outpost-$((6*7)); stty size\r");
    app.wait_for("outpost-42").await;
    // 120×32, less the 26-column sidebar, the tab bar, and the pane border.
    app.wait_for("29 92").await;
    app.keys("exit\r");
    app.wait_for(" exited 0 ").await;
    app.wait_for("0 open").await;
    app.quit().await;
}

/// Running ssh in a shell tab files the tab under that host, named for the
/// user, until ssh ends. A proxy that never answers keeps ssh running
/// without a network, and `-F /dev/null` ignores the user's ssh config.
#[tokio::test]
async fn ssh_in_a_shell_moves_the_tab_under_its_host() {
    let mut app = Outpost::start();
    app.wait_for("no endpoints").await;
    app.prefix("c");
    app.wait_for("╭ sh ").await;
    app.keys("ssh -F /dev/null -o ProxyCommand='sleep 60' -p 2222 root@proxmox.test\r");
    app.wait_for("2 proxmox.test").await;
    app.wait_for("ssh proxmox.test:2222").await;
    app.wait_for("╭ root ").await;

    // Interrupting ssh brings the shell back to local, and the host goes.
    // (The typed command stays on the shell's screen; watch the sidebar row
    // and the tab bar instead.)
    app.keys(CTRL_C);
    app.wait_gone("2 proxmox.test").await;
    app.wait_gone("ssh proxmox.test:2222").await;
    app.wait_for("╭ sh ").await;
    app.keys("exit 0\r");
    app.wait_for(" exited 0 ").await;
    app.quit().await;
}

/// footer menu, clicked with a real mouse, opens settings, which
/// lists the integrations until esc.
#[tokio::test]
async fn footer_menu_opens_settings_with_the_integrations() {
    let mut app = Outpost::start();
    app.wait_for("no endpoints").await;
    app.click_text("menu").await;
    app.wait_for("keybinds").await;
    app.click_text("settings").await;
    app.wait_for("✓ ssh").await;
    app.wait_for("– netcat").await;
    app.keys(ESC);
    app.wait_gone("– netcat").await;
    app.quit().await;
}
