//! Tool enum for type-safe tool identification across hcom.
//!
//! Centralizes tool-specific configuration (ready patterns, etc) to avoid
//! scattered string comparisons and magic values.

use std::str::FromStr;

/// Supported AI coding tools
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Claude,
    Gemini,
    Codex,
}

impl Tool {
    /// Get the ready pattern bytes for this tool
    ///
    /// Ready pattern appears when the tool's TUI has loaded. Used for delivery
    /// thread startup detection (not gating — gate config is in delivery.rs).
    pub fn ready_pattern(&self) -> &'static [u8] {
        match self {
            Tool::Claude => b"? for shortcuts",
            // Codex's responsive footer drops "? for shortcuts" in narrow terminals.
            // Use the › prompt character instead — always visible when TUI is loaded.
            Tool::Codex => "\u{203A} ".as_bytes(),
            Tool::Gemini => b"Type your message",
        }
    }

    /// Get the tool name as a string (lowercase)
    ///
    /// Use this for DB storage, CLI output, and external interfaces.
    pub fn as_str(&self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Gemini => "gemini",
            Tool::Codex => "codex",
        }
    }

    /// Get the tool name as uppercase string (for display)
    #[allow(dead_code)] // Reserved for future terminal title display
    pub fn as_uppercase(&self) -> &'static str {
        match self {
            Tool::Claude => "CLAUDE",
            Tool::Gemini => "GEMINI",
            Tool::Codex => "CODEX",
        }
    }
}

impl FromStr for Tool {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "claude" => Ok(Tool::Claude),
            "gemini" => Ok(Tool::Gemini),
            "codex" => Ok(Tool::Codex),
            _ => Err(format!("Unknown tool: {}", s)),
        }
    }
}

impl std::fmt::Display for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}