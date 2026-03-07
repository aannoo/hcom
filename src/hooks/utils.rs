//! Hook constants, registry, and classification.
//!
//! Single source of truth for which hooks exist and how they behave.

use std::collections::HashSet;
use std::sync::LazyLock;

// ==================== Exit Codes ====================

/// Hook allowed the operation — exit 0.
pub const EXIT_ALLOW: i32 = 0;

/// Hook internal error — exit 1.
pub const EXIT_ERROR: i32 = 1;

/// Hook blocked the operation — exit 2 (message delivery).
pub const EXIT_BLOCK: i32 = 2;

// ==================== Hook Categories ====================

/// Hook input delivery mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookCategory {
    /// Hook receives payload via stdin (JSON).
    Stdin,
    /// Hook receives payload via command-line arguments.
    Argv,
    /// Hook can block for extended periods (polling loops).
    Blocking,
}

// ==================== Hook Info ====================

/// Static info about a hook type.
#[derive(Debug, Clone)]
pub struct HookInfo {
    /// Hook event name as the tool sees it (e.g., "Stop", "PostToolUse").
    pub event_name: &'static str,
    /// hcom subcommand suffix (e.g., "poll", "post", "sessionstart").
    pub command_suffix: &'static str,
    /// Tool matcher pattern (e.g., "Bash|Task|Write|Edit" for PreToolUse).
    pub matcher: &'static str,
    /// Timeout in seconds (None = tool default).
    pub timeout: Option<u64>,
    /// Input categories for this hook.
    pub categories: &'static [HookCategory],
}

// ==================== Hook Registry ====================

/// All Claude hook types with their configuration.
///
pub static HOOK_REGISTRY: LazyLock<Vec<HookInfo>> = LazyLock::new(|| {
    vec![
        HookInfo {
            event_name: "SessionStart",
            command_suffix: "sessionstart",
            matcher: "",
            timeout: None,
            categories: &[HookCategory::Stdin],
        },
        HookInfo {
            event_name: "UserPromptSubmit",
            command_suffix: "userpromptsubmit",
            matcher: "",
            timeout: None,
            categories: &[HookCategory::Stdin],
        },
        HookInfo {
            event_name: "PreToolUse",
            command_suffix: "pre",
            matcher: "Bash|Task|Write|Edit",
            timeout: None,
            categories: &[HookCategory::Stdin],
        },
        HookInfo {
            event_name: "PostToolUse",
            command_suffix: "post",
            matcher: "",
            timeout: Some(86400),
            categories: &[HookCategory::Stdin],
        },
        HookInfo {
            event_name: "Stop",
            command_suffix: "poll",
            matcher: "",
            timeout: None,
            categories: &[HookCategory::Stdin],
        },
        HookInfo {
            event_name: "SubagentStart",
            command_suffix: "subagent-start",
            matcher: "",
            timeout: None,
            categories: &[HookCategory::Stdin],
        },
        HookInfo {
            event_name: "SubagentStop",
            command_suffix: "subagent-stop",
            matcher: "",
            timeout: Some(86400),
            categories: &[HookCategory::Stdin, HookCategory::Blocking],
        },
        HookInfo {
            event_name: "Notification",
            command_suffix: "notify",
            matcher: "",
            timeout: None,
            categories: &[HookCategory::Stdin],
        },
        HookInfo {
            event_name: "SessionEnd",
            command_suffix: "sessionend",
            matcher: "",
            timeout: None,
            categories: &[HookCategory::Stdin],
        },
    ]
});

/// Set of hooks that receive input via stdin.
pub static STDIN_HOOKS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HOOK_REGISTRY
        .iter()
        .filter(|h| h.categories.contains(&HookCategory::Stdin))
        .map(|h| h.event_name)
        .collect()
});

/// Set of hooks that can block for extended periods.
pub static BLOCKING_HOOKS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HOOK_REGISTRY
        .iter()
        .filter(|h| h.categories.contains(&HookCategory::Blocking))
        .map(|h| h.event_name)
        .collect()
});

/// Look up hook info by event name.
pub fn get_hook_info(event_name: &str) -> Option<&'static HookInfo> {
    HOOK_REGISTRY.iter().find(|h| h.event_name == event_name)
}

/// Look up hook info by command suffix.
pub fn get_hook_by_suffix(suffix: &str) -> Option<&'static HookInfo> {
    HOOK_REGISTRY
        .iter()
        .find(|h| h.command_suffix == suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_registry_count() {
        assert_eq!(HOOK_REGISTRY.len(), 9);
    }

    #[test]
    fn test_stdin_hooks_populated() {
        assert!(STDIN_HOOKS.contains("Stop"));
        assert!(STDIN_HOOKS.contains("PostToolUse"));
        assert!(STDIN_HOOKS.contains("SessionStart"));
    }

    #[test]
    fn test_blocking_hooks() {
        assert!(!BLOCKING_HOOKS.contains("Stop"));
        assert!(BLOCKING_HOOKS.contains("SubagentStop"));
        assert!(!BLOCKING_HOOKS.contains("PostToolUse"));
        assert!(!BLOCKING_HOOKS.contains("SessionStart"));
    }

    #[test]
    fn test_get_hook_info() {
        let info = get_hook_info("Stop").unwrap();
        assert_eq!(info.command_suffix, "poll");
        assert_eq!(info.timeout, None);

        let info = get_hook_info("PreToolUse").unwrap();
        assert_eq!(info.matcher, "Bash|Task|Write|Edit");
        assert!(info.timeout.is_none());

        assert!(get_hook_info("NonExistent").is_none());
    }

    #[test]
    fn test_get_hook_by_suffix() {
        let info = get_hook_by_suffix("poll").unwrap();
        assert_eq!(info.event_name, "Stop");

        let info = get_hook_by_suffix("post").unwrap();
        assert_eq!(info.event_name, "PostToolUse");

        assert!(get_hook_by_suffix("nonexistent").is_none());
    }

    #[test]
    fn test_exit_codes() {
        assert_eq!(EXIT_ALLOW, 0);
        assert_eq!(EXIT_ERROR, 1);
        assert_eq!(EXIT_BLOCK, 2);
    }

    #[test]
    fn test_hook_timeouts() {
        // Blocking hooks should have timeouts
        for hook in HOOK_REGISTRY.iter() {
            if hook.categories.contains(&HookCategory::Blocking) {
                assert!(
                    hook.timeout.is_some(),
                    "{} is blocking but has no timeout",
                    hook.event_name
                );
            }
        }
    }
}
