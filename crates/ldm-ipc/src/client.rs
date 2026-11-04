//! Small blocking client for the `ldmd` Unix socket (NDJSON envelopes).
//! Used by the CLI, the native host and tests.
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::Envelope;

pub struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
    /// Unsolicited events received while waiting for a reply.
    pub events: VecDeque<Envelope>,
}

impl Client {
    pub fn connect(path: &Path) -> io::Result<Client> {
        let s = UnixStream::connect(path)?;
        let r = s.try_clone()?;
        Ok(Client { writer: s, reader: BufReader::new(r), next_id: 1, events: VecDeque::new() })
    }

    pub fn set_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.reader.get_ref().set_read_timeout(d)
    }

    pub fn send(&mut self, kind: &str, payload: Value) -> io::Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = Envelope::new(id, kind, payload).to_json();
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        Ok(id)
    }

    /// Read the next envelope from the daemon (`Ok(None)` on EOF).
    pub fn read(&mut self) -> io::Result<Option<Envelope>> {
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                return Ok(None);
            }
            if line.trim().is_empty() {
                continue;
            }
            return Envelope::parse(line.trim().as_bytes())
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
        }
    }

    /// Send a request and wait for its first reply.
    pub fn request(&mut self, kind: &str, payload: Value) -> io::Result<Envelope> {
        Ok(self.request_all(kind, payload, 1)?.remove(0))
    }

    /// Send a request and collect `n` replies carrying its id.
    pub fn request_all(&mut self, kind: &str, payload: Value, n: usize) -> io::Result<Vec<Envelope>> {
        let id = self.send(kind, payload)?;
        let mut out = vec![];
        while out.len() < n {
            match self.read()? {
                None => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed the connection")),
                Some(e) if e.id == id => out.push(e),
                Some(e) => self.events.push_back(e),
            }
        }
        Ok(out)
    }

    /// Next event (buffered first).
    pub fn next_event(&mut self) -> io::Result<Option<Envelope>> {
        if let Some(e) = self.events.pop_front() {
            return Ok(Some(e));
        }
        self.read()
    }
}
