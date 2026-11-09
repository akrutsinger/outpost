//! Programs in pseudo-terminals, for terminal tabs. Like `net`, a PTY talks
//! to the app only through events: a reader thread forwards the program's
//! output, then how it exited.

use std::io::{self, Read, Write};

use portable_pty::{
    ChildKiller, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::Event;
use crate::net::SessId;

pub enum PtyEvent {
    Output {
        tab: SessId,
        bytes: Vec<u8>,
    },
    /// The program ended; `status` says how, e.g. `exited 0`.
    Exited {
        tab: SessId,
        status: String,
    },
}

/// A program running in a PTY. Dropping it hangs up on the program.
pub struct Pty {
    writer: Box<dyn Write + Send>,
    /// `None` for a test PTY, and once the program has exited.
    process: Option<Process>,
}

struct Process {
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

/// The user's shell, e.g. `zsh`: what a new terminal tab is called.
pub fn shell_name() -> String {
    std::env::var("SHELL")
        .ok()
        .and_then(|path| path.rsplit('/').next().map(String::from))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "sh".into())
}

impl Pty {
    /// Starts the user's login shell in their home directory.
    pub fn shell(
        tab: SessId,
        rows: u16,
        cols: u16,
        events: UnboundedSender<Event>,
    ) -> io::Result<Self> {
        Self::spawn(tab, CommandBuilder::new_default_prog(), rows, cols, events)
    }

    fn spawn(
        tab: SessId,
        mut cmd: CommandBuilder,
        rows: u16,
        cols: u16,
        events: UnboundedSender<Event>,
    ) -> io::Result<Self> {
        let pair = native_pty_system()
            .openpty(size(rows, cols))
            .map_err(io::Error::other)?;
        // What outpost's emulator understands, not the terminal outpost runs in.
        cmd.env("TERM", "xterm-256color");
        let mut child = pair.slave.spawn_command(cmd).map_err(io::Error::other)?;
        // Only the program holds the other end now, so reads stop when it exits.
        drop(pair.slave);
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
        let writer = pair.master.take_writer().map_err(io::Error::other)?;
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            // Linux reports an error rather than end-of-file once the program is gone.
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                let bytes = buf[..n].to_vec();
                if events
                    .send(Event::Pty(PtyEvent::Output { tab, bytes }))
                    .is_err()
                {
                    return;
                }
            }
            let status = match child.wait() {
                Ok(status) => exit_text(&status),
                Err(e) => e.to_string(),
            };
            let _ = events.send(Event::Pty(PtyEvent::Exited { tab, status }));
        });
        Ok(Self {
            writer,
            process: Some(Process {
                master: pair.master,
                killer,
            }),
        })
    }

    /// Sends `bytes` to the program, as if typed.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        if let Some(process) = &self.process {
            let _ = process.master.resize(size(rows, cols));
        }
    }

    /// The program exited and was reaped, so dropping the PTY must not
    /// signal its PID, which may already belong to another process.
    pub fn exited(&mut self) {
        self.process = None;
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if let Some(process) = &mut self.process {
            let _ = process.killer.kill();
        }
    }
}

fn size(rows: u16, cols: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn exit_text(status: &ExitStatus) -> String {
    match status.signal() {
        Some(signal) => format!("ended: {}", signal.to_lowercase()),
        None => format!("exited {}", status.exit_code()),
    }
}

#[cfg(test)]
impl Pty {
    /// A PTY with no program behind it. The receiver gets every write.
    pub fn test() -> (Self, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
        struct Channel(UnboundedSender<Vec<u8>>);
        impl Write for Channel {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0
                    .send(buf.to_vec())
                    .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let pty = Self {
            writer: Box::new(Channel(tx)),
            process: None,
        };
        (pty, rx)
    }
}

/// Real PTYs running `/bin/sh`, inside the test process.
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc::{self, UnboundedReceiver};
    use tokio::time::timeout;

    const WAIT: Duration = Duration::from_secs(5);

    fn sh(script: &str) -> CommandBuilder {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", script]);
        cmd
    }

    /// Collects output until the program exits; returns it and the status.
    async fn run_to_exit(rx: &mut UnboundedReceiver<Event>) -> (String, String) {
        let mut output = Vec::new();
        loop {
            match timeout(WAIT, rx.recv()).await {
                Ok(Some(Event::Pty(PtyEvent::Output { tab: 1, bytes }))) => output.extend(bytes),
                Ok(Some(Event::Pty(PtyEvent::Exited { tab: 1, status }))) => {
                    return (String::from_utf8_lossy(&output).into_owned(), status);
                }
                Ok(_) => panic!("unexpected event"),
                Err(_) => panic!(
                    "timed out; output so far: {:?}",
                    String::from_utf8_lossy(&output)
                ),
            }
        }
    }

    #[tokio::test]
    async fn output_then_exit_status() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _pty = Pty::spawn(1, sh("printf hello; exit 3"), 24, 80, tx).unwrap();
        let (output, status) = run_to_exit(&mut rx).await;
        assert_eq!(output, "hello");
        assert_eq!(status, "exited 3");
    }

    #[tokio::test]
    async fn input_reaches_the_program_and_resize_its_size() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut pty =
            Pty::spawn(1, sh("read line; echo got:$line; stty size"), 24, 80, tx).unwrap();
        pty.resize(30, 100);
        pty.write(b"ping\r").unwrap();
        let (output, status) = run_to_exit(&mut rx).await;
        assert!(output.contains("got:ping"), "{output:?}");
        assert!(output.contains("30 100"), "{output:?}");
        assert_eq!(status, "exited 0");
    }

    #[tokio::test]
    async fn dropping_hangs_up_on_the_program() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let pty = Pty::spawn(1, sh("echo ready; exec sleep 30"), 24, 80, tx).unwrap();
        match timeout(WAIT, rx.recv()).await {
            Ok(Some(Event::Pty(PtyEvent::Output { .. }))) => {}
            _ => panic!("expected output first"),
        }
        drop(pty);
        let (_, status) = run_to_exit(&mut rx).await;
        assert_eq!(status, "ended: hangup");
    }
}
