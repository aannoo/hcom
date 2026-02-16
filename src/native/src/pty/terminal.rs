//! Terminal handling - raw mode setup and signal handlers
//!
//! Key functionality:
//! - TerminalGuard: RAII wrapper that restores terminal on drop
//! - Raw mode: Disable line buffering, echo, etc.
//! - Signal handlers: SIGWINCH, SIGINT, SIGTERM

use anyhow::{Context, Result};
use nix::pty::Winsize;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use nix::sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr};
use nix::unistd::isatty;
use std::io;
use std::os::fd::AsRawFd;

use super::{handle_sigwinch, handle_sigint, handle_sigterm, handle_sighup};

/// RAII guard that restores terminal settings on drop.
///
/// When created, puts the terminal into raw mode (no line buffering, no echo,
/// special characters disabled). When dropped, restores the original settings.
///
/// If stdin is not a TTY (headless/background mode), the guard is a no-op.
pub struct TerminalGuard {
    original_termios: Option<Termios>,
}

impl TerminalGuard {
    /// Create a new terminal guard, setting raw mode
    pub fn new() -> Result<Self> {
        let original_termios = setup_raw_mode()?;
        Ok(Self { original_termios })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Some(ref termios) = self.original_termios {
            let _ = tcsetattr(io::stdin(), SetArg::TCSANOW, termios);
        }
    }
}

/// Setup raw terminal mode, returns original termios for restoration.
///
/// Returns `None` if stdin is not a TTY (headless/pipe mode), in which case
/// no terminal settings need to be changed or restored.
fn setup_raw_mode() -> Result<Option<Termios>> {
    let stdin = io::stdin();
    if !isatty(&stdin).unwrap_or(false) {
        // Not a TTY - no terminal settings to modify
        return Ok(None);
    }

    let original = tcgetattr(&stdin).context("tcgetattr failed")?;
    let mut raw = original.clone();
    cfmakeraw(&mut raw);
    tcsetattr(&stdin, SetArg::TCSANOW, &raw).context("tcsetattr failed")?;
    Ok(Some(original))
}

/// Get current terminal size
pub fn get_terminal_size() -> Result<Winsize> {
    // SAFETY: Winsize is a C struct with no invariants beyond being properly initialized.
    // mem::zeroed() produces a valid Winsize with all fields set to 0, which is safe.
    let mut ws: Winsize = unsafe { std::mem::zeroed() };

    // SAFETY:
    // - stdout fd is valid: stdout() returns a valid file descriptor (fd 1) inherited from the process
    // - ws is properly initialized via mem::zeroed() above; ioctl will write terminal size to it
    // - TIOCGWINSZ is the correct ioctl request for querying terminal window size
    // - Return value is checked below; on error (ret == -1) or invalid size, we fall back to 80x24
    let ret = unsafe {
        libc::ioctl(
            io::stdout().as_raw_fd(),
            libc::TIOCGWINSZ.into(),
            &mut ws,
        )
    };
    if ret == -1 || ws.ws_row == 0 || ws.ws_col == 0 {
        // Fallback to default size
        ws.ws_row = 24;
        ws.ws_col = 80;
    }
    Ok(ws)
}

/// Setup signal handler for a specific signal
fn setup_signal_handler(signal: Signal, handler: extern "C" fn(libc::c_int), restart: bool) -> Result<()> {
    let flags = if restart { SaFlags::SA_RESTART } else { SaFlags::empty() };
    let action = SigAction::new(
        SigHandler::Handler(handler),
        flags,
        SigSet::empty(),
    );
    unsafe { sigaction(signal, &action) }.context(format!("sigaction {:?} failed", signal))?;
    Ok(())
}

/// Setup all required signal handlers
pub fn setup_signal_handlers() -> Result<()> {
    // SIGPIPE: ignore — writes to broken pipes (revoked terminal, closed TCP) return EPIPE
    // instead of killing the process. Without this, a write to stdout after terminal close
    // can race SIGHUP and kill us before cleanup runs.
    let ignore = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    unsafe { sigaction(Signal::SIGPIPE, &ignore) }.context("sigaction SIGPIPE failed")?;

    // SIGWINCH: restart syscalls (we just update size, no need to interrupt)
    setup_signal_handler(Signal::SIGWINCH, handle_sigwinch, true)?;
    // SIGINT: restart (forwarded to child, we don't exit)
    setup_signal_handler(Signal::SIGINT, handle_sigint, true)?;
    // SIGTERM/SIGHUP: DON'T restart - we need poll() to return EINTR so we can exit
    setup_signal_handler(Signal::SIGTERM, handle_sigterm, false)?;
    setup_signal_handler(Signal::SIGHUP, handle_sighup, false)?;
    Ok(())
}
