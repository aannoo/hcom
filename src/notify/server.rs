//! TCP notification server for instant wake on message arrival.
//!
//! Used by the delivery loop to block efficiently instead of busy-polling.
//! When a message is sent (`hcom send`), the wake helpers in
//! `crate::notify::wake` connect briefly to each instance's notify port to
//! wake its delivery thread.
//!
//! TCP chosen for clean poll/select integration across process boundaries.

use anyhow::{Context, Result};
use std::net::TcpListener;
use std::time::Duration;

/// TCP notification server for wake-ups
pub struct NotifyServer {
    listener: TcpListener,
    port: u16,
}

impl NotifyServer {
    /// Create a new notify server bound to localhost on auto-assigned port
    pub fn new() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").context("Failed to bind notify server")?;
        let port = listener.local_addr()?.port();

        // Set non-blocking for poll-based waiting
        listener.set_nonblocking(true)?;

        Ok(Self { listener, port })
    }

    /// Get the port the server is listening on
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Wait for notification or timeout
    ///
    /// Returns true if notified (connection received), false on timeout
    pub fn wait(&self, timeout: Duration) -> bool {
        if crate::sys::net::wait_readable(&self.listener, timeout) {
            // Drain all pending notifications
            self.drain();
            true
        } else {
            false
        }
    }

    /// Drain all pending connections (accept and close)
    fn drain(&self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // Just accepting wakes us up; close immediately
                    drop(stream);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }
}
