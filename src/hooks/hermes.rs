//! Hermes hook integration.
//!
//! Hermes ACP has no hcom hook bridge: the ACP delivery loop replaces hook
//! delivery by pulling pending messages directly and injecting them over
//! JSON-RPC (`session/prompt`). These stubs keep the Tool hook-ops adapter
//! total (verify/setup/remove/settings-path) without exposing a dead hook
//! surface: `hook_tools()` already filters on `spec.hooks.names.is_empty()`,
//! so Hermes is invisible to `hcom hooks` status/add/remove.

use std::path::PathBuf;

/// Hermes does not participate in the hook system, so verification trivially
/// passes (there is nothing to verify).
pub fn verify_hermes_hooks_installed(_include_permissions: bool) -> bool {
    true
}

/// Hermes has no hooks to install.
pub fn try_setup_hermes_hooks(_include_permissions: bool) -> anyhow::Result<()> {
    Ok(())
}

/// Hermes has no hooks to remove.
pub fn remove_hermes_hooks() -> bool {
    true
}

/// Hermes config root: HERMES_HOME if set (the launcher surfaces it), else
/// `~/.hermes` (the platform-native default).
fn hermes_config_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("HERMES_HOME")
        && !dir.is_empty()
    {
        return std::path::PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".hermes")
}

/// Path the hook integration would write to. Hermes keeps no hcom hook files;
/// return the hooks dir under HERMES_HOME for symmetry with other tools.
pub fn get_hermes_hooks_path() -> PathBuf {
    hermes_config_dir().join("hooks")
}
