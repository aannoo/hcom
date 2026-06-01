//! Per-request execution context for hcom.
//!
//! HcomContext is constructed once at request entry and passed by reference
//! to all handlers. No global state or thread-locals.

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

use crate::tool::Tool;

/// Per-request execution context.
///
/// Constructed once at entry (hook invocation or CLI command), then passed
/// by reference. Contains everything a handler needs: env snapshot, derived
/// paths, tool detection, and identity info.
///
/// No thread-local storage — explicit parameter passing everywhere.
#[derive(Debug, Clone)]
pub struct HcomContext {
    // === Identity ===
    /// HCOM_PROCESS_ID — identifies launched instances.
    pub process_id: Option<String>,
    /// HCOM_LAUNCHED=1 — true if launched by hcom.
    pub is_launched: bool,
    /// HCOM_PTY_MODE=1 — running in PTY wrapper.
    pub is_pty_mode: bool,
    /// HCOM_BACKGROUND is set — background/headless mode.
    pub is_background: bool,
    /// Log filename for background mode (from HCOM_BACKGROUND).
    pub background_name: Option<String>,

    // === Paths ===
    /// Path to hcom data directory (~/.hcom or HCOM_DIR).
    pub hcom_dir: PathBuf,
    /// True if HCOM_DIR was explicitly set.
    pub hcom_dir_override: bool,
    /// Current working directory when context was captured.
    pub cwd: PathBuf,

    // === Tool detection ===
    /// Detected tool type.
    pub tool: Tool,
    /// CLAUDE_ENV_FILE path (for session ID extraction).
    pub claude_env_file: Option<String>,
    /// Tool markers for context-based detection.
    pub is_claude: bool,
    pub is_antigravity: bool,
    pub is_gemini: bool,
    pub is_codex: bool,
    pub is_opencode: bool,
    pub is_kilo: bool,
    pub is_cursor: bool,
    /// HCOM_IS_FORK=1 (--fork-session launch).
    pub is_fork: bool,
    /// Codex thread ID (session equivalent).
    pub codex_thread_id: Option<String>,

    // === Launch context ===
    /// HCOM_LAUNCHED_BY — name of instance that launched this one.
    pub launched_by: Option<String>,
    /// HCOM_LAUNCH_BATCH_ID — batch identifier for grouped launches.
    pub launch_batch_id: Option<String>,
    /// HCOM_LAUNCH_EVENT_ID — event ID for this launch.
    pub launch_event_id: Option<String>,
    /// HCOM_LAUNCHED_PRESET — terminal preset used to launch.
    pub launched_preset: Option<String>,
    /// HCOM_NOTES — per-instance bootstrap user notes.
    pub notes: String,

    // === I/O ===
    /// Whether client stdin is a TTY.
    pub stdin_is_tty: bool,
    /// Whether client stdout is a TTY.
    pub stdout_is_tty: bool,

    // === Raw env ===
    /// Full forwarded env dict — used by config loading for env overrides.
    pub raw_env: HashMap<String, String>,
}

impl HcomContext {
    /// Build context from an explicit environment map.
    ///
    /// Primary constructor — used by both CLI (from os env) and future
    /// direct-call paths. TTY flags default to true for normal CLI usage;
    /// callers with non-TTY stdin/stdout should use `with_tty()` after construction.
    pub fn from_env(env: &HashMap<String, String>, cwd: PathBuf) -> Self {
        let get = |key: &str| env.get(key).cloned();
        let get_nonempty = |key: &str| get(key).filter(|v| !v.is_empty());
        let is_set = |key: &str| env.contains_key(key);
        let is_eq = |key: &str, val: &str| env.get(key).is_some_and(|v| v == val);

        // Tool markers
        let is_claude = is_eq("CLAUDECODE", "1") || get_nonempty("CLAUDE_ENV_FILE").is_some();
        let is_antigravity = is_set("ANTIGRAVITY_AGENT");
        let is_gemini = is_eq("GEMINI_CLI", "1");
        let is_codex = is_set("CODEX_SANDBOX")
            || is_set("CODEX_SANDBOX_NETWORK_DISABLED")
            || is_set("CODEX_MANAGED_BY_NPM")
            || is_set("CODEX_MANAGED_BY_BUN")
            || is_set("CODEX_THREAD_ID");
        let is_opencode = is_eq("OPENCODE", "1");
        let is_kilo = is_eq("KILO", "1");
        let is_cursor = is_set("CURSOR_AGENT") || is_set("CURSOR_PROJECT_DIR");

        // Determine tool type
        let tool = if is_claude {
            Tool::Claude
        } else if is_antigravity {
            Tool::Antigravity
        } else if is_gemini {
            Tool::Gemini
        } else if is_codex {
            Tool::Codex
        } else if is_opencode {
            Tool::OpenCode
        } else if is_kilo {
            Tool::Kilo
        } else if is_cursor {
            Tool::Cursor
        } else {
            Tool::Adhoc
        };

        // Resolve hcom_dir using the same normalization as Config/paths.
        let (hcom_dir, hcom_dir_override) = crate::paths::resolve_hcom_dir_from_env(env, &cwd);

        Self {
            process_id: get_nonempty("HCOM_PROCESS_ID"),
            is_launched: is_eq("HCOM_LAUNCHED", "1"),
            is_pty_mode: is_eq("HCOM_PTY_MODE", "1"),
            is_background: get_nonempty("HCOM_BACKGROUND").is_some(),
            background_name: get_nonempty("HCOM_BACKGROUND"),
            hcom_dir,
            hcom_dir_override,
            cwd,
            tool,
            claude_env_file: get_nonempty("CLAUDE_ENV_FILE"),
            is_claude,
            is_antigravity,
            is_gemini,
            is_codex,
            is_opencode,
            is_kilo,
            is_cursor,
            is_fork: is_eq("HCOM_IS_FORK", "1"),
            codex_thread_id: get_nonempty("CODEX_THREAD_ID"),
            launched_by: get_nonempty("HCOM_LAUNCHED_BY"),
            launch_batch_id: get_nonempty("HCOM_LAUNCH_BATCH_ID"),
            launch_event_id: get_nonempty("HCOM_LAUNCH_EVENT_ID"),
            launched_preset: get_nonempty("HCOM_LAUNCHED_PRESET"),
            notes: get("HCOM_NOTES").unwrap_or_default(),
            stdin_is_tty: true,
            stdout_is_tty: true,
            raw_env: env.clone(),
        }
    }

    /// Build context from the current process environment.
    ///
    /// Convenience for CLI mode — detects actual stdin/stdout TTY state.
    pub fn from_os() -> Self {
        use std::io::IsTerminal;
        let env: HashMap<String, String> = env::vars().collect();
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut ctx = Self::from_env(&env, cwd);
        ctx.stdin_is_tty = std::io::stdin().is_terminal();
        ctx.stdout_is_tty = std::io::stdout().is_terminal();
        ctx
    }

    /// Set TTY state (for callers that know the client's TTY status).
    pub fn with_tty(mut self, stdin_is_tty: bool, stdout_is_tty: bool) -> Self {
        self.stdin_is_tty = stdin_is_tty;
        self.stdout_is_tty = stdout_is_tty;
        self
    }

    // === Derived paths ===

    /// Path to hcom.db.
    pub fn db_path(&self) -> PathBuf {
        self.hcom_dir.join("hcom.db")
    }

    /// Path to logs directory.
    pub fn log_dir(&self) -> PathBuf {
        self.hcom_dir.join(".tmp").join("logs")
    }

    /// Path to hcom.log.
    pub fn log_path(&self) -> PathBuf {
        self.log_dir().join("hcom.log")
    }

    /// Whether running inside any AI tool.
    pub fn is_inside_ai_tool(&self) -> bool {
        self.is_claude
            || self.is_antigravity
            || self.is_launched
            || self.is_gemini
            || self.is_codex
            || self.is_opencode
            || self.is_kilo
            || self.is_cursor
    }

    /// Detect current tool name, or "adhoc".
    pub fn detect_current_tool(&self) -> &'static str {
        self.tool.as_str()
    }

    /// Detect vanilla (non-hcom-launched) tool, or None.
    pub fn detect_vanilla_tool(&self) -> Option<&'static str> {
        if self.is_launched {
            return None;
        }
        match self.tool {
            Tool::Adhoc => None,
            _ => Some(self.tool.as_str()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn test_from_env_claude() {
        let env = make_env(&[("CLAUDECODE", "1"), ("HOME", "/home/test")]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert!(ctx.is_claude);
        assert!(!ctx.is_gemini);
        assert!(!ctx.is_codex);
        assert_eq!(ctx.tool, Tool::Claude);
        assert_eq!(ctx.cwd, PathBuf::from("/tmp"));
    }

    #[test]
    fn test_from_env_antigravity() {
        let env = make_env(&[("ANTIGRAVITY_AGENT", "1"), ("HOME", "/home/test")]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert!(ctx.is_antigravity);
        assert_eq!(ctx.tool, Tool::Antigravity);
    }

    #[test]
    fn test_antigravity_priority_over_gemini() {
        let env = make_env(&[
            ("ANTIGRAVITY_AGENT", "1"),
            ("GEMINI_CLI", "1"),
            ("HOME", "/home/test"),
        ]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert!(ctx.is_antigravity);
        assert!(ctx.is_gemini);
        assert_eq!(ctx.tool, Tool::Antigravity);
    }

    #[test]
    fn test_from_env_gemini() {
        let env = make_env(&[("GEMINI_CLI", "1"), ("HOME", "/home/test")]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert!(ctx.is_gemini);
        assert_eq!(ctx.tool, Tool::Gemini);
    }

    #[test]
    fn test_from_env_codex() {
        let env = make_env(&[("CODEX_SANDBOX", "1"), ("HOME", "/home/test")]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert!(ctx.is_codex);
        assert_eq!(ctx.tool, Tool::Codex);
    }

    #[test]
    fn test_from_env_codex_thread_id() {
        let env = make_env(&[("CODEX_THREAD_ID", "thread-abc"), ("HOME", "/home/test")]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert!(ctx.is_codex);
        assert_eq!(ctx.codex_thread_id.as_deref(), Some("thread-abc"));
    }

    #[test]
    fn test_from_env_opencode() {
        let env = make_env(&[("OPENCODE", "1"), ("HOME", "/home/test")]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert!(ctx.is_opencode);
        assert_eq!(ctx.tool, Tool::OpenCode);
    }

    #[test]
    fn test_from_env_kilo() {
        let env = make_env(&[("KILO", "1"), ("HOME", "/home/test")]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert!(ctx.is_kilo);
        assert_eq!(ctx.tool, Tool::Kilo);
        assert_eq!(ctx.detect_vanilla_tool(), Some("kilo"));
    }

    #[test]
    fn test_from_env_adhoc() {
        let env = make_env(&[("HOME", "/home/test")]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert!(!ctx.is_claude);
        assert!(!ctx.is_gemini);
        assert!(!ctx.is_codex);
        assert!(!ctx.is_opencode);
        assert!(!ctx.is_kilo);
        assert_eq!(ctx.tool, Tool::Adhoc);
    }

    #[test]
    fn test_from_env_claude_env_file() {
        let env = make_env(&[
            ("CLAUDE_ENV_FILE", "/tmp/.claude_env"),
            ("HOME", "/home/test"),
        ]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert!(ctx.is_claude);
        assert_eq!(ctx.tool, Tool::Claude);
        assert_eq!(ctx.claude_env_file.as_deref(), Some("/tmp/.claude_env"));
    }

    #[test]
    fn test_hcom_dir_default() {
        let env = make_env(&[("HOME", "/home/test")]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert_eq!(ctx.hcom_dir, PathBuf::from("/home/test/.hcom"));
        assert!(!ctx.hcom_dir_override);
    }

    #[test]
    fn test_hcom_dir_override() {
        let env = make_env(&[("HCOM_DIR", "/custom/hcom"), ("HOME", "/home/test")]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert_eq!(ctx.hcom_dir, PathBuf::from("/custom/hcom"));
        assert!(ctx.hcom_dir_override);
    }

    #[test]
    fn test_hcom_dir_tilde_expansion() {
        let env = make_env(&[("HCOM_DIR", "~/custom/.hcom"), ("HOME", "/home/test")]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert_eq!(ctx.hcom_dir, PathBuf::from("/home/test/custom/.hcom"));
    }

    #[test]
    fn test_hcom_dir_relative_resolved_to_absolute() {
        let env = make_env(&[("HCOM_DIR", "relative/.hcom"), ("HOME", "/home/test")]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp/worktree"));

        assert_eq!(ctx.hcom_dir, PathBuf::from("/tmp/worktree/relative/.hcom"));
    }

    #[test]
    fn test_identity_fields() {
        let env = make_env(&[
            ("HCOM_PROCESS_ID", "pid-123"),
            ("HCOM_LAUNCHED", "1"),
            ("HCOM_PTY_MODE", "1"),
            ("HCOM_BACKGROUND", "agent.log"),
            ("HCOM_LAUNCHED_BY", "luna"),
            ("HCOM_LAUNCH_BATCH_ID", "batch-1"),
            ("HCOM_LAUNCH_EVENT_ID", "42"),
            ("HCOM_LAUNCHED_PRESET", "kitty"),
            ("HCOM_IS_FORK", "1"),
            ("HCOM_NOTES", "test notes"),
            ("HOME", "/home/test"),
        ]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert_eq!(ctx.process_id.as_deref(), Some("pid-123"));
        assert!(ctx.is_launched);
        assert!(ctx.is_pty_mode);
        assert!(ctx.is_background);
        assert_eq!(ctx.background_name.as_deref(), Some("agent.log"));
        assert_eq!(ctx.launched_by.as_deref(), Some("luna"));
        assert_eq!(ctx.launch_batch_id.as_deref(), Some("batch-1"));
        assert_eq!(ctx.launch_event_id.as_deref(), Some("42"));
        assert_eq!(ctx.launched_preset.as_deref(), Some("kitty"));
        assert!(ctx.is_fork);
        assert_eq!(ctx.notes, "test notes");
    }

    #[test]
    fn test_empty_values_become_none() {
        let env = make_env(&[
            ("HCOM_PROCESS_ID", ""),
            ("HCOM_LAUNCHED", "0"),
            ("HCOM_BACKGROUND", ""),
            ("HOME", "/home/test"),
        ]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert!(ctx.process_id.is_none());
        assert!(!ctx.is_launched);
        assert!(!ctx.is_background);
        assert!(ctx.background_name.is_none());
    }

    #[test]
    fn test_derived_paths() {
        let env = make_env(&[("HOME", "/home/test")]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert_eq!(ctx.db_path(), PathBuf::from("/home/test/.hcom/hcom.db"));
        assert_eq!(ctx.log_dir(), PathBuf::from("/home/test/.hcom/.tmp/logs"));
        assert_eq!(
            ctx.log_path(),
            PathBuf::from("/home/test/.hcom/.tmp/logs/hcom.log")
        );
    }

    #[test]
    fn test_is_inside_ai_tool() {
        let adhoc =
            HcomContext::from_env(&make_env(&[("HOME", "/home/test")]), PathBuf::from("/tmp"));
        assert!(!adhoc.is_inside_ai_tool());

        let claude = HcomContext::from_env(
            &make_env(&[("CLAUDECODE", "1"), ("HOME", "/home/test")]),
            PathBuf::from("/tmp"),
        );
        assert!(claude.is_inside_ai_tool());

        let launched = HcomContext::from_env(
            &make_env(&[("HCOM_LAUNCHED", "1"), ("HOME", "/home/test")]),
            PathBuf::from("/tmp"),
        );
        assert!(launched.is_inside_ai_tool());
    }

    #[test]
    fn test_detect_vanilla_tool() {
        // Claude not launched by hcom = vanilla
        let ctx = HcomContext::from_env(
            &make_env(&[("CLAUDECODE", "1"), ("HOME", "/home/test")]),
            PathBuf::from("/tmp"),
        );
        assert_eq!(ctx.detect_vanilla_tool(), Some("claude"));

        // Claude launched by hcom = not vanilla
        let ctx = HcomContext::from_env(
            &make_env(&[
                ("CLAUDECODE", "1"),
                ("HCOM_LAUNCHED", "1"),
                ("HOME", "/home/test"),
            ]),
            PathBuf::from("/tmp"),
        );
        assert_eq!(ctx.detect_vanilla_tool(), None);

        // Adhoc = not vanilla
        let ctx =
            HcomContext::from_env(&make_env(&[("HOME", "/home/test")]), PathBuf::from("/tmp"));
        assert_eq!(ctx.detect_vanilla_tool(), None);
    }

    #[test]
    fn test_tool_type_display() {
        assert_eq!(Tool::Claude.as_str(), "claude");
        assert_eq!(Tool::Gemini.as_str(), "gemini");
        assert_eq!(Tool::Codex.as_str(), "codex");
        assert_eq!(Tool::OpenCode.as_str(), "opencode");
        assert_eq!(Tool::Kilo.as_str(), "kilo");
        assert_eq!(Tool::Adhoc.as_str(), "adhoc");
    }

    #[test]
    fn test_tool_priority_claude_over_codex() {
        // If both CLAUDECODE and CODEX_SANDBOX are set, claude wins
        let env = make_env(&[
            ("CLAUDECODE", "1"),
            ("CODEX_SANDBOX", "1"),
            ("HOME", "/home/test"),
        ]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));
        assert_eq!(ctx.tool, Tool::Claude);
    }

    #[test]
    fn test_raw_env_preserved() {
        let env = make_env(&[
            ("HOME", "/home/test"),
            ("HCOM_TAG", "test-tag"),
            ("CUSTOM_VAR", "custom-val"),
        ]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("/tmp"));

        assert_eq!(ctx.raw_env.get("HCOM_TAG").unwrap(), "test-tag");
        assert_eq!(ctx.raw_env.get("CUSTOM_VAR").unwrap(), "custom-val");
    }
}
