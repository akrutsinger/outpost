//! Runs the real binary in a pseudo-terminal and reads its screen through a
//! terminal emulator, so tests see exactly what a user would.

use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub const COLS: u16 = 120;
pub const ROWS: u16 = 32;
const SIZE: (NonZeroU16, NonZeroU16) = match (NonZeroU16::new(ROWS), NonZeroU16::new(COLS)) {
    (Some(rows), Some(cols)) => (rows, cols),
    _ => panic!("the terminal has a size"),
};
/// How long any wait may take before the test fails.
pub const WAIT: Duration = Duration::from_secs(5);

pub const CTRL_B: &str = "\x02";
pub const CTRL_D: &str = "\x04";
pub const ENTER: &str = "\r";
pub const TAB: &str = "\t";
pub const ESC: &str = "\x1b";
pub const BACKSPACE: &str = "\x7f";
pub const LEFT: &str = "\x1b[D";
pub const RIGHT: &str = "\x1b[C";

pub struct Outpost {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    // Closing the master hangs up the terminal, so keep it for the test.
    _master: Box<dyn MasterPty + Send>,
}

/// The screen at one moment, as one string per row (one char per cell).
pub struct Screen(pub Vec<String>);

impl Screen {
    /// The (column, row) where `needle` first appears.
    pub fn find(&self, needle: &str) -> Option<(u16, u16)> {
        self.0.iter().enumerate().find_map(|(row, text)| {
            let byte = text.find(needle)?;
            Some((text[..byte].chars().count() as u16, row as u16))
        })
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.find(needle).is_some()
    }
}

impl std::fmt::Display for Screen {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        for row in &self.0 {
            writeln!(f, "│{row}│")?;
        }
        Ok(())
    }
}

impl Outpost {
    pub fn start() -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_outpost"));
        cmd.env("TERM", "xterm-256color");
        // Shell tabs run a plain shell, not whatever the user has configured.
        cmd.env("SHELL", "/bin/sh");
        let child = pty.slave.spawn_command(cmd).unwrap();
        drop(pty.slave);

        let parser = Arc::new(Mutex::new(vt100::Parser::new(SIZE.0, SIZE.1, 0)));
        let mut reader = pty.master.try_clone_reader().unwrap();
        let feed = parser.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                feed.lock().unwrap().process(&buf[..n]);
            }
        });
        let writer = pty.master.take_writer().unwrap();
        Self {
            child,
            writer,
            parser,
            _master: pty.master,
        }
    }

    /// Writes raw bytes, as if typed.
    pub fn keys(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    /// `ctrl+b` then `key`.
    pub fn prefix(&mut self, key: &str) {
        self.keys(&format!("{CTRL_B}{key}"));
    }

    pub fn screen(&self) -> Screen {
        let parser = self.parser.lock().unwrap();
        let screen = parser.screen();
        Screen(
            (0..ROWS)
                .map(|row| {
                    (0..COLS)
                        .map(|col| match screen.cell(row, col) {
                            Some(cell) if cell.has_contents() => cell.contents().to_string(),
                            _ => " ".to_string(),
                        })
                        .collect()
                })
                .collect(),
        )
    }

    /// Waits until `check` holds for the screen, failing with `what` and the
    /// screen if it doesn't in time.
    pub async fn wait_until<T>(&self, what: &str, check: impl Fn(&Screen) -> Option<T>) -> T {
        let deadline = Instant::now() + WAIT;
        loop {
            let screen = self.screen();
            if let Some(found) = check(&screen) {
                return found;
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for {what}; screen:\n{screen}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits for `needle` on screen and returns its (column, row).
    pub async fn wait_for(&self, needle: &str) -> (u16, u16) {
        self.wait_until(&format!("{needle:?}"), |s| s.find(needle))
            .await
    }

    pub async fn wait_gone(&self, needle: &str) {
        self.wait_until(&format!("{needle:?} to go away"), |s| {
            (!s.contains(needle)).then_some(())
        })
        .await
    }

    /// A left click at a 0-based cell (SGR mouse reporting is 1-based).
    pub fn click(&mut self, (col, row): (u16, u16)) {
        self.keys(&format!(
            "\x1b[<0;{c};{r}M\x1b[<0;{c};{r}m",
            c = col + 1,
            r = row + 1
        ));
    }

    pub fn right_click(&mut self, (col, row): (u16, u16)) {
        self.keys(&format!(
            "\x1b[<2;{c};{r}M\x1b[<2;{c};{r}m",
            c = col + 1,
            r = row + 1
        ));
    }

    /// Presses the left button at `from`, moves to `to`, and releases.
    pub fn drag(&mut self, from: (u16, u16), to: (u16, u16)) {
        self.keys(&format!(
            "\x1b[<0;{};{}M\x1b[<32;{};{}M\x1b[<0;{};{}m",
            from.0 + 1,
            from.1 + 1,
            to.0 + 1,
            to.1 + 1,
            to.0 + 1,
            to.1 + 1
        ));
    }

    /// Waits for `needle` and clicks its first character.
    pub async fn click_text(&mut self, needle: &str) {
        let at = self.wait_for(needle).await;
        self.click(at);
    }

    /// Creates a listener on 127.0.0.1 with an OS-picked port, and returns
    /// that port once it is listening. `choose` picks the protocol from tcp
    /// with arrow keys; `alpn` fills the TLS/QUIC row.
    pub async fn new_listener(&mut self, label: &str, choose: &str, alpn: Option<&str>) -> u16 {
        self.prefix("N");
        self.fill_form(label, choose, alpn).await
    }

    /// [`Outpost::new_listener`] once the form is opened some other way.
    pub async fn fill_form(&mut self, label: &str, choose: &str, alpn: Option<&str>) -> u16 {
        self.wait_for("protocol").await;
        // The form opens on the port.
        self.keys(&BACKSPACE.repeat(5));
        self.keys("0");
        self.keys(TAB); // -> protocol
        self.keys(choose);
        self.keys(TAB); // -> address
        self.keys(&BACKSPACE.repeat(7));
        self.keys("127.0.0.1");
        if let Some(alpn) = alpn {
            self.keys(&TAB.repeat(3)); // -> port, cert, alpn
            self.keys(alpn);
        }
        self.keys(ENTER);
        self.wait_gone("protocol").await;
        self.listening_port(label).await
    }

    /// The port shown for the listener `label`, once it is bound.
    pub async fn listening_port(&self, label: &str) -> u16 {
        let prefix = format!("{label} :");
        self.wait_until(&format!("{label} to listen"), |s| {
            s.0.iter().find_map(|row| {
                let rest = &row[row.find(&prefix)? + prefix.len()..];
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().ok().filter(|&port| port != 0)
            })
        })
        .await
    }

    /// Quits with `prefix q`, which must not need confirming (no live
    /// connections), and checks the process exits on its own.
    pub async fn quit(mut self) {
        self.prefix("q");
        self.wait_exit().await;
    }

    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub async fn wait_exit(&mut self) {
        let deadline = Instant::now() + WAIT;
        while self.is_running() {
            assert!(
                Instant::now() < deadline,
                "outpost did not quit; screen:\n{}",
                self.screen()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// Waits until `check` holds, for conditions outside the screen.
pub async fn eventually(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

impl Drop for Outpost {
    fn drop(&mut self) {
        // A failed test leaves it running; it is ours, so stop it.
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
