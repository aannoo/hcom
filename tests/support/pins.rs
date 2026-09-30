//! Exact versions of the real CLIs the real-tool tests drive, read from
//! `scripts/mock-tools.pins` — the same file the install scripts and the CI
//! cache key use, so a bump is a one-line change there.

const PINS: &str = include_str!("../../scripts/mock-tools.pins");

/// The pinned version of `package` (e.g. `@openai/codex`).
pub fn pinned_version(package: &str) -> &'static str {
    PINS.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .find_map(|line| line.strip_prefix(package)?.strip_prefix('@'))
        .unwrap_or_else(|| panic!("no pin for {package} in scripts/mock-tools.pins"))
}

/// Command that installs every pin into the prefix the tests put on PATH.
pub const INSTALL_HINT: &str = "just mock-tools (or scripts/install-mock-tools.sh)";
