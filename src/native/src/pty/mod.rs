//! PTY wrapper module - spawns child process with terminal emulation
//!
//! Components:
//! - Proxy: Main PTY loop with I/O forwarding
//! - Terminal: Raw mode and signal handling
//! - Screen: vt100-based screen tracking
//! - Inject: TCP injection server
//! - Delivery: Notify-driven message delivery (integrated)

mod terminal;
pub mod screen;
mod inject;

use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::openpty;
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Pid, read, write};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, RwLock};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use terminal::TerminalGuard;
use screen::ScreenTracker;
use inject::InjectServer;

use crate::config::Config;
use crate::db::HcomDb;
use crate::delivery::{DeliveryState, ScreenState, ToolConfig, run_delivery_loop, status_icon};
use crate::log::{log_info, log_error, log_warn};
use crate::notify::NotifyServer;

/// Tracks what type of incomplete escape sequence is pending on stdout.
/// Used to defer title writes until the sequence completes across read boundaries.
#[derive(Clone, Copy, PartialEq, Debug)]
enum PendingEscape {
    None,
    /// Incomplete CSI (ESC [) — complete when final byte (0x40-0x7E) appears
    Csi,
    /// Incomplete string sequence (OSC 3+, DCS, PM, APC) — complete when BEL (0x07)
    /// or ST (ESC \) appears. Title OSCs (0/1/2) are stripped by TitleOscFilter.
    StringSeq,
}

/// Check if it's safe to write title OSC to stdout.
///
/// Three guards prevent corruption from interleaving with tool output:
/// 1. `had_pty_output` — no PTY data this poll iteration (same-iteration guard)
/// 2. `pending_utf8` — no incomplete UTF-8 multi-byte sequence (cross-iteration)
/// 3. `pending_escape` — no incomplete CSI/OSC escape sequence on stdout
///    (cross-iteration guard for escape sequences, which are all-ASCII
///    and invisible to pending_utf8)
#[inline]
fn title_write_safe(had_pty_output: bool, pending_utf8: u8, pending_escape: PendingEscape) -> bool {
    !had_pty_output && pending_utf8 == 0 && pending_escape == PendingEscape::None
}

/// Check if data ends inside an incomplete escape sequence.
///
/// Scans backwards for the last ESC (0x1b) and checks whether the escape
/// sequence that starts there has a valid terminator. Returns the type of
/// pending escape for cross-chunk continuation tracking. Handles:
/// - CSI (`ESC [` ... final byte 0x40-0x7E)
/// - OSC (`ESC ]` ... BEL or ST)
/// - DCS/PM/APC (`ESC P`/`ESC ^`/`ESC _` ... ST)
///
/// Note: The TitleOscFilter eats ESC bytes it's tracking (SawEsc state),
/// so those never appear in the filtered output. This function only sees
/// ESC bytes that the filter passed through (non-title sequences).
#[inline]
fn has_pending_escape(data: &[u8]) -> PendingEscape {
    if data.is_empty() {
        return PendingEscape::None;
    }

    // Scan backwards for the last ESC
    let mut esc_pos = None;
    for i in (0..data.len()).rev() {
        if data[i] == 0x1b {
            esc_pos = Some(i);
            break;
        }
    }

    let esc_pos = match esc_pos {
        Some(pos) => pos,
        None => return PendingEscape::None,
    };

    let after = &data[esc_pos + 1..];
    if after.is_empty() {
        // ESC at end — TitleOscFilter should have eaten this, but be safe
        return PendingEscape::Csi;
    }

    match after[0] {
        b'[' => {
            // CSI: complete when a final byte (0x40-0x7E) appears after params
            for &b in &after[1..] {
                if (0x40..=0x7E).contains(&b) {
                    return PendingEscape::None;
                }
            }
            PendingEscape::Csi
        }
        b']' => {
            // OSC: complete when BEL (0x07) or ST (ESC \) appears
            // TitleOscFilter strips OSC 0/1/2; this catches OSC 8+ (hyperlinks etc.)
            let content = &after[1..];
            let mut i = 0;
            while i < content.len() {
                if content[i] == 0x07 {
                    return PendingEscape::None;
                }
                if content[i] == 0x1b && i + 1 < content.len() && content[i + 1] == b'\\' {
                    return PendingEscape::None;
                }
                i += 1;
            }
            PendingEscape::StringSeq
        }
        b'P' | b'^' | b'_' => {
            // DCS / PM / APC: terminated by ST (ESC \)
            let content = &after[1..];
            let mut i = 0;
            while i < content.len() {
                if content[i] == 0x1b && i + 1 < content.len() && content[i + 1] == b'\\' {
                    return PendingEscape::None;
                }
                i += 1;
            }
            PendingEscape::StringSeq
        }
        _ => {
            // Simple 2-byte escape (ESC + letter) — always complete
            PendingEscape::None
        }
    }
}

/// Resolve pending escape state when a continuation chunk has no ESC byte.
///
/// When the previous read left an incomplete escape and the current chunk
/// has no new ESC, check whether a type-appropriate terminator appears:
/// - CSI: any byte in 0x40-0x7E (the final byte)
/// - StringSeq: BEL (0x07) — ST (ESC \) requires ESC, handled by caller
#[inline]
fn resolve_pending_escape(pending: PendingEscape, data: &[u8]) -> PendingEscape {
    match pending {
        PendingEscape::None => PendingEscape::None,
        PendingEscape::Csi => {
            if data.iter().any(|&b| (0x40..=0x7E).contains(&b)) {
                PendingEscape::None
            } else {
                PendingEscape::Csi
            }
        }
        PendingEscape::StringSeq => {
            if data.contains(&0x07) {
                PendingEscape::None
            } else {
                PendingEscape::StringSeq
            }
        }
    }
}

/// Check if buffer ends with an incomplete UTF-8 multi-byte sequence.
/// Returns the number of continuation bytes still expected (0-3).
///
/// This is used to defer writing our title OSC until the UTF-8 sequence completes,
/// preventing corruption when PTY reads split multi-byte characters.
///
/// UTF-8 encoding:
/// - 1-byte: 0xxxxxxx (0x00-0x7F) - complete
/// - 2-byte: 110xxxxx 10xxxxxx (starts 0xC0-0xDF)
/// - 3-byte: 1110xxxx 10xxxxxx 10xxxxxx (starts 0xE0-0xEF)
/// - 4-byte: 11110xxx 10xxxxxx 10xxxxxx 10xxxxxx (starts 0xF0-0xF7)
#[inline]
fn pending_utf8_bytes(data: &[u8]) -> u8 {
    if data.is_empty() {
        return 0;
    }

    // Check last 1-3 bytes for incomplete multi-byte sequence start
    // Work backwards from end to find potential incomplete sequence
    let len = data.len();

    // Check if we're in the middle of a multi-byte sequence
    // by looking for a leading byte without all its continuation bytes

    // Check last byte first
    let last = data[len - 1];

    // If last byte is ASCII (< 0x80), we're complete
    if last < 0x80 {
        return 0;
    }

    // If last byte is a continuation byte (10xxxxxx), check if sequence is complete
    // by scanning backwards for the leading byte
    if (last & 0xC0) == 0x80 {
        // Count how many continuation bytes we have at the end
        let mut cont_count = 1;
        let mut pos = len - 2;
        while pos < len && (data[pos] & 0xC0) == 0x80 {
            cont_count += 1;
            if pos == 0 {
                break;
            }
            pos = pos.wrapping_sub(1);
        }

        // Find the leading byte
        if pos < len && (data[pos] & 0xC0) != 0x80 {
            let lead = data[pos];
            let expected = if (lead & 0xF8) == 0xF0 {
                3 // 4-byte sequence
            } else if (lead & 0xF0) == 0xE0 {
                2 // 3-byte sequence
            } else if (lead & 0xE0) == 0xC0 {
                1 // 2-byte sequence
            } else {
                0 // Invalid or ASCII
            };

            if cont_count < expected {
                return (expected - cont_count) as u8;
            }
        }
        return 0; // Sequence complete or invalid
    }

    // Last byte is a leading byte - check which type
    if (last & 0xF8) == 0xF0 {
        return 3; // 4-byte sequence, needs 3 more
    } else if (last & 0xF0) == 0xE0 {
        return 2; // 3-byte sequence, needs 2 more
    } else if (last & 0xE0) == 0xC0 {
        return 1; // 2-byte sequence, needs 1 more
    }

    0 // Complete or invalid
}

/// Stateful title OSC filter — strips OSC 0/1/2 (title/icon) sequences even when
/// split across read() boundaries.
///
/// Different from the old TitleEscapeFilter (removed c6bc73c2) which buffered entire
/// OSC sequences including real output to replace them inline (caused timing delays).
/// This filter only DISCARDS title bytes — real output passes through immediately.
/// Max 3 prefix bytes (ESC, ], digit) held at buffer boundary for one poll cycle.
#[derive(Clone, Copy, PartialEq)]
enum TitleFilterState {
    Pass,
    SawEsc,
    SawBracket,
    /// Saw ESC ] followed by 0, 1, or 2. Waiting for ; to confirm title.
    SawDigit(u8),
    /// Inside title content. Discarding until BEL (0x07) or ST (ESC \).
    InTitle,
    /// Inside title, saw ESC. Check next byte for \ (ST terminator).
    InTitleSawEsc,
}

struct TitleOscFilter {
    state: TitleFilterState,
    discard_count: usize,
}

impl TitleOscFilter {
    fn new() -> Self {
        Self {
            state: TitleFilterState::Pass,
            discard_count: 0,
        }
    }

    /// Filter data, stripping title OSC sequences. Returns (filtered_output, had_title).
    #[inline]
    fn filter(&mut self, data: &[u8]) -> (Vec<u8>, bool) {
        let mut result = Vec::with_capacity(data.len());
        let mut found_title = false;

        for &byte in data {
            match self.state {
                TitleFilterState::Pass => {
                    if byte == 0x1b {
                        self.state = TitleFilterState::SawEsc;
                    } else {
                        result.push(byte);
                    }
                }
                TitleFilterState::SawEsc => {
                    if byte == b']' {
                        self.state = TitleFilterState::SawBracket;
                    } else {
                        result.push(0x1b);
                        result.push(byte);
                        self.state = TitleFilterState::Pass;
                    }
                }
                TitleFilterState::SawBracket => {
                    if byte == b'0' || byte == b'1' || byte == b'2' {
                        self.state = TitleFilterState::SawDigit(byte);
                    } else {
                        result.push(0x1b);
                        result.push(b']');
                        result.push(byte);
                        self.state = TitleFilterState::Pass;
                    }
                }
                TitleFilterState::SawDigit(digit) => {
                    if byte == b';' {
                        // Confirmed title OSC — discard until terminator
                        self.state = TitleFilterState::InTitle;
                        self.discard_count = 0;
                        found_title = true;
                    } else {
                        // Multi-digit OSC number (10, 11, etc.) or malformed — pass through
                        result.push(0x1b);
                        result.push(b']');
                        result.push(digit);
                        result.push(byte);
                        self.state = TitleFilterState::Pass;
                    }
                }
                TitleFilterState::InTitle => {
                    self.discard_count += 1;
                    if byte == 0x07 {
                        self.state = TitleFilterState::Pass;
                    } else if byte == 0x1b {
                        self.state = TitleFilterState::InTitleSawEsc;
                    } else if self.discard_count > 256 {
                        // Safety: abort on absurdly long unterminated sequence
                        self.state = TitleFilterState::Pass;
                    }
                }
                TitleFilterState::InTitleSawEsc => {
                    self.discard_count += 1;
                    if byte == b'\\' {
                        // ST terminator (ESC \)
                        self.state = TitleFilterState::Pass;
                    } else {
                        self.state = TitleFilterState::InTitle;
                    }
                }
            }
        }

        (result, found_title)
    }

    /// Flush held prefix bytes on EOF/exit.
    fn flush(&self) -> Vec<u8> {
        match self.state {
            TitleFilterState::SawEsc => vec![0x1b],
            TitleFilterState::SawBracket => vec![0x1b, b']'],
            TitleFilterState::SawDigit(d) => vec![0x1b, b']', d],
            _ => Vec::new(),
        }
    }
}

// Signal flags (set by signal handlers, checked in main loop)
static SIGWINCH_RECEIVED: AtomicBool = AtomicBool::new(false);
static SIGINT_RECEIVED: AtomicBool = AtomicBool::new(false);
static SIGTERM_RECEIVED: AtomicBool = AtomicBool::new(false);
static SIGHUP_RECEIVED: AtomicBool = AtomicBool::new(false);

// Exit reason flag (for cleanup to know context)
// false = normal exit (closed), true = signal exit (killed)
// Pub so delivery.rs can check it during cleanup
pub static EXIT_WAS_KILLED: AtomicBool = AtomicBool::new(false);

pub extern "C" fn handle_sigwinch(_: libc::c_int) {
    SIGWINCH_RECEIVED.store(true, Ordering::Release);
}

pub extern "C" fn handle_sigint(_: libc::c_int) {
    SIGINT_RECEIVED.store(true, Ordering::Release);
}

pub extern "C" fn handle_sigterm(_: libc::c_int) {
    SIGTERM_RECEIVED.store(true, Ordering::Release);
}

extern "C" fn handle_sighup(_: libc::c_int) {
    SIGHUP_RECEIVED.store(true, Ordering::Release);
}

/// Build minimal launch_context JSON from env vars available in the PTY process.
/// Captures terminal_preset and process_id — the fields needed by kill to close the pane.
/// The Python hook captures the full context (git_branch, tty, env snapshot) later.
fn build_early_launch_context() -> String {
    use serde_json::{Map, Value};

    let mut ctx = Map::new();

    if let Ok(preset) = std::env::var("HCOM_LAUNCHED_PRESET") {
        if !preset.is_empty() {
            ctx.insert("terminal_preset".into(), Value::String(preset));
        }
    }
    if let Ok(pid) = std::env::var("HCOM_PROCESS_ID") {
        if !pid.is_empty() {
            ctx.insert("process_id".into(), Value::String(pid));
        }
    }

    // Capture pane_id env vars for common terminal presets
    for var in &["WEZTERM_PANE", "TMUX_PANE", "KITTY_WINDOW_ID"] {
        if let Ok(val) = std::env::var(var) {
            if !val.is_empty() {
                ctx.insert("pane_id".into(), Value::String(val));
                break;
            }
        }
    }

    // Read terminal_id from temp file written by parent's launch stdout capture.
    // This is the ID returned by `kitten @ launch` (or similar) and serves as
    // fallback for pane_id when the terminal env var isn't available.
    //
    // Race condition: parent writes this file after `kitten @ launch` returns
    // (~500ms after child starts), but we run within ~10-100ms of spawn.
    // Retry with backoff only when pane_id not already captured from env vars
    // (tmux/wezterm set env vars directly, no file needed).
    if let Some(process_id) = ctx.get("process_id").and_then(|v| v.as_str()) {
        let id_file = crate::paths::hcom_dir().join(".tmp").join("terminal_ids").join(process_id);
        let needs_id = !ctx.contains_key("pane_id");
        let max_attempts: usize = if needs_id { 10 } else { 1 };
        let mut terminal_id_value = String::new();

        for attempt in 0..max_attempts {
            match std::fs::read_to_string(&id_file) {
                Ok(contents) => {
                    let trimmed = contents.trim().to_string();
                    if !trimmed.is_empty() {
                        terminal_id_value = trimmed;
                        break;
                    }
                }
                Err(_) => {}
            }
            if attempt + 1 < max_attempts {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }

        if !terminal_id_value.is_empty() {
            ctx.insert("terminal_id".into(), Value::String(terminal_id_value.clone()));
            if !ctx.contains_key("pane_id") {
                ctx.insert("pane_id".into(), Value::String(terminal_id_value));
            }
        }
        let _ = std::fs::remove_file(&id_file);
    }

    Value::Object(ctx).to_string()
}

/// Configuration for the PTY proxy
pub struct ProxyConfig {
    /// Pattern to detect when tool is ready (e.g., b"? for shortcuts")
    pub ready_pattern: Vec<u8>,
    /// Instance name for logging and database tracking
    #[allow(dead_code)]
    pub instance_name: Option<String>,
    /// Tool name (claude, gemini, codex)
    pub tool: String,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            ready_pattern: b"? for shortcuts".to_vec(),
            instance_name: None,
            tool: "claude".to_string(),
        }
    }
}

/// PTY proxy that manages the child process and I/O forwarding
pub struct Proxy {
    config: ProxyConfig,
    pty_master: OwnedFd,
    child: Child,
    _terminal_guard: TerminalGuard,
    screen: ScreenTracker,
    inject_server: InjectServer,
    last_user_input: Instant,
    user_activity_cooldown_ms: u64,
    /// Shared delivery state (for delivery thread)
    delivery_state: Arc<RwLock<ScreenState>>,
    /// Running flag for delivery thread
    running: Arc<AtomicBool>,
    /// Last resize time for debouncing (fix #3)
    last_resize: Option<Instant>,
    /// Delivery thread handle (for cleanup on drop)
    delivery_handle: Option<std::thread::JoinHandle<()>>,
    /// Notify port for waking delivery thread on shutdown
    notify_port: Arc<AtomicU16>,
    /// Current instance name (shared with delivery thread, updated on rebind)
    current_name: Arc<RwLock<String>>,
    /// Current status (shared with delivery thread, updated on status change)
    current_status: Arc<RwLock<String>>,
}

impl Proxy {
    /// Spawn a new PTY process
    pub fn spawn(command: &str, args: &[&str], config: ProxyConfig) -> Result<Self> {
        let winsize = terminal::get_terminal_size()?;
        let pty = openpty(&winsize, None).context("openpty failed")?;

        // Setup raw mode and signal handlers
        let terminal_guard = TerminalGuard::new()?;
        terminal::setup_signal_handlers()?;

        // Spawn child process
        let slave_fd = pty.slave.as_raw_fd();
        let master_fd = pty.master.as_raw_fd();

        // SAFETY: pre_exec closure runs in the child process after fork() but before exec().
        // All operations are async-signal-safe (setsid, ioctl, dup2, close).
        // slave_fd and master_fd are i32 (Copy), captured by value before the OwnedFds are moved.
        let child = unsafe {
            Command::new(command)
                .args(args)
                .pre_exec(move || {
                    // Create new session
                    if libc::setsid() == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    // Set controlling terminal
                    if libc::ioctl(slave_fd, libc::TIOCSCTTY.into(), 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    // Redirect stdio to slave
                    if libc::dup2(slave_fd, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::dup2(slave_fd, 1) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::dup2(slave_fd, 2) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    // Close slave fd if it's not stdio
                    if slave_fd > 2 {
                        libc::close(slave_fd);
                    }
                    // Close master fd — child should only have the slave side.
                    // Without this, the child holds a ref to the PTY master,
                    // preventing proper SIGHUP delivery on PTY teardown.
                    libc::close(master_fd);
                    Ok(())
                })
                .spawn()
                .context("spawn failed")?
        };

        // Write PID and launch context to database for hcom kill
        if let Some(ref instance_name) = config.instance_name {
            if let Ok(db) = crate::db::HcomDb::open() {
                let _ = db.update_instance_pid(instance_name, child.id());

                // Capture minimal launch context early so kill can close the terminal pane.
                // The Python hook may later overwrite with richer context (git_branch, tty, env).
                let _ = db.store_launch_context(
                    instance_name,
                    &build_early_launch_context(),
                );
            }
        }

        // Close slave in parent
        drop(pty.slave);

        // Set master to non-blocking
        set_nonblocking(&pty.master)?;

        // Create screen tracker (with instance name for debug logging)
        let screen = ScreenTracker::new_with_instance(
            winsize.ws_row,
            winsize.ws_col,
            &config.ready_pattern,
            config.instance_name.as_deref(),
        );

        // Start injection server
        let inject_server = InjectServer::new()?;

        // Emit inject port to stderr ONLY when stderr is captured by Python adapter.
        // When running directly in terminal (bash script), stderr is a TTY - skip printing.
        // When running via spawn_native_pty(), stderr is a pipe - print for Python to parse.
        let stderr_is_tty = unsafe { libc::isatty(libc::STDERR_FILENO) == 1 };
        if !stderr_is_tty {
            eprintln!("INJECT_PORT={}", inject_server.port());
        }

        let user_activity_cooldown_ms = 500; // 0.5s for all tools (dim detection enables this for Claude)

        // Initialize shared state for terminal title (updated by delivery thread).
        // Query tag from DB to show full display name (tag-name) from the start.
        let initial_display_name = {
            let base = config.instance_name.clone().unwrap_or_default();
            if base.is_empty() {
                base
            } else if let Ok(db) = crate::db::HcomDb::open() {
                match db.get_instance_tag(&base) {
                    Some(tag) => format!("{}-{}", tag, base),
                    None => base,
                }
            } else {
                base
            }
        };
        let current_name = Arc::new(RwLock::new(initial_display_name));
        let current_status = Arc::new(RwLock::new("listening".to_string()));

        Ok(Self {
            config,
            pty_master: pty.master,
            child,
            _terminal_guard: terminal_guard,
            screen,
            inject_server,
            last_user_input: Instant::now(),
            user_activity_cooldown_ms,
            delivery_state: Arc::new(RwLock::new(ScreenState::default())),
            running: Arc::new(AtomicBool::new(true)),
            last_resize: None,
            delivery_handle: None,
            notify_port: Arc::new(AtomicU16::new(0)),
            current_name,
            current_status,
        })
    }

    /// Run the PTY proxy main loop
    pub fn run(&mut self) -> Result<i32> {
        let stdin_fd = io::stdin();
        let stdout_fd = io::stdout();

        // Check if stdout is a TTY before writing escape sequences
        let stdout_is_tty = unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 };

        let mut buf = [0u8; 65536];
        let mut ready_signaled = false;
        let mut delivery_started = false;
        let startup_time = Instant::now();

        // Track last written title to detect changes (delivery thread updates Arcs)
        let mut last_written_name = String::new();
        let mut last_written_status = String::new();

        // Track incomplete UTF-8 sequences to defer title writes.
        // When PTY output ends with partial multi-byte character, writing our title OSC
        // would corrupt the UTF-8 stream. We defer until sequence completes or timeout.
        let mut pending_utf8: u8 = 0;

        // Track incomplete escape sequences across reads to defer title writes.
        // Typed by escape kind so continuation chunks check the correct terminator.
        let mut pending_escape = PendingEscape::None;

        // Stateful title OSC filter — strips tool's title sequences across read boundaries
        let mut title_filter = TitleOscFilter::new();

        // Title writes deferred to iterations with NO PTY output, preventing interleaving
        // with any incomplete escape sequence (CSI, OSC, UTF-8, etc.).
        let mut had_pty_output: bool;

        // For Claude in accept-edits mode, ready pattern may be hidden.
        // Start delivery after timeout if ready pattern not seen.
        use crate::tool::Tool;
        use std::str::FromStr;

        let delivery_start_timeout = match Tool::from_str(&self.config.tool) {
            Ok(Tool::Claude) | Ok(Tool::Codex) => Duration::from_secs(5), // Ready pattern unreliable (Claude: accept-edits, Codex: narrow terminals)
            _ => Duration::from_secs(60), // Gemini: ready pattern always visible
        };

        loop {
            had_pty_output = false;

            // Handle signals
            if SIGWINCH_RECEIVED.swap(false, Ordering::AcqRel) {
                self.forward_winsize()?;
            }
            if SIGINT_RECEIVED.swap(false, Ordering::AcqRel) {
                self.forward_signal(Signal::SIGINT);
            }
            if SIGTERM_RECEIVED.swap(false, Ordering::AcqRel) {
                self.forward_signal(Signal::SIGTERM);
                EXIT_WAS_KILLED.store(true, Ordering::Release);
                break;
            }
            if SIGHUP_RECEIVED.swap(false, Ordering::AcqRel) {
                // Terminal closed - break to trigger cleanup (Drop runs)
                // Don't forward SIGHUP to child - it will get its own when terminal closes
                EXIT_WAS_KILLED.store(true, Ordering::Release);
                break;
            }

            // Collect raw fds for polling (avoid holding borrows)
            let master_raw = self.pty_master.as_raw_fd();
            let stdin_raw = stdin_fd.as_raw_fd();
            let inject_listener_raw = self.inject_server.listener_raw_fd();

            // Build poll fds from raw values
            let master_fd = unsafe { BorrowedFd::borrow_raw(master_raw) };
            let stdin_borrowed = unsafe { BorrowedFd::borrow_raw(stdin_raw) };
            let inject_listener_fd = unsafe { BorrowedFd::borrow_raw(inject_listener_raw) };

            let mut poll_fds = vec![
                PollFd::new(master_fd, PollFlags::POLLIN),
                PollFd::new(stdin_borrowed, PollFlags::POLLIN),
                PollFd::new(inject_listener_fd, PollFlags::POLLIN),
            ];

            // Add inject client fds
            let client_raw_fds: Vec<i32> = self.inject_server.client_raw_fds().collect();
            for raw_fd in &client_raw_fds {
                let fd = unsafe { BorrowedFd::borrow_raw(*raw_fd) };
                poll_fds.push(PollFd::new(fd, PollFlags::POLLIN));
            }

            // Poll timeout: 5s when debug enabled (for periodic dumps), otherwise block
            // Delivery thread has its own timing via notify.wait(), doesn't need fast polling here
            let poll_timeout = if self.screen.debug_enabled() {
                5000u16  // 5s for debug periodic dumps
            } else {
                10000u16  // 10s, allows runtime debug flag check
            };
            match poll(&mut poll_fds, PollTimeout::from(poll_timeout)) {
                Ok(0) => {
                    // Timeout - still update delivery state for time-based checks
                    if ready_signaled {
                        self.update_delivery_state();
                    }
                    // Start delivery thread on timeout if startup_time exceeded
                    // (child may produce no output after initial render, so the
                    // child-output path at line ~621 may never run)
                    if !delivery_started && startup_time.elapsed() > delivery_start_timeout {
                        self.screen.dump_screen(
                            &self.config.tool,
                            self.inject_server.port(),
                            "Starting delivery thread (poll timeout)",
                        );
                        self.start_delivery_thread()?;
                        delivery_started = true;
                    }
                    // Check runtime debug flag toggle
                    self.screen.check_debug_flag();
                    // Periodic debug dump every 5 seconds
                    self.screen.check_periodic_dump(
                        &self.config.tool,
                        self.inject_server.port(),
                        "Periodic dump (main loop)",
                    );
                    // Detect lost terminal (e.g. terminal window closed, stdin redirected to /dev/null)
                    // SAFETY: stdin_raw is a valid fd obtained from stdin().as_raw_fd() at function start
                    if !nix::unistd::isatty(unsafe { BorrowedFd::borrow_raw(stdin_raw) }).unwrap_or(false) {
                        break;
                    }
                    // Fall through to title write — timeout means no PTY output, safe to write.
                }
                Ok(_) => {}
                Err(Errno::EINTR) => {
                    // Interrupted - still update delivery state
                    if ready_signaled {
                        self.update_delivery_state();
                    }
                    continue;
                }
                Err(e) => bail!("poll failed: {}", e),
            }

            // Handle PTY output
            if let Some(revents) = poll_fds[0].revents() {
                if revents.contains(PollFlags::POLLIN) {
                    match nix_read(&self.pty_master, &mut buf) {
                        Ok(0) => break, // EOF
                        Ok(n) => {
                            let data = &buf[..n];
                            // Strip tool's title OSCs (stateful — handles split sequences)
                            let (filtered, had_title) = if stdout_is_tty {
                                title_filter.filter(data)
                            } else {
                                (data.to_vec(), false)
                            };
                            write_all(&stdout_fd, &filtered)?;
                            // Track if we wrote any real output this iteration.
                            // Title write deferred to iterations with no PTY output.
                            if !filtered.is_empty() {
                                had_pty_output = true;
                                pending_utf8 = pending_utf8_bytes(&filtered);
                                // Track escape state across reads. If chunk has ESC,
                                // scan from the new ESC. If no ESC, check if the
                                // continuation resolves a previously pending escape.
                                pending_escape = if filtered.iter().any(|&b| b == 0x1b) {
                                    has_pending_escape(&filtered)
                                } else {
                                    resolve_pending_escape(pending_escape, &filtered)
                                };
                            }
                            // If tool tried to set title, ensure we write ours at end-of-loop
                            if had_title {
                                last_written_name.clear();
                            }
                            // Update screen tracker (use original data for pattern detection)
                            self.screen.process(data);
                            // Update delivery state
                            self.update_delivery_state();
                            // Check for ready pattern
                            if !ready_signaled && self.screen.is_ready() {
                                // Note: Don't print READY to stdout - it pollutes tool output
                                // Instead, just start the delivery thread
                                ready_signaled = true;
                                self.screen.dump_screen(
                                    &self.config.tool,
                                    self.inject_server.port(),
                                    "Ready pattern detected",
                                );
                            }

                            // Start delivery thread when ready OR after timeout
                            // (Claude in accept-edits mode may never show ready pattern)
                            if !delivery_started {
                                let should_start = ready_signaled
                                    || startup_time.elapsed() > delivery_start_timeout;
                                if should_start {
                                    self.screen.dump_screen(
                                        &self.config.tool,
                                        self.inject_server.port(),
                                        "Starting delivery thread",
                                    );
                                    // Propagate delivery thread init errors (CLI domain: proper exit codes)
                                    self.start_delivery_thread()?;
                                    delivery_started = true;
                                }
                            }
                        }
                        Err(Errno::EAGAIN) => {}
                        Err(Errno::EIO) => break,
                        Err(e) => bail!("read from pty failed: {}", e),
                    }
                }
                if revents.contains(PollFlags::POLLHUP) {
                    break;
                }
            }

            // Handle stdin
            if let Some(revents) = poll_fds[1].revents() {
                if revents.contains(PollFlags::POLLHUP) {
                    // Terminal disconnected - exit cleanly
                    break;
                }
                if revents.contains(PollFlags::POLLIN) {
                    match nix_read(&stdin_fd, &mut buf) {
                        Ok(0) => break, // stdin EOF = terminal gone, exit cleanly
                        Ok(n) => {
                            self.last_user_input = Instant::now();
                            self.screen.clear_approval();
                            // Update delivery state for user activity
                            if let Ok(mut state) = self.delivery_state.write() {
                                state.last_user_input = Instant::now();
                                state.approval = false;
                            }
                            write_all(&self.pty_master, &buf[..n])?;
                        }
                        Err(Errno::EAGAIN) => {}
                        Err(e) => bail!("read from stdin failed: {}", e),
                    }
                }
            }

            // Handle inject server accept
            if let Some(revents) = poll_fds[2].revents() {
                if revents.contains(PollFlags::POLLIN) {
                    self.inject_server.accept()?;
                }
            }

            // Handle inject client data (process in reverse to handle removals)
            for i in (0..client_raw_fds.len()).rev() {
                let poll_idx = 3 + i;
                if let Some(revents) = poll_fds[poll_idx].revents() {
                    if revents.contains(PollFlags::POLLIN) || revents.contains(PollFlags::POLLHUP) {
                        match self.inject_server.read_client(i)? {
                            inject::InjectResult::Inject(text) => {
                                write_all(&self.pty_master, text.as_bytes())?;
                            }
                            inject::InjectResult::Query(client) => {
                                match client.command {
                                    inject::QueryCommand::Screen => {
                                        let dump = self.screen.get_screen_dump(
                                            &self.config.tool,
                                            self.inject_server.port(),
                                        );
                                        client.respond(&dump);
                                    }
                                    inject::QueryCommand::Unknown => {
                                        client.respond("error: unknown command\n");
                                    }
                                }
                            }
                            inject::InjectResult::Pending => {}
                        }
                    }
                }
            }

            // Check for title changes (delivery thread updates shared Arcs)
            // Writing here ensures title OSC is serialized with PTY output, preventing interleaving
            //
            // Only write title when this iteration had NO PTY output. This prevents
            // interleaving with any incomplete escape sequence (CSI, UTF-8, etc.).
            // pending_utf8 catches cross-iteration incomplete UTF-8 (e.g., title-only
            // read after a read that ended with partial multi-byte char).
            if stdout_is_tty && title_write_safe(had_pty_output, pending_utf8, pending_escape) {
                let (name, status) = {
                    let n = self.current_name.read().ok().map(|n| n.clone()).unwrap_or_default();
                    let s = self.current_status.read().ok().map(|s| s.clone()).unwrap_or_default();
                    (n, s)
                };
                if !name.is_empty() && (name != last_written_name || status != last_written_status) {
                    let icon = status_icon(&status);
                    let tool_upper = self.config.tool.to_uppercase();
                    let title = format!("{} {} [{}]", icon, name, tool_upper);
                    let escape = format!("\x1b]1;{}\x07\x1b]2;{}\x07", title, title);
                    write_all(&stdout_fd, escape.as_bytes())?;
                    last_written_name = name;
                    last_written_status = status;
                }
            }
        }

        // Flush any held prefix bytes from title filter
        if stdout_is_tty {
            let remaining = title_filter.flush();
            if !remaining.is_empty() {
                let _ = write_all(&stdout_fd, &remaining);
            }
        }

        // Stop delivery thread
        self.running.store(false, Ordering::Release);

        // Kill child process group (child is session leader via setsid(), so PID = PGID)
        // This ensures claude and all its children are killed, not just the launch script
        let pgid = Pid::from_raw(-(self.child.id() as i32));
        let _ = kill(pgid, Signal::SIGTERM);

        self.drain_and_wait_child()
    }

    fn forward_winsize(&mut self) -> Result<()> {
        // Fix #3: Debounce resize signals by 50ms to avoid races during rapid resize
        const RESIZE_DEBOUNCE_MS: u64 = 50;
        if let Some(last) = self.last_resize {
            if last.elapsed().as_millis() < RESIZE_DEBOUNCE_MS as u128 {
                return Ok(()); // Skip if too recent
            }
        }
        self.last_resize = Some(Instant::now());

        if let Ok(winsize) = terminal::get_terminal_size() {
            self.screen.resize(winsize.ws_row, winsize.ws_col);

            // SAFETY:
            // - self.pty_master is an OwnedFd, valid for the lifetime of Proxy
            // - winsize comes from get_terminal_size() which validates the struct and falls back to 80x24 on error
            // - TIOCSWINSZ is the correct ioctl request for setting terminal window size on the PTY
            // - Return value is intentionally ignored: terminal resize is best-effort; failure is non-fatal
            //   and doesn't affect correctness (child process continues with old size)
            unsafe {
                libc::ioctl(
                    self.pty_master.as_raw_fd(),
                    libc::TIOCSWINSZ.into(),
                    &winsize,
                );
            }
        }
        Ok(())
    }

    fn forward_signal(&self, signal: Signal) {
        // Kill process group (negative PID) since child is session leader via setsid()
        // This ensures claude and all its children are killed, not just the launch script
        let pgid = Pid::from_raw(-(self.child.id() as i32));
        let _ = kill(pgid, signal);
    }

    /// Wait for child to exit while draining PTY master to prevent deadlock.
    ///
    /// After the main loop breaks, the child may still be writing output during
    /// shutdown. If nobody reads the PTY master, the kernel buffer fills and the
    /// child blocks on write() — deadlocking with our waitpid(). We drain the
    /// master in a poll loop with non-blocking try_wait, escalating to SIGKILL
    /// after a timeout.
    fn drain_and_wait_child(&mut self) -> Result<i32> {
        let mut buf = [0u8; 65536];
        let deadline = Instant::now() + Duration::from_secs(5);

        loop {
            // Non-blocking child check
            match self.child.try_wait() {
                Ok(Some(status)) => return Ok(exit_code_from_status(status)),
                Ok(None) => {} // Still running
                Err(e) => bail!("wait failed: {}", e),
            }

            // Timeout — escalate to SIGKILL
            if Instant::now() > deadline {
                let pgid = Pid::from_raw(-(self.child.id() as i32));
                let _ = kill(pgid, Signal::SIGKILL);
                // Wait up to 2s for process to die after SIGKILL
                let kill_deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < kill_deadline {
                    match self.child.try_wait() {
                        Ok(Some(status)) => return Ok(exit_code_from_status(status)),
                        Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                        Err(e) => bail!("wait after SIGKILL failed: {}", e),
                    }
                }
                // Process stuck in uninterruptible state — give up
                return Ok(1);
            }

            // Drain PTY master (non-blocking, discard output)
            match nix_read(&self.pty_master, &mut buf) {
                Ok(0) => {
                    // EOF — child closed its side, do blocking wait
                    match self.child.wait() {
                        Ok(status) => return Ok(exit_code_from_status(status)),
                        Err(e) => bail!("wait failed: {}", e),
                    }
                }
                Ok(_) => {} // Drained some data, loop again
                Err(Errno::EAGAIN) => {
                    // Nothing to read — sleep briefly before next try_wait
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(Errno::EIO) => {
                    // PTY gone — child side closed, do blocking wait
                    match self.child.wait() {
                        Ok(status) => return Ok(exit_code_from_status(status)),
                        Err(e) => bail!("wait failed: {}", e),
                    }
                }
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    /// Update shared delivery state from screen tracker
    fn update_delivery_state(&self) {
        if let Ok(mut state) = self.delivery_state.write() {
            state.ready = self.screen.is_ready();
            state.approval = self.screen.is_waiting_approval();
            state.prompt_empty = self.screen.is_prompt_empty(&self.config.tool);
            state.input_text = self.screen.get_input_box_text(&self.config.tool);
            state.last_output = self.screen.last_output_instant();
            state.cols = self.screen.cols();
        }
    }

    /// Start the delivery thread (and transcript watcher for Codex)
    ///
    /// Returns Ok(()) if delivery thread initialized successfully (DB opened, notify server created).
    /// Returns Err if initialization failed.
    fn start_delivery_thread(&mut self) -> Result<()> {
        let instance_name = match &self.config.instance_name {
            Some(name) => name.clone(),
            None => {
                // Try to get from environment (fallback for testing without explicit config)
                Config::get().instance_name.unwrap_or_default()
            }
        };

        if instance_name.is_empty() {
            // No instance name - skip delivery (hybrid mode or testing)
            crate::log::log_warn("native", "delivery.skip.no_instance_name",
                "No instance name - delivery disabled. Set config.instance_name or HCOM_INSTANCE_NAME env var.");
            return Ok(());
        }

        // Create oneshot channel for init result
        let (init_tx, init_rx) = mpsc::channel();

        let running = self.running.clone();
        let delivery_state = self.delivery_state.clone();
        let inject_port = self.inject_server.port();
        let tool = self.config.tool.clone();
        let user_activity_cooldown_ms = self.user_activity_cooldown_ms;
        let notify_port_shared = self.notify_port.clone();
        let shared_name = self.current_name.clone();
        let shared_status = self.current_status.clone();

        // For Codex: spawn transcript watcher thread
        use crate::tool::Tool;
        use std::str::FromStr;

        if let Ok(Tool::Codex) = Tool::from_str(&tool) {
            let watcher_running = self.running.clone();
            let watcher_name = instance_name.clone();
            std::thread::spawn(move || {
                crate::transcript::run_transcript_watcher(
                    watcher_running,
                    watcher_name,
                    Duration::from_secs(5),
                );
            });
        }

        let handle = std::thread::spawn(move || {
            log_info("native", "delivery.start", &format!("Starting delivery thread for {}", instance_name));

            // Initialize delivery components with dependency injection
            let (db, notify) = match initialize_delivery_components(
                &instance_name,
                HcomDb::open,
                NotifyServer::new,
            ) {
                Ok((db, notify)) => {
                    log_info("native", "delivery.init.success", &format!("Initialized delivery for {}", instance_name));
                    // Store port for shutdown wakeup
                    notify_port_shared.store(notify.port(), Ordering::Release);
                    log_info("native", "notify.registered", &format!("Registered notify port {}", notify.port()));
                    // Register inject port for screen queries
                    if let Err(e) = db.register_inject_port(&instance_name, inject_port) {
                        log_warn("native", "inject.register_fail", &format!("Failed to register inject port: {}", e));
                    }

                    // Signal successful initialization to parent
                    let _ = init_tx.send(Ok(()));
                    (db, notify)
                }
                Err(e) => {
                    log_error("native", "delivery.init.fail", &format!("Failed to initialize delivery: {}", e));
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            // Create delivery state wrapper
            let state = DeliveryState {
                screen: delivery_state,
                inject_port,
                user_activity_cooldown_ms,
            };

            // Get tool config
            let config = ToolConfig::for_tool(&tool);

            // Run delivery loop (pass shared state for main loop's OSC override)
            run_delivery_loop(running, &db, &notify, &state, &instance_name, &config, Some(shared_name), Some(shared_status));

            log_info("native", "delivery.stop", &format!("Delivery thread stopped for {}", instance_name));
        });

        self.delivery_handle = Some(handle);

        // Wait for initialization result (with timeout to avoid blocking forever)
        match init_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {
                log_info("native", "delivery.init.success", "Delivery thread initialized successfully");
                Ok(())
            }
            Ok(Err(e)) => {
                log_error("native", "delivery.init.fail", &format!("Delivery thread init failed: {}", e));
                Err(e)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                log_error("native", "delivery.init.timeout", "Delivery thread init timed out after 5s");
                bail!("Delivery thread initialization timed out")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                log_error("native", "delivery.init.disconnect", "Delivery thread init channel disconnected");
                bail!("Delivery thread initialization channel disconnected")
            }
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        use crate::log::log_info;

        // Signal delivery thread to stop
        self.running.store(false, Ordering::Release);

        // Wake delivery thread if it's blocked in notify.wait()
        let port = self.notify_port.load(Ordering::Acquire);
        log_info("native", "proxy.drop.wake", &format!("Waking notify port {}", port));
        if port != 0 {
            // Connect briefly to wake the notify server's poll()
            match std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                std::time::Duration::from_millis(100),
            ) {
                Ok(_) => log_info("native", "proxy.drop.wake_ok", "Connected to notify port"),
                Err(e) => log_info("native", "proxy.drop.wake_fail", &format!("Failed to connect: {}", e)),
            }
        }

        // Wait for delivery thread to finish cleanup
        if let Some(handle) = self.delivery_handle.take() {
            // Give thread up to 5 seconds to finish cleanup
            let timeout = std::time::Duration::from_secs(5);
            let start = std::time::Instant::now();

            // Busy-wait with timeout (JoinHandle doesn't have timeout join)
            loop {
                if handle.is_finished() {
                    let _ = handle.join();
                    break;
                }
                if start.elapsed() > timeout {
                    crate::log::log_warn("native", "delivery.join_timeout", "Delivery thread did not finish in time");
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

fn exit_code_from_status(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(signal) = status.signal() {
        128 + signal
    } else {
        1
    }
}

fn set_nonblocking<Fd: AsFd>(fd: &Fd) -> Result<()> {
    let flags = fcntl(fd.as_fd(), FcntlArg::F_GETFL).context("fcntl F_GETFL failed")?;
    let flags = OFlag::from_bits_truncate(flags);
    fcntl(fd.as_fd(), FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
        .context("fcntl F_SETFL failed")?;
    Ok(())
}

fn write_all<F: AsFd>(fd: &F, data: &[u8]) -> Result<()> {
    let mut written = 0;
    while written < data.len() {
        match write(fd, &data[written..]) {
            Ok(n) => written += n,
            Err(Errno::EAGAIN) | Err(Errno::EINTR) => continue,
            Err(e) => bail!("write failed: {}", e),
        }
    }
    Ok(())
}

fn nix_read<F: AsFd>(fd: &F, buf: &mut [u8]) -> Result<usize, Errno> {
    read(fd.as_fd(), buf)
}

/// Initialize delivery components with dependency injection for testing
///
/// Returns (db, notify) on success, Err on failure
fn initialize_delivery_components<DbF, NotifyF>(
    instance_name: &str,
    db_factory: DbF,
    notify_factory: NotifyF,
) -> Result<(crate::db::HcomDb, crate::notify::NotifyServer)>
where
    DbF: FnOnce() -> Result<crate::db::HcomDb>,
    NotifyF: FnOnce() -> Result<crate::notify::NotifyServer>,
{
    // Open database
    let db = db_factory()
        .context("Failed to open database")?;

    // Create notify server
    let notify = notify_factory()
        .context("Failed to create notify server")?;

    // Register notify port
    db.register_notify_port(instance_name, notify.port())
        .context("Failed to register notify port")?;

    Ok((db, notify))
}

#[cfg(test)]
mod tests {
    use anyhow::{anyhow, Context, Result};

    // Test helper that mirrors initialize_delivery_components logic but uses generic types
    fn test_init_with_factories<T, U, DbF, NotifyF>(
        db_factory: DbF,
        notify_factory: NotifyF,
    ) -> Result<(T, U)>
    where
        DbF: FnOnce() -> Result<T>,
        NotifyF: FnOnce() -> Result<U>,
    {
        let db = db_factory().context("Failed to open database")?;
        let notify = notify_factory().context("Failed to create notify server")?;
        Ok((db, notify))
    }

    #[test]
    fn test_initialize_delivery_components_db_failure_propagates() {
        // Verify that DB factory errors are propagated with proper context

        let db_factory = || Err(anyhow!("DB connection refused"));
        let notify_factory = || {
            panic!("NotifyServer factory should not be called when DB fails");
        };

        let result: Result<(i32, i32)> = test_init_with_factories(
            db_factory,
            notify_factory,
        );

        assert!(result.is_err(), "Should return Err when DB open fails");
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(err_msg.contains("Failed to open database"), "Error should have context");
    }

    #[test]
    fn test_initialize_delivery_components_notify_failure_propagates() {
        // Verify that NotifyServer factory errors are propagated with proper context

        let db_factory = || Ok(42); // DB succeeds
        let notify_factory = || Err(anyhow!("Port already in use"));

        let result: Result<(i32, i32)> = test_init_with_factories(
            db_factory,
            notify_factory,
        );

        assert!(result.is_err(), "Should return Err when NotifyServer creation fails");
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(err_msg.contains("Failed to create notify server"), "Error should have context");
    }

    #[test]
    fn test_initialize_delivery_components_success_path() {
        // Verify that both factories succeeding returns Ok

        let db_factory = || Ok(42);
        let notify_factory = || Ok(100);

        let result: Result<(i32, i32)> = test_init_with_factories(
            db_factory,
            notify_factory,
        );

        assert!(result.is_ok(), "Should return Ok when both succeed");
        let (db, notify) = result.unwrap();
        assert_eq!(db, 42);
        assert_eq!(notify, 100);
    }

    #[test]
    fn test_initialize_delivery_components_db_error_short_circuits() {
        // Verify that DB failure prevents notify factory from being called (? operator short-circuits)

        let mut notify_called = false;
        let db_factory = || Err(anyhow!("DB error"));
        let notify_factory = || {
            notify_called = true;
            Ok(100)
        };

        let result: Result<(i32, i32)> = test_init_with_factories(
            db_factory,
            notify_factory,
        );

        assert!(result.is_err(), "Should propagate DB error");
        assert!(!notify_called, "Notify factory should not be called when DB fails (? short-circuits)");
    }

    // ---- pending_utf8_bytes tests ----

    use super::pending_utf8_bytes;

    #[test]
    fn test_pending_utf8_empty() {
        assert_eq!(pending_utf8_bytes(&[]), 0);
    }

    #[test]
    fn test_pending_utf8_ascii_complete() {
        // ASCII text is always complete
        assert_eq!(pending_utf8_bytes(b"Hello world"), 0);
        assert_eq!(pending_utf8_bytes(b"x"), 0);
    }

    #[test]
    fn test_pending_utf8_complete_2byte() {
        // é (U+00E9) = C3 A9 (complete 2-byte)
        assert_eq!(pending_utf8_bytes(&[0xC3, 0xA9]), 0);
    }

    #[test]
    fn test_pending_utf8_incomplete_2byte() {
        // Leading byte of 2-byte sequence without continuation
        assert_eq!(pending_utf8_bytes(&[0xC3]), 1);
    }

    #[test]
    fn test_pending_utf8_complete_3byte() {
        // ─ (U+2500) = E2 94 80 (complete 3-byte)
        assert_eq!(pending_utf8_bytes(&[0xE2, 0x94, 0x80]), 0);
    }

    #[test]
    fn test_pending_utf8_incomplete_3byte_needs_2() {
        // E2 alone needs 2 more bytes
        assert_eq!(pending_utf8_bytes(&[0xE2]), 2);
    }

    #[test]
    fn test_pending_utf8_incomplete_3byte_needs_1() {
        // E2 94 needs 1 more byte
        assert_eq!(pending_utf8_bytes(&[0xE2, 0x94]), 1);
    }

    #[test]
    fn test_pending_utf8_complete_4byte() {
        // 😀 (U+1F600) = F0 9F 98 80 (complete 4-byte)
        assert_eq!(pending_utf8_bytes(&[0xF0, 0x9F, 0x98, 0x80]), 0);
    }

    #[test]
    fn test_pending_utf8_incomplete_4byte_needs_3() {
        // F0 alone needs 3 more bytes
        assert_eq!(pending_utf8_bytes(&[0xF0]), 3);
    }

    #[test]
    fn test_pending_utf8_incomplete_4byte_needs_2() {
        // F0 9F needs 2 more bytes
        assert_eq!(pending_utf8_bytes(&[0xF0, 0x9F]), 2);
    }

    #[test]
    fn test_pending_utf8_incomplete_4byte_needs_1() {
        // F0 9F 98 needs 1 more byte
        assert_eq!(pending_utf8_bytes(&[0xF0, 0x9F, 0x98]), 1);
    }

    #[test]
    fn test_pending_utf8_mixed_content_complete() {
        // "text─more" = complete (box drawing char is complete)
        let data = b"text\xe2\x94\x80more";
        assert_eq!(pending_utf8_bytes(data), 0);
    }

    #[test]
    fn test_pending_utf8_mixed_content_incomplete() {
        // "text" + first 2 bytes of ─
        let data = b"text\xe2\x94";
        assert_eq!(pending_utf8_bytes(data), 1);
    }

    #[test]
    fn test_pending_utf8_line_of_box_drawing_incomplete() {
        // Multiple complete ─ chars followed by incomplete start
        // ─────\xe2 (5 complete + 1 incomplete start)
        let mut data = Vec::new();
        for _ in 0..5 {
            data.extend_from_slice(&[0xE2, 0x94, 0x80]); // ─
        }
        data.push(0xE2); // Start of next ─
        assert_eq!(pending_utf8_bytes(&data), 2);
    }

    // ---- title_write_safe tests ----

    use super::{PendingEscape, title_write_safe, has_pending_escape, resolve_pending_escape};

    #[test]
    fn test_title_write_blocked_by_pty_output() {
        assert!(!title_write_safe(true, 0, PendingEscape::None));
    }

    #[test]
    fn test_title_write_blocked_by_pending_utf8() {
        assert!(!title_write_safe(false, 1, PendingEscape::None));
    }

    #[test]
    fn test_title_write_blocked_by_pending_csi() {
        assert!(!title_write_safe(false, 0, PendingEscape::Csi));
    }

    #[test]
    fn test_title_write_blocked_by_pending_string_seq() {
        assert!(!title_write_safe(false, 0, PendingEscape::StringSeq));
    }

    #[test]
    fn test_title_write_safe_when_all_clear() {
        assert!(title_write_safe(false, 0, PendingEscape::None));
    }

    #[test]
    fn test_title_write_blocked_by_multiple_conditions() {
        assert!(!title_write_safe(true, 2, PendingEscape::Csi));
    }

    // ---- has_pending_escape tests ----

    #[test]
    fn test_pending_escape_empty() {
        assert_eq!(has_pending_escape(&[]), PendingEscape::None);
    }

    #[test]
    fn test_pending_escape_plain_text() {
        assert_eq!(has_pending_escape(b"Hello world"), PendingEscape::None);
    }

    #[test]
    fn test_pending_escape_complete_csi() {
        assert_eq!(has_pending_escape(b"\x1b[38;2;100m"), PendingEscape::None);
    }

    #[test]
    fn test_pending_escape_incomplete_csi() {
        assert_eq!(has_pending_escape(b"\x1b[38;2;"), PendingEscape::Csi);
    }

    #[test]
    fn test_pending_escape_bare_esc() {
        assert_eq!(has_pending_escape(b"text\x1b"), PendingEscape::Csi);
    }

    #[test]
    fn test_pending_escape_complete_osc_bel() {
        assert_eq!(has_pending_escape(b"\x1b]8;id=link;https://example.com\x07"), PendingEscape::None);
    }

    #[test]
    fn test_pending_escape_incomplete_osc() {
        assert_eq!(has_pending_escape(b"\x1b]8;id=link;https://example.com"), PendingEscape::StringSeq);
    }

    #[test]
    fn test_pending_escape_complete_osc_st() {
        assert_eq!(has_pending_escape(b"\x1b]8;id=link;https://example.com\x1b\\"), PendingEscape::None);
    }

    #[test]
    fn test_pending_escape_simple_two_byte() {
        assert_eq!(has_pending_escape(b"\x1bM"), PendingEscape::None);
    }

    #[test]
    fn test_pending_escape_after_complete_sequence() {
        assert_eq!(has_pending_escape(b"\x1b[38;2;100mhello"), PendingEscape::None);
    }

    #[test]
    fn test_pending_escape_incomplete_dcs() {
        assert_eq!(has_pending_escape(b"\x1bPsome data"), PendingEscape::StringSeq);
    }

    #[test]
    fn test_pending_escape_complete_dcs() {
        assert_eq!(has_pending_escape(b"\x1bPsome data\x1b\\"), PendingEscape::None);
    }

    // ---- resolve_pending_escape (cross-chunk) tests ----

    #[test]
    fn test_resolve_csi_continuation_no_final() {
        // CSI params without final byte — stays pending
        assert_eq!(resolve_pending_escape(PendingEscape::Csi, b"100;50;"), PendingEscape::Csi);
    }

    #[test]
    fn test_resolve_csi_continuation_with_final() {
        // CSI terminated by 'm' (0x6D)
        assert_eq!(resolve_pending_escape(PendingEscape::Csi, b"200m"), PendingEscape::None);
    }

    #[test]
    fn test_resolve_csi_continuation_final_mid_chunk() {
        // Final byte followed by normal text
        assert_eq!(resolve_pending_escape(PendingEscape::Csi, b"200mHello world"), PendingEscape::None);
    }

    #[test]
    fn test_resolve_string_seq_continuation_no_terminator() {
        // OSC URL continuation without BEL — stays pending
        assert_eq!(resolve_pending_escape(PendingEscape::StringSeq, b"ample.com/path"), PendingEscape::StringSeq);
    }

    #[test]
    fn test_resolve_string_seq_continuation_with_bel() {
        // OSC terminated by BEL
        assert_eq!(resolve_pending_escape(PendingEscape::StringSeq, b"url\x07rest"), PendingEscape::None);
    }

    #[test]
    fn test_resolve_none_stays_none() {
        assert_eq!(resolve_pending_escape(PendingEscape::None, b"any data"), PendingEscape::None);
    }

    #[test]
    fn test_resolve_string_seq_letters_dont_clear() {
        // Letters in OSC content (e.g., URL) must NOT clear StringSeq —
        // only BEL or ST terminates. (Letters would falsely clear CSI.)
        assert_eq!(resolve_pending_escape(PendingEscape::StringSeq, b"https://example"), PendingEscape::StringSeq);
    }

    #[test]
    fn test_three_way_csi_split() {
        // Simulate the exact 3-way split bug: ESC[38;2; | 100;50; | 200m
        let chunk1 = b"\x1b[38;2;";
        let chunk2 = b"100;50;";
        let chunk3 = b"200m";

        let state = has_pending_escape(chunk1);
        assert_eq!(state, PendingEscape::Csi);

        // Chunk 2 has no ESC — use resolve
        let state = resolve_pending_escape(state, chunk2);
        assert_eq!(state, PendingEscape::Csi, "must stay pending through middle chunk");

        // Chunk 3 has no ESC — use resolve, 'm' terminates
        let state = resolve_pending_escape(state, chunk3);
        assert_eq!(state, PendingEscape::None);
    }

    #[test]
    fn test_three_way_osc_split() {
        // OSC 8 hyperlink split: ESC]8;id=x; | https://long.url | .com/path BEL
        let chunk1 = b"\x1b]8;id=x;";
        let chunk2 = b"https://long.url";
        let chunk3 = b".com/path\x07";

        let state = has_pending_escape(chunk1);
        assert_eq!(state, PendingEscape::StringSeq);

        let state = resolve_pending_escape(state, chunk2);
        assert_eq!(state, PendingEscape::StringSeq, "URL letters must not terminate OSC");

        let state = resolve_pending_escape(state, chunk3);
        assert_eq!(state, PendingEscape::None);
    }
}
