//! Terminal tabs: a shell in a PTY, the screen it draws, what its foreground
//! command connects to, and turning keys into the bytes a terminal sends.

use std::io;
use std::num::NonZeroU16;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::Status;
use crate::detect::{self, Destination, Integration};
use crate::net::SessId;
use crate::pty::Pty;

pub struct Terminal {
    pub id: SessId,
    /// User-chosen name; `None` shows the shell's name.
    pub name: Option<String>,
    /// What the shell is called, e.g. `zsh`.
    shell: String,
    pub parser: vt100::Parser,
    pub unread: bool,
    /// `None` once the shell has exited (or never started).
    pty: Option<Pty>,
    /// How the shell ended, once it has.
    pub exit: Option<String>,
    /// The foreground command when last checked.
    foreground: Option<Vec<String>>,
    /// Where that command connects, once an integration has worked it out.
    dest: Option<Destination>,
}

impl Terminal {
    /// A tab for `pty`, sized `rows`×`cols`. A shell that failed to start
    /// still gets a tab, which shows why.
    pub(super) fn new(
        id: SessId,
        shell: String,
        pty: io::Result<Pty>,
        rows: u16,
        cols: u16,
    ) -> Self {
        let (pty, exit) = match pty {
            Ok(pty) => (Some(pty), None),
            Err(e) => (None, Some(format!("could not start {shell}: {e}"))),
        };
        Self {
            id,
            name: None,
            shell,
            parser: vt100::Parser::new(nonzero(rows), nonzero(cols), 0),
            unread: false,
            pty,
            exit,
            foreground: None,
            dest: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.pty.is_some()
    }

    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.default_label())
    }

    /// The shell's name, or the user it's logged in as on another machine
    /// (the host endpoint names the machine).
    pub(super) fn default_label(&self) -> String {
        match &self.dest {
            Some(dest) => dest.user.clone(),
            None => self.shell.clone(),
        }
    }

    /// The machine this shell is connected to, if it's running a command an
    /// integration recognized and resolved.
    pub fn destination(&self) -> Option<&Destination> {
        self.dest.as_ref()
    }

    /// Checks the foreground command. Returns a new command for an
    /// integration to resolve.
    pub(super) fn poll(&mut self) -> Option<(Integration, Vec<String>)> {
        let foreground = self.pty.as_ref().and_then(Pty::foreground);
        self.observe(foreground)
    }

    /// Records `foreground` as the running command; a different one forgets
    /// where the last one was connected.
    pub(super) fn observe(
        &mut self,
        foreground: Option<Vec<String>>,
    ) -> Option<(Integration, Vec<String>)> {
        if foreground == self.foreground {
            return None;
        }
        self.foreground = foreground;
        self.dest = None;
        let argv = self.foreground.clone()?;
        Some((detect::recognize(&argv)?, argv))
    }

    /// Where `argv` connects, unless the shell has moved on to another command.
    pub(super) fn resolved(&mut self, argv: &[String], dest: Option<Destination>) {
        if self.foreground.as_deref() == Some(argv) {
            self.dest = dest;
        }
    }

    pub fn status(&self) -> Status {
        if !self.is_open() {
            Status::Closed
        } else if self.unread {
            Status::Unread
        } else {
            Status::Open
        }
    }

    pub(super) fn receive(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        self.unread = true;
    }

    pub(super) fn exited(&mut self, status: String) {
        if let Some(mut pty) = self.pty.take() {
            pty.exited();
        }
        self.exit = Some(status);
    }

    pub(super) fn send(&mut self, bytes: &[u8]) {
        // A failed write means the shell is going away; its exit event follows.
        if let Some(pty) = &mut self.pty {
            let _ = pty.write(bytes);
        }
    }

    pub(super) fn send_key(&mut self, key: KeyEvent) {
        if let Some(bytes) = encode_key(key, self.parser.screen().application_cursor()) {
            self.send(&bytes);
        }
    }

    /// Fits the screen, and the shell's idea of it, to `rows`×`cols`.
    pub(super) fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (nonzero(rows), nonzero(cols));
        if self.parser.screen().size() == (rows, cols) {
            return;
        }
        self.parser.set_size(rows, cols);
        if let Some(pty) = &self.pty {
            pty.resize(rows.get(), cols.get());
        }
    }
}

/// The emulator needs at least one row and column, even in a squeezed pane.
fn nonzero(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN)
}

/// The bytes an xterm sends for `key`, or `None` for keys it has no
/// encoding for. `app_cursor` is the program's cursor-key mode (DECCKM),
/// which full-screen programs like vim turn on.
pub(super) fn encode_key(key: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // xterm's modifier parameter: 1 + shift + 2·alt + 4·ctrl.
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    // Cursor keys: `ESC [ A`, `ESC O A` in application mode, `ESC [ 1;5 A` with ctrl.
    let cursor = |end: char| {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{end}")
        } else if app_cursor {
            format!("\x1bO{end}")
        } else {
            format!("\x1b[{end}")
        }
    };
    // Editing keys: `ESC [ 5 ~`, `ESC [ 5;5 ~` with ctrl.
    let tilde = |code: u8| {
        if modifier > 1 {
            format!("\x1b[{code};{modifier}~")
        } else {
            format!("\x1b[{code}~")
        }
    };
    let alt_prefixed = |bytes: Vec<u8>| {
        if alt {
            [b"\x1b".as_slice(), &bytes].concat()
        } else {
            bytes
        }
    };
    let text = match key.code {
        KeyCode::Char(c) if ctrl => return control(c).map(|byte| alt_prefixed(vec![byte])),
        KeyCode::Char(c) => return Some(alt_prefixed(c.to_string().into_bytes())),
        KeyCode::Enter => return Some(alt_prefixed(b"\r".to_vec())),
        KeyCode::Tab => "\t".into(),
        KeyCode::BackTab => "\x1b[Z".into(),
        KeyCode::Backspace => return Some(alt_prefixed(vec![0x7f])),
        KeyCode::Esc => "\x1b".into(),
        KeyCode::Up => cursor('A'),
        KeyCode::Down => cursor('B'),
        KeyCode::Right => cursor('C'),
        KeyCode::Left => cursor('D'),
        KeyCode::Home => cursor('H'),
        KeyCode::End => cursor('F'),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(n @ 1..=4) => {
            let end = char::from(b'P' + n - 1);
            if modifier > 1 {
                format!("\x1b[1;{modifier}{end}")
            } else {
                format!("\x1bO{end}")
            }
        }
        KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)]),
        _ => return None,
    };
    Some(text.into_bytes())
}

/// The control byte for ctrl+`c`, e.g. ctrl+c → 0x03.
fn control(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        ' ' | '@' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '7' | '/' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
        encode_key(KeyEvent::new(code, modifiers), false).expect("encodable")
    }

    #[test]
    fn text_and_control_keys() {
        assert_eq!(encode(KeyCode::Char('a'), KeyModifiers::NONE), b"a");
        assert_eq!(
            encode(KeyCode::Char('é'), KeyModifiers::NONE),
            "é".as_bytes()
        );
        assert_eq!(encode(KeyCode::Char('A'), KeyModifiers::SHIFT), b"A");
        assert_eq!(encode(KeyCode::Char('c'), KeyModifiers::CONTROL), [0x03]);
        assert_eq!(encode(KeyCode::Char(' '), KeyModifiers::CONTROL), [0x00]);
        assert_eq!(encode(KeyCode::Char('x'), KeyModifiers::ALT), b"\x1bx");
        assert_eq!(encode(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(encode(KeyCode::Backspace, KeyModifiers::NONE), [0x7f]);
        assert_eq!(encode(KeyCode::Esc, KeyModifiers::NONE), b"\x1b");
        assert_eq!(encode(KeyCode::BackTab, KeyModifiers::SHIFT), b"\x1b[Z");
    }

    #[test]
    fn cursor_keys_follow_the_cursor_mode() {
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(encode_key(up, false).unwrap(), b"\x1b[A");
        assert_eq!(encode_key(up, true).unwrap(), b"\x1bOA");
        // Modified keys are the same in both modes.
        let ctrl_right = KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL);
        assert_eq!(encode_key(ctrl_right, true).unwrap(), b"\x1b[1;5C");
    }

    #[test]
    fn editing_and_function_keys() {
        assert_eq!(encode(KeyCode::PageUp, KeyModifiers::NONE), b"\x1b[5~");
        assert_eq!(encode(KeyCode::Delete, KeyModifiers::SHIFT), b"\x1b[3;2~");
        assert_eq!(encode(KeyCode::F(1), KeyModifiers::NONE), b"\x1bOP");
        assert_eq!(encode(KeyCode::F(5), KeyModifiers::NONE), b"\x1b[15~");
        assert_eq!(encode(KeyCode::F(12), KeyModifiers::CONTROL), b"\x1b[24;5~");
    }
}
