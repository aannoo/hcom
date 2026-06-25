//! Platform-abstraction layer.
//!
//! Every OS-specific primitive the rest of hcom needs lives behind this module
//! so call sites never touch `nix`, `libc`, `std::os::unix`, or Windows APIs
//! directly. Each capability is a thin, platform-neutral function API; the
//! per-OS implementation is selected inline with `#[cfg]`. This turns the
//! historically scattered `#[cfg]` blocks into a single, auditable boundary and
//! keeps Unix and Windows behavior side by side.

pub mod fs;
pub mod io;
pub mod net;
pub mod process;
pub mod signal;
