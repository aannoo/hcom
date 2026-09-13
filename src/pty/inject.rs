//! TCP injection server — accepts text on a local port and writes to PTY master.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::fd::AsRawFd;

/// Magic prefix for query commands (not injection)
const QUERY_PREFIX: u8 = 0x00;

/// Magic prefix for raw injection: the payload passes through verbatim, keeping
/// any trailing newline. Used by the Hermes ACP delivery loop, whose
/// newline-delimited JSON-RPC framing requires an intact trailing `\n`.
pub(crate) const RAW_PREFIX: u8 = 0x01;

/// Result of reading from an inject client
pub enum InjectResult {
    /// Text to inject into PTY
    Inject(String),
    /// Query command — client removed from vec, caller must respond via stream
    Query(QueryClient),
    /// No data ready yet
    Pending,
}

/// A query client removed from the connection pool, ready for response
pub struct QueryClient {
    stream: TcpStream,
    pub command: QueryCommand,
}

#[derive(Debug)]
pub enum QueryCommand {
    Screen,
    Unknown,
}

impl QueryClient {
    /// Send response and close connection
    pub fn respond(mut self, response: &str) {
        let _ = self.stream.write_all(response.as_bytes());
        let _ = self.stream.flush();
        // stream dropped here, connection closed
    }
}

/// TCP server for text injection
pub struct InjectServer {
    listener: TcpListener,
    port: u16,
    clients: Vec<(TcpStream, Vec<u8>)>,
}

impl InjectServer {
    /// Create a new injection server on localhost
    pub fn new() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").context("Failed to bind inject server")?;
        let port = listener.local_addr()?.port();

        // Set non-blocking
        listener.set_nonblocking(true)?;

        Ok(Self {
            listener,
            port,
            clients: Vec::new(),
        })
    }

    /// Get the port the server is listening on
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Get the listener raw file descriptor for polling (Unix poll loop only).
    #[cfg(unix)]
    pub fn listener_raw_fd(&self) -> i32 {
        self.listener.as_raw_fd()
    }

    /// Get raw file descriptors for active clients (Unix poll loop only).
    #[cfg(unix)]
    pub fn client_raw_fds(&self) -> impl Iterator<Item = i32> + '_ {
        self.clients.iter().map(|(stream, _)| stream.as_raw_fd())
    }

    /// Number of connected inject clients (portable; the Unix loop uses
    /// `client_raw_fds().count()`, but tests and the Windows loop need this
    /// without raw fds).
    #[cfg(any(test, windows))]
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Accept a new connection.
    ///
    /// Returns `Ok(true)` if a connection was accepted, `Ok(false)` if the accept
    /// queue was empty (WouldBlock). The caller uses this to apply backoff on
    /// macOS, where a non-blocking listener can keep reporting POLLIN via poll()
    /// even after the accept queue is drained.
    pub fn accept(&mut self) -> Result<bool> {
        match self.listener.accept() {
            Ok((stream, _addr)) => {
                stream.set_nonblocking(true)?;
                self.clients.push((stream, Vec::new()));
                Ok(true)
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Read from a client by index. Returns InjectResult:
    /// - Inject(text): text to write to PTY
    /// - ScreenQuery(index): caller should dump screen and call respond_query()
    /// - Pending: no data ready yet
    pub fn read_client(&mut self, index: usize) -> Result<InjectResult> {
        // Ensure any pending connections are accepted so that the requested index is valid.
        while self.clients.len() <= index {
            if !self.accept()? {
                break;
            }
        }
        if index >= self.clients.len() {
            return Ok(InjectResult::Pending);
        }

        let (stream, buffer) = &mut self.clients[index];
        let mut buf = [0u8; 8192];

        loop {
            match stream.read(&mut buf) {
                Ok(0) => {
                    // EOF - client closed, process the data
                    let data = std::mem::take(buffer);

                    // Check for command (starts with \x00)
                    if data.first() == Some(&QUERY_PREFIX) {
                        let cmd = std::str::from_utf8(&data[1..]).unwrap_or("").trim();
                        let (stream, _) = self.clients.remove(index);
                        let command = match cmd {
                            "SCREEN" => QueryCommand::Screen,
                            _ => QueryCommand::Unknown,
                        };
                        return Ok(InjectResult::Query(QueryClient { stream, command }));
                    }

                    self.clients.remove(index);
                    return Ok(InjectResult::Inject(self.process_inject_data(&data)));
                }
                Ok(n) => {
                    buffer.extend_from_slice(&buf[..n]);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    break;
                }
                Err(e) => {
                    self.clients.remove(index);
                    return Err(e.into());
                }
            }
        }

        // If the client closed its write side without a zero‑length read (e.g.,
        // on platforms where shutdown triggers WouldBlock first), treat any buffered
        // data as complete.
        if !buffer.is_empty() {
            let data = std::mem::take(buffer);
            // Process possible query command.
            if data.first() == Some(&QUERY_PREFIX) {
                let cmd = std::str::from_utf8(&data[1..]).unwrap_or("").trim();
                let (stream, _) = self.clients.remove(index);
                let command = match cmd {
                    "SCREEN" => QueryCommand::Screen,
                    _ => QueryCommand::Unknown,
                };
                return Ok(InjectResult::Query(QueryClient { stream, command }));
            }
            self.clients.remove(index);
            return Ok(InjectResult::Inject(self.process_inject_data(&data)));
        }
        Ok(InjectResult::Pending)
    }

    /// Process injection data: decode and strip trailing LF
    /// Fix #7: Use UTF-8 with Latin-1 fallback instead of lossy (which mangles bytes)
    fn process_inject_data(&self, data: &[u8]) -> String {
        // Determine if this is a raw injection (prefix 0x01).
        let (raw, payload) = match data.first() {
            Some(&RAW_PREFIX) => (true, &data[1..]),
            _ => (false, data),
        };

        // Decode payload, falling back to Latin‑1 if UTF‑8 is invalid.
        let decoded = match String::from_utf8(payload.to_vec()) {
            Ok(s) => s,
            Err(_) => payload.iter().map(|&b| b as char).collect(),
        };

        if raw {
            // If the payload is valid UTF‑8 and contains no control characters (except
            // the framing newline), we can return it unchanged – the newline is kept.
            let utf8_ok = String::from_utf8(payload.to_vec()).is_ok();
            let has_control = decoded.chars().any(|c| (c as u32) <= 0x1F && c != '\n');
            if utf8_ok && !has_control {
                return decoded;
            }
            // Raw injection: pass through verbatim (including newline and control bytes).
            return decoded;
        }

        // Non‑raw injection: strip a single trailing LF (typical of echo/nc), preserve CR.
        let mut s = decoded;
        if s.ends_with('\n') {
            s.pop();
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::{InjectResult, InjectServer};
    use std::io::Write;
    use std::net::{Shutdown, TcpStream};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn accept_returns_false_when_queue_is_empty() {
        let mut server = InjectServer::new().unwrap();

        assert!(!server.accept().unwrap());
        assert_eq!(server.client_count(), 0);
    }

    #[test]
    fn accept_returns_true_when_connection_is_pending() {
        let mut server = InjectServer::new().unwrap();
        let _client = TcpStream::connect(("127.0.0.1", server.port())).unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        let accepted = loop {
            if server.accept().unwrap() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(10));
        };

        assert!(accepted);
        assert_eq!(server.client_count(), 1);
    }

    #[test]
    fn completed_clients_can_be_drained_in_connection_order() {
        let mut server = InjectServer::new().unwrap();
        let mut first = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        first.write_all(b"text").unwrap();
        first.shutdown(Shutdown::Write).unwrap();
        let mut second = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        second.write_all(b"\r").unwrap();
        second.shutdown(Shutdown::Write).unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        while server.client_count() < 2 && Instant::now() < deadline {
            let _ = server.accept();
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(server.client_count(), 2);

        let read_next = |server: &mut InjectServer| {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                if let InjectResult::Inject(text) = server.read_client(0).unwrap() {
                    break text;
                }
                assert!(Instant::now() < deadline, "client did not complete");
                thread::sleep(Duration::from_millis(5));
            }
        };
        assert_eq!(read_next(&mut server), "text");
        assert_eq!(read_next(&mut server), "\r");
    }

    #[test]
    fn raw_prefix_preserves_trailing_newline() {
        let mut server = InjectServer::new().unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        let json_line = br#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#;
        let mut payload = Vec::new();
        payload.push(super::RAW_PREFIX);
        payload.extend_from_slice(json_line);
        payload.push(b'\n');
        client.write_all(&payload).unwrap();
        client.shutdown(Shutdown::Write).unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let InjectResult::Inject(text) = server.read_client(0).unwrap() {
                let expected = String::from_utf8(json_line.to_vec()).unwrap();
                assert_eq!(text, expected + "\n");
                break;
            }
            assert!(Instant::now() < deadline, "raw client did not complete");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn raw_prefix_keeps_c0_bytes() {
        let mut server = InjectServer::new().unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        client
            .write_all(&[super::RAW_PREFIX, b'a', b'\x1b', b'b', b'\n'])
            .unwrap();
        client.shutdown(Shutdown::Write).unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let InjectResult::Inject(text) = server.read_client(0).unwrap() {
                assert_eq!(text, "a\u{1b}b\n");
                break;
            }
            assert!(Instant::now() < deadline, "raw client did not complete");
            thread::sleep(Duration::from_millis(5));
        }
    }
}
