//! Shared constants for hcom — version, limits, patterns, and status definitions.

use regex::Regex;
use std::sync::LazyLock;

use super::ansi::{
    BG_BLUE, BG_GRAY, BG_GREEN, BG_RED, BG_YELLOW, FG_BLUE, FG_GRAY, FG_GREEN, FG_RED, FG_YELLOW,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// CLI sender identity (the human operator).
pub const SENDER: &str = "bigboss";

/// System notification identity (launcher, watchdog, subscriptions).
pub const SYSTEM_SENDER: &str = "hcom";

/// Max messages delivered in a single hook response.
pub const MAX_MESSAGES_PER_DELIVERY: usize = 50;

/// Max message body size (1MB).
pub const MAX_MESSAGE_SIZE: usize = 1_048_576;

/// Stop hook polling interval in seconds.
pub const STOP_HOOK_POLL_INTERVAL_SECS: f64 = 0.1;

/// @mention regex — matches `@name` but not `email@domain` or `path.@name`.
///
/// Rejects @mentions preceded by [a-zA-Z0-9._-] to prevent matching:
/// - email: user@domain.com
/// - paths: /file.@test
/// - identifiers: var_@name, some-id@mention
///
/// Capture group 1 is the name. Includes `:` for remote names (@luna:BOXE).
pub static MENTION_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[^a-zA-Z0-9._\-])@([a-zA-Z0-9][\w:\-]*)").unwrap());

/// Extract all @mention names from text.
pub fn extract_mentions(text: &str) -> Vec<String> {
    MENTION_PATTERN
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .collect()
}

/// Binding marker for vanilla sessions: [hcom:<name>].
pub static BIND_MARKER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[hcom:([a-z0-9_]+)\]").unwrap());

/// Tools available for launch.
pub const RELEASED_TOOLS: &[&str] = &["claude", "gemini", "codex", "opencode", "kilocode"];

/// Tools that support background/headless mode.
pub const RELEASED_BACKGROUND: &[&str] = &["claude"];

/// Tool detection markers — set by AI tools, cleared to prevent inheritance.
pub const TOOL_MARKER_VARS: &[&str] = &[
    "CLAUDECODE",
    "GEMINI_CLI",
    "GEMINI_SYSTEM_MD",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
    "CODEX_MANAGED_BY_NPM",
    "CODEX_MANAGED_BY_BUN",
    "CODEX_THREAD_ID",
    "OPENCODE",
    "KILOCODE",
];

/// HCOM identity vars — set per-instance, cleared to prevent parent identity leakage.
pub const HCOM_IDENTITY_VARS: &[&str] = &[
    "HCOM_PROCESS_ID",
    "HCOM_LAUNCHED",
    // HCOM_LAUNCHED_PRESET excluded — must survive into Rust binary for hook forwarding
    "HCOM_PTY_MODE",
    "HCOM_BACKGROUND",
    "HCOM_LAUNCHED_BY",
    "HCOM_LAUNCH_BATCH_ID",
    "HCOM_LAUNCH_EVENT_ID",
];

pub const ST_ACTIVE: &str = "active";
pub const ST_LISTENING: &str = "listening";
pub const ST_BLOCKED: &str = "blocked";
pub const ST_INACTIVE: &str = "inactive";
pub const ST_LAUNCHING: &str = "launching";
pub const ST_ERROR: &str = "error";

/// Valid status values (ordered for display priority).
pub const STATUS_ORDER: &[&str] = &[
    ST_ACTIVE,
    ST_LISTENING,
    ST_BLOCKED,
    ST_ERROR,
    ST_LAUNCHING,
    ST_INACTIVE,
];

/// Status icons (unicode).
pub fn status_icon(status: &str) -> &'static str {
    match status {
        ST_ACTIVE => "\u{25b6}",    // ▶
        ST_LISTENING => "\u{25c9}", // ◉
        ST_BLOCKED => "\u{25a0}",   // ■
        ST_LAUNCHING => "\u{25ce}", // ◎
        ST_ERROR => "\u{2717}",     // ✗
        "stopped" => "\u{2298}",    // ⊘
        ST_INACTIVE => "\u{25cb}",  // ○
        _ => "\u{25cb}",            // ○
    }
}

/// Adhoc instance icon (neutral — not claiming alive or dead).
pub const ADHOC_ICON: &str = "\u{25e6}"; // ◦

/// Status foreground ANSI color.
pub fn status_fg(status: &str) -> &'static str {
    match status {
        ST_ACTIVE => FG_GREEN,
        ST_LISTENING => FG_BLUE,
        ST_BLOCKED => FG_RED,
        ST_LAUNCHING => FG_YELLOW,
        ST_ERROR => FG_RED,
        ST_INACTIVE => FG_GRAY,
        _ => FG_GRAY,
    }
}

/// Status background ANSI color.
pub fn status_bg(status: &str) -> &'static str {
    match status {
        ST_ACTIVE => BG_GREEN,
        ST_LISTENING => BG_BLUE,
        ST_BLOCKED => BG_RED,
        ST_LAUNCHING => BG_YELLOW,
        ST_ERROR => BG_RED,
        ST_INACTIVE => BG_GRAY,
        _ => BG_GRAY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mention_pattern_basic() {
        assert_eq!(
            extract_mentions("hello @luna and @nova"),
            vec!["luna", "nova"]
        );
    }

    #[test]
    fn test_mention_pattern_rejects_email() {
        assert!(extract_mentions("email user@domain.com").is_empty());
    }

    #[test]
    fn test_mention_pattern_remote_name() {
        assert_eq!(extract_mentions("send to @luna:BOXE"), vec!["luna:BOXE"]);
    }

    #[test]
    fn test_mention_pattern_rejects_underscore_prefix() {
        assert!(
            extract_mentions("var_@name").is_empty(),
            "should reject underscore-prefixed mention"
        );
    }

    #[test]
    fn test_mention_pattern_rejects_dot_prefix() {
        assert!(
            extract_mentions("file.@name").is_empty(),
            "should reject dot-prefixed mention"
        );
    }

    #[test]
    fn test_mention_pattern_start_of_string() {
        assert_eq!(extract_mentions("@luna hello"), vec!["luna"]);
    }

    #[test]
    fn test_bind_marker() {
        let caps = BIND_MARKER_RE.captures("[hcom:luna]");
        assert_eq!(caps.unwrap()[1].to_string(), "luna");
    }

    #[test]
    fn test_bind_marker_no_legacy() {
        assert!(BIND_MARKER_RE.captures("[HCOM:BIND:test_name]").is_none());
    }

    #[test]
    fn test_status_icons() {
        assert_eq!(status_icon(ST_ACTIVE), "▶");
        assert_eq!(status_icon(ST_LISTENING), "◉");
        assert_eq!(status_icon(ST_BLOCKED), "■");
        assert_eq!(status_icon(ST_LAUNCHING), "◎");
        assert_eq!(status_icon(ST_ERROR), "✗");
        assert_eq!(status_icon(ST_INACTIVE), "○");
    }

    #[test]
    fn test_released_tools() {
        assert!(RELEASED_TOOLS.contains(&"claude"));
        assert!(RELEASED_TOOLS.contains(&"gemini"));
        assert!(RELEASED_TOOLS.contains(&"codex"));
        assert!(RELEASED_TOOLS.contains(&"opencode"));
        assert!(RELEASED_TOOLS.contains(&"kilocode"));
        assert_eq!(RELEASED_TOOLS.len(), 5);
    }

    #[test]
    fn test_status_order() {
        assert_eq!(STATUS_ORDER.len(), 6);
        assert_eq!(STATUS_ORDER[0], ST_ACTIVE);
        assert_eq!(STATUS_ORDER[5], ST_INACTIVE);
    }
}
