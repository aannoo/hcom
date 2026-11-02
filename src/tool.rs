//! Tool enum for type-safe tool identification across hcom.
//!
//! Per-tool data (hook names, ready pattern, delivery gates, help, status
//! mappings, etc.) lives in [`crate::integration_spec`]. This module just
//! defines the enum and a thin set of forwarders.

use std::str::FromStr;

use crate::integration_spec;

/// Supported AI coding tools
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    Qoder,
    Grok,
    Pi,
    Omp,
    Hermes,
    Adhoc,
}

impl Tool {
    /// On-screen markers for PTY readiness detection (any one matches).
    pub fn ready_patterns(&self) -> &'static [&'static str] {
        self.spec().ready_patterns
    }

    /// Lowercase tool name used in DB, CLI output, and external interfaces.
    pub fn as_str(&self) -> &'static str {
        self.spec().name
    }

    /// Hook command names listed for this tool. Some tools borrow another
    /// tool's names; use `from_hook_name` for routing ownership.
    pub fn hooks(&self) -> &'static [&'static str] {
        self.spec().hooks.names
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
    // Setup/installation error detail for persistent tools intentionally stays
    // in `launcher::ensure_hooks_installed`; per-run tools report through
    // `hooks::runtime::plan`.

    /// Verify a persistent tool's global hook install. `include_permissions`
    /// controls whether the auto-approve permission block is also checked.
    /// Per-run tools (`hooks::runtime::is_per_run`) have no install; callers
    /// must branch on that first.
    pub fn verify_hooks_installed(&self, include_permissions: bool) -> bool {
        match self.hook_installer() {
            Some(ops) => (ops.verify)(include_permissions),
            None if *self == Tool::Adhoc => false,
            None => per_run_has_no_install(*self),
        }
    }

    /// Install a persistent tool's global hooks. Returns `Err(message)` on
    /// failure. Per-run tools have nothing to install (see above).
    /// `Tool::Adhoc` always errors — adhoc has no hook surface.
    pub fn try_setup_hooks(&self, include_permissions: bool) -> Result<(), String> {
        match self.hook_installer() {
            Some(ops) => (ops.setup)(include_permissions),
            None if *self == Tool::Adhoc => Err("Adhoc has no hooks to install".to_string()),
            None => per_run_has_no_install(*self),
        }
    }

    /// Remove hooks for this tool. `Ok(true)` when something was removed,
    /// `Ok(false)` when nothing was there, `Err(message)` on failure (an empty
    /// message when the tool only logged the detail).
    pub fn remove_hooks(&self) -> Result<bool, String> {
        // Persistent removers only report success, so infer "removed" from
        // the install state beforehand.
        let persistent = |installed: bool, remove: fn() -> bool| {
            if remove() {
                Ok(installed)
            } else {
                Err(String::new())
            }
        };
        let installed = || self.verify_hooks_installed(false);
        match self {
            Tool::Claude => {
                crate::hooks::claude::remove_claude_hooks().map_err(|e| format!("{e:#}"))
            }
            Tool::Codex => crate::hooks::codex::remove_codex_hooks().map_err(|e| format!("{e:#}")),
            Tool::Copilot => {
                crate::hooks::copilot::remove_copilot_hooks().map_err(|e| format!("{e:#}"))
            }
            Tool::OpenCode => {
                crate::hooks::opencode::remove_opencode_plugin().map_err(|e| e.to_string())
            }
            Tool::Kilo => crate::hooks::opencode::remove_kilo_plugin().map_err(|e| e.to_string()),
            Tool::Pi => crate::hooks::pi::remove_pi_plugin().map_err(|e| e.to_string()),
            Tool::Omp => crate::hooks::omp::remove_omp_plugin().map_err(|e| format!("{e:#}")),
            Tool::Hermes => {
                crate::hooks::hermes::remove_hermes_plugin().map_err(|e| format!("{e:#}"))
            }
            // Per-run from the start: never installed anything globally.
            Tool::Qoder | Tool::Grok => Ok(false),
            Tool::Gemini => persistent(installed(), crate::hooks::gemini::remove_gemini_hooks),
            Tool::Antigravity => persistent(
                installed(),
                crate::hooks::antigravity::remove_antigravity_hooks,
            ),
            Tool::Cursor => persistent(installed(), crate::hooks::cursor::remove_cursor_hooks),
            Tool::Kimi => persistent(installed(), crate::hooks::kimi::remove_kimi_hooks),
            Tool::Adhoc => Err(String::new()),
        }
    }

    /// Filesystem path a persistent tool's global install writes to (settings
    /// file or plugin location). Empty for `Tool::Adhoc`. Per-run tools have
    /// none (see above).
    pub fn hooks_settings_path(&self) -> String {
        match self.hook_installer() {
            Some(ops) => (ops.settings_path)().to_string_lossy().to_string(),
            None if *self == Tool::Adhoc => String::new(),
            None => per_run_has_no_install(*self),
        }
    }

    /// Persistent hook operations are registered together so verify, setup and
    /// diagnostics cannot disagree about which tools have a global install.
    pub(crate) fn hook_installer(&self) -> Option<HookInstaller> {
        use crate::hooks::{antigravity, cursor, gemini, hermes, kimi};
        Some(match self {
            Tool::Gemini => HookInstaller {
                verify: gemini::verify_gemini_hooks_installed,
                setup: |p| gemini::try_setup_gemini_hooks(p).map_err(|e| e.to_string()),
                settings_path: gemini::get_gemini_settings_path,
            },
            Tool::Antigravity => HookInstaller {
                verify: antigravity::verify_antigravity_hooks_installed,
                setup: |p| antigravity::try_setup_antigravity_hooks(p).map_err(|e| e.to_string()),
                settings_path: antigravity::get_antigravity_hooks_path,
            },
            Tool::Cursor => HookInstaller {
                verify: cursor::verify_cursor_hooks_installed,
                setup: |p| cursor::try_setup_cursor_hooks(p).map_err(|e| e.to_string()),
                settings_path: cursor::get_cursor_hooks_path,
            },
            Tool::Kimi => HookInstaller {
                verify: kimi::verify_kimi_hooks_installed,
                setup: |p| kimi::try_setup_kimi_hooks(p).map_err(|e| e.to_string()),
                settings_path: kimi::get_kimi_settings_path,
            },
            // Hermes has its own approval system; hcom grants no permissions.
            Tool::Hermes => HookInstaller {
                verify: |_| hermes::verify_hermes_plugin_installed(),
                setup: |_| hermes::try_setup_hermes_plugin().map_err(|e| format!("{e:#}")),
                settings_path: hermes::get_hermes_plugin_path,
            },
            _ => return None,
        })
    }
}

pub(crate) struct HookInstaller {
    verify: fn(bool) -> bool,
    setup: fn(bool) -> Result<(), String>,
    settings_path: fn() -> std::path::PathBuf,
}

/// Per-run tools load hooks per launch (`hooks::runtime`) and have no global
/// install to verify, set up or locate; callers branch on `is_per_run` first.
fn per_run_has_no_install(tool: Tool) -> ! {
    unreachable!(
        "{} uses per-run hooks and has no persistent install",
        tool.as_str()
    )
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
        assert_eq!(Tool::Antigravity.ready_patterns(), ["? for shortcuts"]);
    }

    #[test]
    fn copilot_from_alias() {
        assert_eq!("copilot".parse::<Tool>(), Ok(Tool::Copilot));
    }

    #[test]
    fn qoder_from_name_and_alias() {
        assert_eq!("qoder".parse::<Tool>(), Ok(Tool::Qoder));
        assert_eq!("qodercli".parse::<Tool>(), Ok(Tool::Qoder));
        assert_eq!(
            Tool::from_hook_name("qoder-sessionstart"),
            Some(Tool::Qoder)
        );
    }

    #[test]
    fn pi_from_str() {
        assert_eq!("pi".parse::<Tool>(), Ok(Tool::Pi));
        assert_eq!("pi-agent".parse::<Tool>(), Ok(Tool::Pi));
    }

    #[test]
    fn hermes_from_str() {
        assert_eq!("hermes".parse::<Tool>(), Ok(Tool::Hermes));
        assert_eq!(Tool::from_hook_name("hermes-start"), Some(Tool::Hermes));
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
        assert_eq!(Tool::from_hook_name("opencode-start"), Some(Tool::OpenCode));
    }
}
