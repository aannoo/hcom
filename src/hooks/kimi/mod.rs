//! Kimi Code CLI hook handlers and config.toml management.
//!
//! Kimi hooks are declared in `~/.kimi-code/config.toml` under `[[hooks]]` array
//! tables. Each hook receives JSON on stdin and uses exit code / stdout for
//! results (0 = allow, 2 = block).

mod config;
mod handlers;

#[cfg(test)]
mod tests;

pub use config::{
    SetupError, get_kimi_settings_path, remove_kimi_hooks, try_setup_kimi_hooks,
    verify_kimi_hooks_installed,
};
pub use handlers::{derive_kimi_transcript_path, dispatch_kimi_hook};

#[cfg(test)]
pub(crate) use config::{
    HOOK_TIMEOUT_SECS, KIMI_HOOK_COMMANDS, build_kimi_hook_command, is_hcom_kimi_command,
    kimi_permission_patterns, merge_hcom_hooks, merge_hcom_permissions, remove_hcom_permissions,
};
#[cfg(test)]
pub(crate) use handlers::{get_handler, handle_sessionend, handle_stop};
