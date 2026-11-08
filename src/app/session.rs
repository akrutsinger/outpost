//! Sessions: one connection's transcript, input line, and QUIC streams.

use std::net::SocketAddr;

use super::{Status, wrap};
use crate::net::quic::{self, QuicOut, StreamEnd, StreamId};
use crate::net::{Outlet, SessId};
use crate::text_input::TextInput;

pub(super) const MAX_LINES: usize = 10_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    In,
    Out,
    Info,
}

pub struct Line {
    pub dir: Dir,
    pub text: String,
    /// The QUIC stream it belongs to.
    pub stream: Option<StreamId>,
}

/// A QUIC session's streams.
#[derive(Default)]
pub struct Streams {
    /// Every stream seen, in order. Lines get stream tags once there are two.
    pub seen: Vec<StreamId>,
    /// Bidirectional streams we can still send on, in the order seen.
    pub writable: Vec<StreamId>,
    /// Where the next line goes; `None` opens a new stream.
    pub send_to: Option<StreamId>,
    /// How many streams we have opened.
    opened: u64,
}

impl Streams {
    pub fn tagged(&self) -> bool {
        self.seen.len() > 1
    }

    fn see(&mut self, stream: StreamId) {
        if !self.seen.contains(&stream) {
            self.seen.push(stream);
            if quic::is_bidi(stream) {
                self.writable.push(stream);
            }
        }
    }

    /// Opens our next stream, which the connection will number the same way.
    fn open(&mut self) -> StreamId {
        let stream = quic::server_bidi(self.opened);
        self.opened += 1;
        self.see(stream);
        stream
    }

    /// Steps the send target through the writable streams, then "new".
    fn cycle(&mut self, delta: isize) {
        let options: Vec<Option<StreamId>> = self
            .writable
            .iter()
            .copied()
            .map(Some)
            .chain([None])
            .collect();
        let index = options.iter().position(|&o| o == self.send_to).unwrap_or(0);
        self.send_to = options[wrap(index, delta, options.len())];
    }

    /// We can't send on `stream` any more; target the newest one left instead.
    fn stop_sending(&mut self, stream: StreamId) {
        self.writable.retain(|&s| s != stream);
        if self.send_to == Some(stream) {
            self.send_to = self.writable.last().copied();
        }
    }
}

pub struct Session {
    pub id: SessId,
    /// User-chosen name; `None` shows the peer address.
    pub name: Option<String>,
    pub peer: SocketAddr,
    outlet: Option<Outlet>,
    pub lines: Vec<Line>,
    /// The last line is incoming text still waiting for its newline.
    in_open: bool,
    pub unread: bool,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub input: TextInput,
    /// Rows scrolled up from the bottom of the transcript.
    pub scroll: usize,
    /// QUIC sessions only.
    pub streams: Option<Streams>,
}

impl Session {
    pub(super) fn new(id: SessId, peer: SocketAddr, outlet: Outlet) -> Self {
        let streams = matches!(outlet, Outlet::Quic(_)).then(Streams::default);
        Self {
            id,
            name: None,
            peer,
            outlet: Some(outlet),
            lines: vec![Line {
                dir: Dir::Info,
                text: format!("connected from {peer}"),
                stream: None,
            }],
            in_open: false,
            unread: true,
            bytes_in: 0,
            bytes_out: 0,
            input: TextInput::default(),
            scroll: 0,
            streams,
        }
    }

    pub fn is_open(&self) -> bool {
        self.outlet.is_some()
    }

    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.peer.to_string())
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

    pub(super) fn push(&mut self, dir: Dir, text: String) {
        self.push_on(dir, None, text);
    }

    /// Adds a line; whatever incoming line was still open is now closed.
    fn push_on(&mut self, dir: Dir, stream: Option<StreamId>, text: String) {
        self.in_open = false;
        self.lines.push(Line { dir, text, stream });
        if self.lines.len() > MAX_LINES {
            self.lines.drain(..self.lines.len() - MAX_LINES);
        }
    }

    pub(super) fn receive(&mut self, bytes: &[u8]) {
        self.receive_on(None, bytes);
    }

    /// QUIC data. Replies go to the stream that last sent something.
    pub(super) fn receive_stream(&mut self, stream: StreamId, bytes: &[u8]) {
        if let Some(streams) = &mut self.streams {
            streams.see(stream);
            if streams.writable.contains(&stream) {
                streams.send_to = Some(stream);
            }
        }
        self.receive_on(Some(stream), bytes);
    }

    pub(super) fn stream_ended(&mut self, stream: StreamId, end: StreamEnd) {
        let text = match end {
            StreamEnd::PeerFinished => "peer finished sending".to_string(),
            StreamEnd::PeerReset(code) => format!("reset by peer (code {code})"),
            StreamEnd::WriteFailed(e) => {
                if let Some(streams) = &mut self.streams {
                    streams.stop_sending(stream);
                }
                format!("can't send: {e}")
            }
        };
        self.push_on(Dir::Info, Some(stream), text);
    }

    fn receive_on(&mut self, stream: Option<StreamId>, bytes: &[u8]) {
        // A different stream's data starts its own line.
        if self.in_open && self.lines.last().map(|l| l.stream) != Some(stream) {
            self.in_open = false;
        }
        self.bytes_in += bytes.len() as u64;
        self.unread = true;
        // Lossy: a multi-byte character split across reads shows as U+FFFD.
        for ch in String::from_utf8_lossy(bytes).chars() {
            if ch == '\r' {
                continue;
            }
            if ch == '\n' {
                if !self.in_open {
                    self.push_on(Dir::In, stream, String::new());
                }
                self.in_open = false;
                continue;
            }
            if !self.in_open {
                self.push_on(Dir::In, stream, String::new());
                self.in_open = true;
            }
            let line = &mut self.lines.last_mut().expect("just pushed").text;
            match ch {
                '\t' => line.push_str("    "),
                c if c.is_control() => line.push_str(&format!("\\x{:02x}", c as u32)),
                c => line.push(c),
            }
        }
    }

    pub(super) fn submit(&mut self) {
        let Some(outlet) = &self.outlet else { return };
        let text = self.input.take();
        let mut bytes = text.clone().into_bytes();
        bytes.push(b'\n');
        let len = bytes.len() as u64;
        let (sent, stream) = match &mut self.streams {
            Some(streams) => {
                let stream = match streams.send_to {
                    Some(stream) => stream,
                    None => {
                        let stream = streams.open();
                        streams.send_to = Some(stream);
                        stream
                    }
                };
                (outlet.quic(QuicOut::Write { stream, bytes }), Some(stream))
            }
            None => (outlet.send(bytes), None),
        };
        if !sent {
            self.close(Some("send failed".into()));
            return;
        }
        self.bytes_out += len;
        self.push_on(Dir::Out, stream, text);
        self.scroll = 0;
    }

    /// Ends our side of the QUIC stream we're sending on.
    pub(super) fn finish_stream(&mut self) {
        let (Some(outlet), Some(streams)) = (&self.outlet, &mut self.streams) else {
            return;
        };
        let Some(stream) = streams.send_to else {
            return;
        };
        if outlet.quic(QuicOut::Finish(stream)) {
            streams.stop_sending(stream);
            self.push_on(Dir::Info, Some(stream), "finished sending".into());
        }
    }

    /// Picks the QUIC stream the next line goes to.
    pub(super) fn cycle_stream(&mut self, delta: isize) {
        if self.is_open()
            && let Some(streams) = &mut self.streams
        {
            streams.cycle(delta);
        }
    }

    pub(super) fn close(&mut self, error: Option<String>) {
        if self.outlet.take().is_none() {
            return;
        }
        self.in_open = false;
        let text = match error {
            Some(e) => format!("disconnected: {e}"),
            None => "disconnected".into(),
        };
        self.push(Dir::Info, text);
    }
}
