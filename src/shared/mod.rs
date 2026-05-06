//! Shared types, constants, and utilities for hcom.

pub mod ansi;
pub mod constants;
pub mod context;
pub mod errors;
pub mod identity;
pub mod platform;
pub mod terminal_presets;
pub mod time;

// Re-export key types at module level for convenience.
pub use crate::tool::Tool;
pub use constants::{
    BIND_MARKER_RE,
    MAX_MESSAGE_SIZE,
    MAX_MESSAGES_PER_DELIVERY,
    // Patterns
    MENTION_PATTERN,
    // Message constants
    SENDER,
    // Status constants
    ST_ACTIVE,
    ST_BLOCKED,
    ST_ERROR,
    ST_INACTIVE,
    ST_LAUNCHING,
    ST_LISTENING,
    SYSTEM_SENDER,
    // Functions
    extract_mentions,
    status_bg,
    status_fg,
    status_icon,
};
pub use context::HcomContext;
pub use errors::{CLIError, HcomError, HookError};
pub use identity::{CommandContext, SenderIdentity, SenderKind};
pub use platform::{
    detect_current_tool_from_env, dev_root_binary, is_inside_ai_tool, is_termux, is_wsl,
    platform_name, shorten_path, shorten_path_max,
};
pub use terminal_presets::{TerminalPreset, get_terminal_preset};
pub use time::{format_age, now_epoch_f64, now_epoch_i64, system_time_to_epoch_f64};
