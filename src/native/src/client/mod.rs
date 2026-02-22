//! Daemon client for fast hook/CLI handling.
//!
//! Connects to Python daemon via Unix socket for <20ms latency.
//! Falls back to direct Python execution if daemon unavailable.

mod connection;
mod daemon;
mod protocol;
pub use daemon::{exec_python_fallback, run};
