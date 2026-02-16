//! TCP injection server for external text injection.
//!
//! Listens on 127.0.0.1:0 (auto-assign port) and accepts text injection.
//!
//! ## Protocol
//!
//! 1. Client connects to inject port
//! 2. Client sends text data
//! 3. Client closes connection (signals EOF)
//! 4. Server strips trailing `\n` (from echo/nc) but preserves `\r` for submit
//! 5. Server writes text to PTY master
//!
//! ## Submit vs Text-only
//!
//! - Text ending with `\r`: Submitted (Enter pressed)
//! - Text without `\r`: Text-only injection (for verification before Enter)
//!
//! The delivery loop uses text-only injection first, waits for the text to
//! appear in the input box, then sends a separate `\r` to submit.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;

/// Magic prefix for query commands (not injection)
const QUERY_PREFIX: u8 = 0x00;

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
        let listener = TcpListener::bind("127.0.0.1:0")
            .context("Failed to bind inject server")?;
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

    /// Get the listener raw file descriptor for polling
    pub fn listener_raw_fd(&self) -> i32 {
        self.listener.as_raw_fd()
    }

    /// Get raw file descriptors for active clients
    pub fn client_raw_fds(&self) -> impl Iterator<Item = i32> + '_ {
        self.clients.iter().map(|(stream, _)| stream.as_raw_fd())
    }

    /// Accept a new connection
    pub fn accept(&mut self) -> Result<()> {
        match self.listener.accept() {
            Ok((stream, _addr)) => {
                stream.set_nonblocking(true)?;
                self.clients.push((stream, Vec::new()));
                Ok(())
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Read from a client by index. Returns InjectResult:
    /// - Inject(text): text to write to PTY
    /// - ScreenQuery(index): caller should dump screen and call respond_query()
    /// - Pending: no data ready yet
    pub fn read_client(&mut self, index: usize) -> Result<InjectResult> {
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
                        let cmd = std::str::from_utf8(&data[1..])
                            .unwrap_or("")
                            .trim();
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

        Ok(InjectResult::Pending)
    }

    /// Process injection data: decode and strip trailing LF
    /// Fix #7: Use UTF-8 with Latin-1 fallback instead of lossy (which mangles bytes)
    fn process_inject_data(&self, data: &[u8]) -> String {
        let mut text = match String::from_utf8(data.to_vec()) {
            Ok(s) => s,
            Err(_) => {
                // Fallback to Latin-1 (preserves all byte values as chars)
                data.iter().map(|&b| b as char).collect()
            }
        };

        // Strip single trailing LF (from echo/nc), preserve CR for submit
        if text.ends_with('\n') {
            text.pop();
        }

        text
    }
}
