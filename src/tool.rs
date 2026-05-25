//! Tool enum for type-safe tool identification across hcom.
//!
//! Centralizes tool-specific configuration (ready patterns, etc) to avoid
//! scattered string comparisons and magic values.

use std::str::FromStr;

const CLAUDE_HOOKS: &[&str] = &[
    "poll",
    "notify",
    "permission-request",
    "pre",
    "post",
    "sessionstart",
    "userpromptsubmit",
    "sessionend",
    "subagent-start",
    "subagent-stop",
];

const GEMINI_HOOKS: &[&str] = &[
    "gemini-sessionstart",
    "gemini-beforeagent",
    "gemini-afteragent",
    "gemini-beforetool",
    "gemini-aftertool",
    "gemini-notification",
    "gemini-sessionend",
];

const CODEX_HOOKS: &[&str] = &[
    "codex-sessionstart",
    "codex-userpromptsubmit",
    "codex-pretooluse",
    "codex-posttooluse",
    "codex-stop",
];

const OPENCODE_HOOKS: &[&str] = &[
    "opencode-start",
    "opencode-status",
    "opencode-read",
    "opencode-stop",
];

/// Supported AI coding tools
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Claude,
    Gemini,
    Codex,
    OpenCode,
    Antigravity,
    Adhoc,
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
            // OpenCode: bottom status bar — appears when TUI is fully rendered.
            // Gates delivery thread startup so PTY bootstrap inject doesn't fire
            // into a blank screen before the input box exists.
            Tool::OpenCode => b"ctrl+p commands",
            Tool::Antigravity => b"? for shortcuts",
            Tool::Adhoc => b"",
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
            Tool::OpenCode => "opencode",
            Tool::Antigravity => "antigravity",
            Tool::Adhoc => "adhoc",
        }
    }

    /// Hook command names owned by this tool.
    pub fn hooks(&self) -> &'static [&'static str] {
        match self {
            Tool::Claude => CLAUDE_HOOKS,
            Tool::Gemini => GEMINI_HOOKS,
            Tool::Codex => CODEX_HOOKS,
            Tool::OpenCode => OPENCODE_HOOKS,
            Tool::Antigravity => GEMINI_HOOKS,
            Tool::Adhoc => &[],
        }
    }

    /// Return true if this tool owns the hook command name.
    pub fn owns_hook(&self, name: &str) -> bool {
        self.hooks().contains(&name)
    }

    /// Resolve the tool that owns a hook command name.
    ///
    /// Antigravity is intentionally excluded: it reuses Gemini hook command names
    /// (`gemini-sessionstart`, etc.) and is identified via `ANTIGRAVITY_AGENT` in
    /// `HcomContext`, not hook-name routing.
    pub fn from_hook_name(name: &str) -> Option<Self> {
        [Tool::Claude, Tool::Gemini, Tool::Codex, Tool::OpenCode]
            .into_iter()
            .find(|tool| tool.owns_hook(name))
    }

    /// Return true if any supported tool owns the hook command name.
    pub fn is_hook_name(name: &str) -> bool {
        Self::from_hook_name(name).is_some()
    }
}

impl FromStr for Tool {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "claude" => Ok(Tool::Claude),
            "gemini" => Ok(Tool::Gemini),
            "codex" => Ok(Tool::Codex),
            "opencode" => Ok(Tool::OpenCode),
            "antigravity" | "agy" => Ok(Tool::Antigravity),
            "adhoc" => Ok(Tool::Adhoc),
            _ => Err(format!("Unknown tool: {}", s)),
        }
    }
}

impl std::fmt::Display for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn adhoc_has_no_hooks() {
        assert!(Tool::Adhoc.hooks().is_empty());
        assert_ne!(Tool::from_hook_name("poll"), Some(Tool::Adhoc));
    }

    #[test]
    fn hook_names_are_disjoint() {
        // Antigravity shares GEMINI_HOOKS — excluded so gemini-* names stay owned by Gemini.
        let mut owners = HashMap::new();
        for tool in [Tool::Claude, Tool::Gemini, Tool::Codex, Tool::OpenCode] {
            for hook in tool.hooks() {
                assert_eq!(
                    owners.insert(*hook, tool),
                    None,
                    "{hook} has multiple owners"
                );
                assert_eq!(Tool::from_hook_name(hook), Some(tool));
            }
        }
    }

    #[test]
    fn antigravity_as_str() {
        assert_eq!(Tool::Antigravity.as_str(), "antigravity");
    }

    #[test]
    fn antigravity_from_str() {
        assert_eq!("antigravity".parse::<Tool>(), Ok(Tool::Antigravity));
    }

    #[test]
    fn antigravity_agy_alias() {
        assert_eq!("agy".parse::<Tool>(), Ok(Tool::Antigravity));
    }

    #[test]
    fn antigravity_ready_pattern() {
        assert_eq!(Tool::Antigravity.ready_pattern(), b"? for shortcuts");
    }

    #[test]
    fn antigravity_shares_gemini_hooks() {
        assert_eq!(Tool::Antigravity.hooks(), Tool::Gemini.hooks());
    }
}
