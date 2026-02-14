//! Protocol types and request/response handling for daemon client.
//!
//! Defines wire format (Request/Response), error types, and send/receive logic.

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, ErrorKind, IsTerminal, Read, Write};
use std::os::fd::BorrowedFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::log::log_info;

/// Hook types that read from stdin (Claude/Gemini hooks)
pub const STDIN_HOOKS: &[&str] = &[
    "poll", "notify", "pre", "post", "sessionstart",
    "userpromptsubmit", "sessionend", "subagent-start", "subagent-stop",
    "gemini-sessionstart", "gemini-beforeagent", "gemini-afteragent",
    "gemini-beforetool", "gemini-aftertool", "gemini-notification", "gemini-sessionend",
];

/// Hook types that read from argv (Codex hooks)
pub const ARGV_HOOKS: &[&str] = &["codex-notify"];

/// Blocking hooks need longer timeout (Stop hook blocks 30s)
/// Keep in sync with Python hook handlers that block (poll_messages in family.py)
pub const BLOCKING_HOOKS: &[&str] = &["poll", "subagent-stop"];

/// Tools that support launch commands (hcom [N] <tool>)
pub const LAUNCH_TOOLS: &[&str] = &["claude", "codex", "gemini", "f", "r"];

/// Environment variables to forward to daemon
const FORWARD_ENV: &[&str] = &[
    // Identity and mode
    "HCOM_PROCESS_ID", "HCOM_LAUNCHED", "HCOM_PTY_MODE", "HCOM_BACKGROUND", "HCOM_IS_FORK",
    // Launch context
    "HCOM_LAUNCHED_BY", "HCOM_LAUNCH_BATCH_ID", "HCOM_LAUNCH_EVENT_ID",
    "HCOM_LAUNCHED_PRESET",
    // Paths and config
    "CLAUDE_ENV_FILE", "HCOM_DIR", "HCOM_GO", "HCOM_NOTES",
    // Tool detection (keep in sync with TOOL_MARKER_VARS in shared.py)
    "CLAUDECODE", "GEMINI_CLI", "GEMINI_SYSTEM_MD",
    "CODEX_SANDBOX", "CODEX_SANDBOX_NETWORK_DISABLED",
    "CODEX_MANAGED_BY_NPM", "CODEX_MANAGED_BY_BUN",
    "CODEX_THREAD_ID",
];

pub const PROTOCOL_VERSION: u32 = 1;

/// Parse listen timeout from argv: "listen [N]" or "listen --timeout N".
/// Default: 86400s (24h).
pub(super) fn parse_listen_timeout(argv: &[String]) -> u64 {
    argv.iter()
        .position(|s| s == "--timeout")
        .and_then(|i| argv.get(i + 1))
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| argv.get(1).and_then(|s| s.parse::<u64>().ok()))
        .unwrap_or(86400)
}

// Read/write timeouts
const WRITE_TIMEOUT_SECS: u64 = 5;
const DEFAULT_READ_TIMEOUT_SECS: u64 = 30;
const LISTEN_TIMEOUT_BUFFER_SECS: u64 = 5;
const EVENTS_LAUNCH_TIMEOUT_SECS: u64 = 65; // 60s wait + 5s buffer
const TRANSCRIPT_SEARCH_TIMEOUT_SECS: u64 = 60;
const ARCHIVE_QUERY_TIMEOUT_SECS: u64 = 15;

/// Flags that take a value for `hcom send` (skip next arg when scanning).
const SEND_FLAGS_WITH_VALUES: &[&str] = &[
    "--name",
    "--from",
    "--intent",
    "--thread",
    "--reply-to",
    "--title",
    "--description",
    "--events",
    "--files",
    "--transcript",
    "--extends",
    "--file",
    "--base64",
];

/// Request kind discriminator.
///
/// Serializes to "hook" or "cli" to match Python daemon's expected wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestKind {
    /// Hook invocation (Claude/Gemini/Codex hooks read from stdin or argv)
    Hook,
    /// CLI command (hcom send, hcom list, etc.)
    Cli,
}

impl std::fmt::Display for RequestKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestKind::Hook => write!(f, "hook"),
            RequestKind::Cli => write!(f, "cli"),
        }
    }
}

#[derive(Serialize)]
pub struct Request {
    pub version: u32,
    pub request_id: String,
    pub kind: RequestKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    pub env: HashMap<String, String>,
    pub cwd: String,
    /// True if client's stdin is a TTY (for interactive behavior)
    pub stdin_is_tty: bool,
    /// True if client's stdout is a TTY (for interactive behavior)
    pub stdout_is_tty: bool,
}

#[derive(Deserialize)]
pub struct Response {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Error types for daemon communication
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// Connection failed - safe to fallback to Python
    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    /// Permission denied on socket connect (e.g. Codex sandbox blocks Unix sockets).
    /// Safe to fallback to Python. Must NOT spawn a new daemon (would kill the healthy one).
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// Request sent but read timed out - NOT safe to fallback (may cause double execution)
    #[error("read timeout: {0}")]
    ReadTimeout(String),

    /// I/O error with source
    #[error("io error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },

    /// JSON serialization/deserialization error with source
    #[error("json error: {source}")]
    Json {
        #[from]
        source: serde_json::Error,
    },

    /// Other error during communication
    #[error("other error: {0}")]
    Other(String),
}

/// Build JSON request from arguments.
pub fn build_request(args: &[String]) -> Request {
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("");
    let request_id = generate_request_id();
    let stdin_is_tty = std::io::stdin().is_terminal();
    let stdout_is_tty = std::io::stdout().is_terminal();

    let mut env = HashMap::new();
    for key in FORWARD_ENV {
        if let Ok(val) = std::env::var(key) {
            env.insert(key.to_string(), val);
        }
    }

    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "/".to_string());

    if STDIN_HOOKS.contains(&cmd) {
        // Hook that reads from stdin
        let mut stdin_content = String::new();
        let _ = std::io::stdin().read_to_string(&mut stdin_content);
        Request {
            version: PROTOCOL_VERSION,
            request_id,
            kind: RequestKind::Hook,
            hook_type: Some(cmd.to_string()),
            stdin: Some(stdin_content),
            argv: None,
            env,
            cwd,
            stdin_is_tty,
            stdout_is_tty,
        }
    } else if ARGV_HOOKS.contains(&cmd) {
        // Hook that reads from argv (Codex)
        Request {
            version: PROTOCOL_VERSION,
            request_id,
            kind: RequestKind::Hook,
            hook_type: Some(cmd.to_string()),
            stdin: None,
            argv: Some(args.to_vec()),
            env,
            cwd,
            stdin_is_tty,
            stdout_is_tty,
        }
    } else {
        // CLI command - forward stdin if piped and data is available.
        // Uses poll() + non-blocking read to avoid indefinite blocking when
        // stdin is a pipe that never closes (e.g., Bash tool in Claude Code).
        let stdin_content = if !stdin_is_tty && cli_wants_stdin(args) {
            read_stdin_nonblocking(500)
        } else {
            None
        };
        Request {
            version: PROTOCOL_VERSION,
            request_id,
            kind: RequestKind::Cli,
            hook_type: None,
            stdin: stdin_content,
            argv: Some(args.to_vec()),
            env,
            cwd,
            stdin_is_tty,
            stdout_is_tty,
        }
    }
}

/// Send request and receive response.
/// Returns specific error types to distinguish connection failures from read timeouts.
pub fn try_send(stream: &UnixStream, request: &Request) -> std::result::Result<Response, DaemonError> {
    let total_start = Instant::now();
    let request_id = request.request_id.as_str();

    let timeout = get_read_timeout(request);
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(Duration::from_secs(WRITE_TIMEOUT_SECS))).ok();

    let mut stream = stream;

    // Serialize
    let serialize_start = Instant::now();
    let json = serde_json::to_string(request)?;
    let serialize_ms = serialize_start.elapsed().as_secs_f64() * 1000.0;

    // Write request - if this fails, request wasn't sent, safe to fallback
    let write_start = Instant::now();
    if let Err(e) = stream.write_all(json.as_bytes()) {
        return Err(DaemonError::ConnectionFailed(format!("Failed to write: {}", e)));
    }
    if let Err(e) = stream.write_all(b"\n") {
        return Err(DaemonError::ConnectionFailed(format!("Failed to write newline: {}", e)));
    }
    stream.shutdown(std::net::Shutdown::Write).ok();
    let write_ms = write_start.elapsed().as_secs_f64() * 1000.0;

    // Read response - if this times out, request WAS sent, NOT safe to fallback
    let read_start = Instant::now();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => {
            // EOF - daemon closed connection (likely died or restarted)
            let read_ms = read_start.elapsed().as_secs_f64() * 1000.0;
            log_info("client", "try_send.eof", &format!(
                "request_id={} serialize={:.1}ms write={:.1}ms read={:.1}ms (EOF)",
                request_id, serialize_ms, write_ms, read_ms
            ));
            return Err(DaemonError::ConnectionFailed("Daemon closed connection".into()));
        }
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
            let read_ms = read_start.elapsed().as_secs_f64() * 1000.0;
            log_info("client", "try_send.timeout", &format!(
                "request_id={} serialize={:.1}ms write={:.1}ms read={:.1}ms timeout={:?} err={}",
                request_id, serialize_ms, write_ms, read_ms, timeout, e
            ));
            return Err(DaemonError::ReadTimeout(format!("Read timed out after {:.1}ms (timeout={:?}): {}", read_ms, timeout, e)));
        }
        Err(e) => {
            let read_ms = read_start.elapsed().as_secs_f64() * 1000.0;
            log_info("client", "try_send.read_error", &format!(
                "request_id={} serialize={:.1}ms write={:.1}ms read={:.1}ms err={}",
                request_id, serialize_ms, write_ms, read_ms, e
            ));
            return Err(DaemonError::Other(format!("Failed to read: {}", e)));
        }
    }
    let read_ms = read_start.elapsed().as_secs_f64() * 1000.0;

    // Parse response
    let parse_start = Instant::now();
    let result = serde_json::from_str(&line).map_err(|e| e.into());
    let parse_ms = parse_start.elapsed().as_secs_f64() * 1000.0;
    let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;

    log_info("client", "try_send.done", &format!(
        "request_id={} total={:.1}ms serialize={:.1}ms write={:.1}ms read={:.1}ms parse={:.1}ms response_len={}",
        request_id, total_ms, serialize_ms, write_ms, read_ms, parse_ms, line.len()
    ));

    result
}

/// Get read timeout based on request type.
/// Blocking hooks (poll, subagent-stop) wait up to 30s, so use 35s timeout (30s + buffer).
/// Blocking CLI commands (listen, events launch) use their explicit timeout arg + buffer.
fn get_read_timeout(request: &Request) -> Duration {
    // Note: BLOCKING_HOOKS (poll, subagent-stop) bypass daemon in vanilla mode.
    // In PTY mode they go through daemon but exit immediately, so default 5s is fine.

    // Check blocking CLI commands
    if request.kind == RequestKind::Cli {
        if let Some(ref argv) = request.argv {
            if let Some(cmd) = argv.first() {
                if cmd == "listen" {
                    return Duration::from_secs(parse_listen_timeout(argv) + LISTEN_TIMEOUT_BUFFER_SECS);
                }
                if cmd == "events" {
                    // "events launch" blocks up to 60s waiting for agents to be ready
                    if argv.get(1).map(|s| s.as_str()) == Some("launch") {
                        return Duration::from_secs(EVENTS_LAUNCH_TIMEOUT_SECS);
                    }
                }
                // Potentially slow commands (file search, archive queries)
                if cmd == "transcript" {
                    // "transcript search" uses ripgrep --json which produces huge output
                    // (transcript lines are full conversation turns, 424MB+ for 50 matches)
                    if argv.get(1).map(|s| s.as_str()) == Some("search") {
                        return Duration::from_secs(TRANSCRIPT_SEARCH_TIMEOUT_SECS);
                    }
                }
                if cmd == "archive" {
                    // Archive queries SQLite DBs - can be slow with many archives
                    return Duration::from_secs(ARCHIVE_QUERY_TIMEOUT_SECS);
                }
            }
        }
    }

    // Default timeout - quick commands should fail fast if daemon is hung
    Duration::from_secs(DEFAULT_READ_TIMEOUT_SECS)
}

/// Read available stdin data without blocking.
///
/// Uses poll() to check for data, then reads in non-blocking mode.
/// Returns None if no data available within timeout_ms.
/// Avoids indefinite blocking when pipe never closes (Claude Code Bash tool).
fn read_stdin_nonblocking(timeout_ms: u16) -> Option<String> {
    let has_data = {
        let fd = unsafe { BorrowedFd::borrow_raw(0) };
        let mut pfd = [PollFd::new(fd, PollFlags::POLLIN)];
        poll(&mut pfd, PollTimeout::from(timeout_ms)).unwrap_or(0) > 0
    };
    if !has_data {
        return None;
    }

    // Set stdin to non-blocking, read all available bytes, restore
    let fd = 0i32; // stdin
    let old_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if old_flags < 0 {
        return None;
    }
    unsafe { libc::fcntl(fd, libc::F_SETFL, old_flags | libc::O_NONBLOCK) };

    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = unsafe {
            libc::read(fd, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len())
        };
        if n > 0 {
            buf.extend_from_slice(&tmp[..n as usize]);
        } else {
            // n == 0 (EOF) or n == -1 (EAGAIN/error) — stop reading
            break;
        }
    }

    // Restore original flags
    unsafe { libc::fcntl(fd, libc::F_SETFL, old_flags) };

    if buf.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&buf).into_owned())
    }
}

/// Determine whether a CLI command should read stdin.
///
/// Only `send` supports stdin, and only when --stdin is set or no message arg is present.
/// New syntax: `hcom send @target1 @target2 -- message text`
/// - @targets are skipped (they're routing, not message)
/// - -- separator means message follows inline
/// - No -- means read message from stdin
fn cli_wants_stdin(args: &[String]) -> bool {
    let cmd = match args.first() {
        Some(cmd) => cmd.as_str(),
        None => return false,
    };
    if cmd != "send" {
        return false;
    }

    let mut i = 1;
    let mut saw_stdin = false;
    let mut saw_message = false;

    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--stdin" {
            saw_stdin = true;
            i += 1;
            continue;
        }
        if arg == "--file" || arg == "--base64" {
            // These flags provide the message content — don't read stdin
            saw_message = true;
            break;
        }
        if arg == "--" {
            // -- separator means "inline message follows"
            // Always set saw_message=true - if nothing follows, Python will error
            // (don't fall back to stdin when -- is explicit)
            saw_message = true;
            break;
        }
        if arg.starts_with('@') {
            // Check for backward compat: "@name message" (with space) is old format
            if arg.contains(' ') {
                // Old format: entire arg is message with embedded @mention
                saw_message = true;
                break;
            }
            // New format: @target - skip (routing, not message)
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            if arg == "-b" || arg == "--help" || arg == "-h" {
                i += 1;
                continue;
            }
            if SEND_FLAGS_WITH_VALUES.contains(&arg) {
                if i + 1 < args.len() {
                    i += 2;
                } else {
                    break;
                }
                continue;
            }
            i += 1;
            continue;
        }
        // Non-flag, non-@target = message arg (backward compat for quoted message)
        saw_message = true;
        break;
    }

    if saw_stdin {
        return true;
    }
    !saw_message
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn non_send_commands_skip_stdin() {
        assert!(!cli_wants_stdin(&args(&["list"])));
        assert!(!cli_wants_stdin(&args(&["events", "--last", "5"])));
        assert!(!cli_wants_stdin(&args(&["config", "get", "foo"])));
        assert!(!cli_wants_stdin(&args(&[])));
    }

    #[test]
    fn send_with_message_arg_skips_stdin() {
        // Backward compat: quoted message without --
        assert!(!cli_wants_stdin(&args(&["send", "hello"])));
        assert!(!cli_wants_stdin(&args(&["send", "--name", "foo", "hello"])));
        assert!(!cli_wants_stdin(&args(&["send", "--intent", "request", "hello"])));
    }

    #[test]
    fn send_without_message_reads_stdin() {
        assert!(cli_wants_stdin(&args(&["send"])));
        assert!(cli_wants_stdin(&args(&["send", "--name", "foo"])));
        assert!(cli_wants_stdin(&args(&["send", "--name", "foo", "--intent", "inform"])));
    }

    #[test]
    fn send_with_explicit_stdin_flag() {
        assert!(cli_wants_stdin(&args(&["send", "--stdin", "hello"])));
        assert!(cli_wants_stdin(&args(&["send", "--stdin"])));
    }

    #[test]
    fn send_with_double_dash() {
        assert!(!cli_wants_stdin(&args(&["send", "--", "hello"])));
        // -- with nothing after = NOT stdin (Python will error "No message after --")
        assert!(!cli_wants_stdin(&args(&["send", "--"])));
    }

    #[test]
    fn send_with_at_targets() {
        // @targets without -- = read stdin
        assert!(cli_wants_stdin(&args(&["send", "@luna"])));
        assert!(cli_wants_stdin(&args(&["send", "@luna", "@nova"])));
        assert!(cli_wants_stdin(&args(&["send", "@luna", "--name", "foo"])));

        // @targets with -- and message = no stdin
        assert!(!cli_wants_stdin(&args(&["send", "@luna", "--", "hello"])));
        assert!(!cli_wants_stdin(&args(&["send", "@luna", "@nova", "--", "msg"])));
        assert!(!cli_wants_stdin(&args(&["send", "@luna", "--intent", "request", "--", "msg"])));

        // @targets with -- but no message = NOT stdin (-- means inline message, Python errors)
        assert!(!cli_wants_stdin(&args(&["send", "@luna", "--"])));
        assert!(!cli_wants_stdin(&args(&["send", "@luna", "@nova", "--"])));
    }

    #[test]
    fn send_backward_compat_old_syntax() {
        // Old syntax: "@name message" as single arg (backward compat)
        assert!(!cli_wants_stdin(&args(&["send", "@luna hello there"])));
        assert!(!cli_wants_stdin(&args(&["send", "-b", "@luna hello"])));
    }

    #[test]
    fn send_with_file_flag() {
        assert!(!cli_wants_stdin(&args(&["send", "--file", "/tmp/msg.txt"])));
        assert!(!cli_wants_stdin(&args(&["send", "@luna", "--file", "/tmp/msg.txt"])));
        assert!(!cli_wants_stdin(&args(&["send", "--name", "foo", "--file", "/tmp/msg.txt"])));
    }

    #[test]
    fn send_with_base64_flag() {
        assert!(!cli_wants_stdin(&args(&["send", "--base64", "aGVsbG8="])));
        assert!(!cli_wants_stdin(&args(&["send", "@luna", "--base64", "aGVsbG8="])));
        assert!(!cli_wants_stdin(&args(&["send", "--name", "foo", "--base64", "aGVsbG8="])));
    }
}

/// Generate unique request ID for logging.
pub fn generate_request_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    format!(
        "{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}
