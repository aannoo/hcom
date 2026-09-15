//! CLI commands for hcom.
//!

// Messaging
pub mod listen;
pub mod send;

// Lifecycle
pub mod daemon;
pub mod fork;
pub mod kill;
pub mod launch;
pub mod resume;
pub mod start;
pub mod stop;

// Diagnostics
pub mod bundle;
pub mod events;
pub mod list;
pub mod status;
pub mod term;
pub mod transcript;

// Management
pub mod archive;
pub mod completion;
pub mod config;
pub mod help;
pub mod hooks;
pub mod relay;
pub mod reset;
pub(crate) mod reset_ops;
pub(crate) mod reset_preview;
pub mod run;
pub mod update;
