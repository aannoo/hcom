//! Tool-specific argument parsing and launch preprocessing.
//!
//! Each AI CLI tool (Claude, Gemini, Codex, OpenCode) has its own argument
//! parsing semantics. This module provides shared infrastructure and
//! tool-specific parsers used by the launcher.

pub mod args_common;
pub mod codex_args;
pub mod cline_preprocessing;
pub mod codex_preprocessing;
pub mod gemini_args;
pub mod kilo_preprocessing;
pub mod opencode_preprocessing;
