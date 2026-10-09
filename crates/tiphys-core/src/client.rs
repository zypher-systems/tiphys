//! A client's end of the daemon's socket.
//!
//! This is for programs that are not themselves async: the terminal app and
//! a one-shot run. Requests are written as they are sent, and a thread reads
//! events into a channel. When the daemon goes away the channel closes, which
//! is how the reader of it finds out.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use crate::proto::{Event, Request};
use crate::wire::{ClientFrame, PROTOCOL, ServerFrame, line};
use crate::{Error, Result};

/// How long the daemon has to answer a hello.
const HELLO_WITHIN: Duration = Duration::from_secs(10);

/// A connection to the daemon.
pub struct Client {
    writer: Mutex<UnixStream>,
    /// What the daemon sends. It closes when the connection is lost.
    pub events: Receiver<Event>,
}

/// Connects to the daemon at `socket`, to talk as `audience`.
pub fn connect(socket: &Path, audience: &str) -> Result<Client> {
    let unreachable = |e: std::io::Error| {
        Error::Io(format!(
            "the Tiphys daemon is not answering on {} ({e}); is it running?",
            socket.display()
        ))
    };
    let mut stream = UnixStream::connect(socket).map_err(unreachable)?;
    stream
        .set_read_timeout(Some(HELLO_WITHIN))
        .map_err(unreachable)?;
    let hello = ClientFrame::Hello {
        protocol: PROTOCOL,
        version: crate::VERSION.to_string(),
        audience: audience.to_string(),
    };
    stream
        .write_all(line(&hello).as_bytes())
        .map_err(unreachable)?;

    let mut reader = BufReader::new(stream.try_clone().map_err(unreachable)?);
    let mut answer = String::new();
    reader.read_line(&mut answer).map_err(unreachable)?;
    match serde_json::from_str::<ServerFrame>(&answer) {
        Ok(ServerFrame::Hello { .. }) => {}
        Ok(ServerFrame::Refused { message }) => {
            return Err(Error::Config(format!(
                "the Tiphys daemon turned this away: {message}"
            )));
        }
        _ => {
            return Err(Error::Io(format!(
                "what answered on {} is not a Tiphys daemon this version understands",
                socket.display()
            )));
        }
    }
    stream.set_read_timeout(None).map_err(unreachable)?;

    let (sender, events) = channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        loop {
            text.clear();
            match reader.read_line(&mut text) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            // A frame this version does not know is passed over; a newer
            // daemon may say things an older client has no use for.
            if let Ok(ServerFrame::Event { event }) = serde_json::from_str(&text)
                && sender.send(event).is_err()
            {
                return;
            }
        }
    });
    Ok(Client {
        writer: Mutex::new(stream),
        events,
    })
}

/// Closing the connection is what tells the daemon this client has gone, and
/// what lets the reading thread end.
impl Drop for Client {
    fn drop(&mut self) {
        if let Ok(stream) = self.writer.lock() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Client {
    /// Sends a request. A failure means the daemon is gone, which the event
    /// channel closing says as well.
    pub fn send(&self, request: Request) -> Result<()> {
        let frame = line(&ClientFrame::Request { request });
        self.writer
            .lock()
            .unwrap()
            .write_all(frame.as_bytes())
            .map_err(|e| Error::Io(format!("the connection to the Tiphys daemon was lost: {e}")))
    }
}
