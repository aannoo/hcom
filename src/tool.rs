//! Tool enum for type-safe tool identification across hcom.
//!
//! Per-tool data (hook names, ready pattern, delivery gates, help, status
//! mappings, etc.) lives in [`crate::integration_spec`]. This module just
//! defines the enum and a thin set of forwarders.

use std::str::FromStr;

use crate::integration_spec;

/// Supported AI coding tools
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Claude,
    Gemini,
    Codex,
    OpenCode,
    Kilo,
    Antigravity,
    Cursor,
    Kimi,
    Copilot,
    Pi,
    Omp,
    Adhoc,
}

impl Tool {
    /// Ready-pattern bytes for PTY readiness detection.
    pub fn ready_pattern(&self) -> &'static [u8] {
        self.spec().ready_pattern
    }

    /// Lowercase tool name used in DB, CLI output, and external interfaces.
    pub fn as_str(&self) -> &'static str {
        self.spec().name
    }

    /// Hook command names listed for this tool. Some tools borrow another
    /// tool's names; use `owns_hook` for routing ownership.
    pub fn hooks(&self) -> &'static [&'static str] {
        self.spec().hooks.names
    }

    /// True if this tool owns `name` for routing. Borrowed hook names do not
    /// count as ownership.
    pub fn owns_hook(&self, name: &str) -> bool {
        let hooks = &self.spec().hooks;
        hooks.shared_hooks_with.is_none() && hooks.names.contains(&name)
    }

    /// Resolve the tool that owns a hook command name.
    ///
    /// Shared hook specs route to their declared owner. Antigravity, for
    /// example, lists Gemini hook names but routes them to Gemini.
    pub fn from_hook_name(name: &str) -> Option<Self> {
        integration_spec::ALL
            .iter()
            .find(|spec| spec.hooks.names.contains(&name))
            .map(|spec| spec.hooks.shared_hooks_with.unwrap_or(spec.tool))
    }

    /// True if any spec with routing ownership claims this hook name.
    pub fn is_hook_name(name: &str) -> bool {
        Self::from_hook_name(name).is_some()
    }

    // ── Hook-ops adapter ────────────────────────────────────────────────
    //
    // The four helpers below are the single source of truth for routing
    // verify/setup/remove/settings-path to the right per-tool hook module.
    // `commands/hooks.rs` iterates released hook-bearing tools through these
    // helpers so new tools only need a hooks module + a spec + a match arm,
    // not a fresh parallel block per dispatch site.
    //
    // Setup/installation error detail (codex hook-trust fallback, claude
    // diagnostic context, etc.) intentionally stays in `launcher::ensure_hooks_installed`
    // — those error shapes vary per tool and aren't suitable for a uniform trait.

    /// Verify hooks are installed for this tool. `include_permissions` controls
    /// whether the auto-approve permission block is also checked.
    pub fn verify_hooks_installed(&self, include_permissions: bool) -> bool {
        match self {
            Tool::Claude => {
                crate::hooks::claude::verify_claude_hooks_installed(None, include_permissions)
            }
            Tool::Gemini => {
                crate::hooks::gemini::verify_gemini_hooks_installed(include_permissions)
            }
            Tool::Codex => {
                crate::hooks::codex::verify_codex_hooks_installed(include_permissions)
                    && crate::hooks::codex::codex_current_feature_enabled()
            }
            Tool::OpenCode => crate::hooks::opencode::verify_opencode_plugin_installed(),
            Tool::Kilo => crate::hooks::opencode::verify_kilo_plugin_installed(),
            Tool::Antigravity => {
                crate::hooks::antigravity::verify_antigravity_hooks_installed(include_permissions)
            }
            Tool::Cursor => {
                crate::hooks::cursor::verify_cursor_hooks_installed(include_permissions)
            }
            Tool::Kimi => crate::hooks::kimi::verify_kimi_hooks_installed(include_permissions),
            Tool::Copilot => {
                crate::hooks::copilot::verify_copilot_hooks_installed(include_permissions)
            }
            Tool::Pi => crate::hooks::pi::verify_pi_plugin_installed(),
            Tool::Omp => crate::hooks::omp::verify_omp_plugin_installed(),
            Tool::Adhoc => false,
        }
    }

    /// Try to install hooks for this tool. Returns `Err(message)` on failure.
    /// `Tool::Adhoc` always errors — adhoc has no hook surface.
    pub fn try_setup_hooks(&self, include_permissions: bool) -> Result<(), String> {
        match self {
            Tool::Claude => crate::hooks::claude::try_setup_claude_hooks(include_permissions)
                .map_err(|e| e.to_string()),
            Tool::Gemini => crate::hooks::gemini::try_setup_gemini_hooks(include_permissions)
                .map_err(|e| e.to_string()),
            Tool::Codex => crate::hooks::codex::try_setup_codex_hooks(include_permissions)
                .map_err(|e| e.to_string()),
            Tool::OpenCode => match crate::hooks::opencode::install_opencode_plugin() {
                Ok(true) => Ok(()),
                Ok(false) => Err(String::new()),
                Err(e) => Err(e.to_string()),
            },
            Tool::Kilo => match crate::hooks::opencode::install_kilo_plugin() {
                Ok(true) => Ok(()),
                Ok(false) => Err(String::new()),
                Err(e) => Err(e.to_string()),
            },
            Tool::Antigravity => {
                crate::hooks::antigravity::try_setup_antigravity_hooks(include_permissions)
                    .map_err(|e| e.to_string())
            }
            Tool::Cursor => crate::hooks::cursor::try_setup_cursor_hooks(include_permissions)
                .map_err(|e| e.to_string()),
            Tool::Kimi => crate::hooks::kimi::try_setup_kimi_hooks(include_permissions)
                .map_err(|e| e.to_string()),
            Tool::Copilot => crate::hooks::copilot::try_setup_copilot_hooks(include_permissions)
                .map_err(|e| e.to_string()),
            Tool::Pi => match crate::hooks::pi::install_pi_plugin() {
                Ok(true) => Ok(()),
                Ok(false) => Err(String::new()),
                Err(e) => Err(e.to_string()),
            },
            Tool::Omp => match crate::hooks::omp::install_omp_plugin() {
                Ok(true) => Ok(()),
                Ok(false) => Err(String::new()),
                Err(e) => Err(e.to_string()),
            },
            Tool::Adhoc => Err("Adhoc has no hooks to install".to_string()),
        }
    }

    /// Remove hooks for this tool. Returns `Ok(true)` on success, `Ok(false)`
    /// if the tool reports a non-error failure, and `Err(message)` on
    /// recoverable errors that callers should display verbatim.
    pub fn remove_hooks(&self) -> Result<bool, String> {
        match self {
            Tool::Claude => Ok(crate::hooks::claude::remove_claude_hooks()),
            Tool::Gemini => Ok(crate::hooks::gemini::remove_gemini_hooks()),
            Tool::Codex => Ok(crate::hooks::codex::remove_codex_hooks()),
            Tool::OpenCode => crate::hooks::opencode::remove_opencode_plugin()
                .map(|_| true)
                .map_err(|e| e.to_string()),
            Tool::Kilo => crate::hooks::opencode::remove_kilo_plugin()
                .map(|_| true)
                .map_err(|e| e.to_string()),
            Tool::Antigravity => Ok(crate::hooks::antigravity::remove_antigravity_hooks()),
            Tool::Cursor => Ok(crate::hooks::cursor::remove_cursor_hooks()),
            Tool::Kimi => Ok(crate::hooks::kimi::remove_kimi_hooks()),
            Tool::Copilot => Ok(crate::hooks::copilot::remove_copilot_hooks()),
            Tool::Pi => crate::hooks::pi::remove_pi_plugin()
                .map(|_| true)
                .map_err(|e| e.to_string()),
            Tool::Omp => crate::hooks::omp::remove_omp_plugin()
                .map(|_| true)
                .map_err(|e| e.to_string()),
            Tool::Adhoc => Ok(false),
        }
    }

    /// Filesystem path the hook integration writes to (settings/config file or
    /// plugin location). Empty for `Tool::Adhoc`.
    pub fn hooks_settings_path(&self) -> String {
        let path_buf = match self {
            Tool::Claude => crate::hooks::claude::get_claude_settings_path(),
            Tool::Gemini => crate::hooks::gemini::get_gemini_settings_path(),
            Tool::Codex => crate::hooks::codex::get_codex_config_path(),
            Tool::OpenCode => crate::hooks::opencode::get_opencode_plugin_path(),
            Tool::Kilo => crate::hooks::opencode::get_kilo_plugin_path(),
            Tool::Antigravity => crate::hooks::antigravity::get_antigravity_hooks_path(),
            Tool::Cursor => crate::hooks::cursor::get_cursor_hooks_path(),
            Tool::Kimi => crate::hooks::kimi::get_kimi_settings_path(),
            Tool::Copilot => crate::hooks::copilot::get_copilot_hooks_path(),
            Tool::Pi => crate::hooks::pi::get_pi_plugin_path(),
            Tool::Omp => crate::hooks::omp::get_omp_plugin_path(),
            Tool::Adhoc => return String::new(),
        };
        path_buf.to_string_lossy().to_string()
    }
}

impl FromStr for Tool {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.to_lowercase();
        // Primary name match.
        if let Some(spec) = integration_spec::ALL.iter().find(|s| s.name == lower) {
            return Ok(spec.tool);
        }
        // Alias match.
        if let Some(spec) = integration_spec::ALL
            .iter()
            .find(|s| s.aliases.iter().any(|a| *a == lower))
        {
            return Ok(spec.tool);
        }
        Err(format!("Unknown tool: {}", s))
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
        // Shared hooks are owned by their spec's `shared_hooks_with` declaration.
        let mut owners = HashMap::new();
        for spec in crate::integration_spec::ALL {
            if spec.hooks.shared_hooks_with.is_some() {
                continue;
            }
            let tool = spec.tool;
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
    fn antigravity_borrows_gemini_hooks_without_owning_them() {
        assert!(Tool::Gemini.owns_hook("gemini-beforeagent"));
        assert!(!Tool::Antigravity.owns_hook("gemini-beforeagent"));
        assert_eq!(
            Tool::from_hook_name("gemini-beforeagent"),
            Some(Tool::Gemini)
        );
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
    fn copilot_from_alias() {
        assert_eq!("copilot".parse::<Tool>(), Ok(Tool::Copilot));
    }

    #[test]
    fn pi_from_str() {
        assert_eq!("pi".parse::<Tool>(), Ok(Tool::Pi));
        assert_eq!("pi-agent".parse::<Tool>(), Ok(Tool::Pi));
    }

    #[test]
    fn omp_from_str() {
        assert_eq!("omp".parse::<Tool>(), Ok(Tool::Omp));
        assert_eq!("omp-agent".parse::<Tool>(), Ok(Tool::Omp));
    }

    #[test]
    fn antigravity_shares_gemini_hooks() {
        assert_eq!(Tool::Antigravity.hooks(), Tool::Gemini.hooks());
    }

    #[test]
    fn kilo_shares_opencode_hooks() {
        assert_eq!(Tool::Kilo.hooks(), Tool::OpenCode.hooks());
        assert!(!Tool::Kilo.owns_hook("opencode-start"));
        assert_eq!(Tool::from_hook_name("opencode-start"), Some(Tool::OpenCode));
    }
}
