//! Platform detection utilities.
//!

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Cached WSL detection result.
static IS_WSL: LazyLock<bool> = LazyLock::new(detect_wsl);

/// Cached Termux detection result.
static IS_TERMUX: LazyLock<bool> = LazyLock::new(detect_termux);

/// Whether running in WSL (Windows Subsystem for Linux).
pub fn is_wsl() -> bool {
    *IS_WSL
}

/// Whether running in Termux on Android.
pub fn is_termux() -> bool {
    *IS_TERMUX
}

/// Detect WSL by checking /proc/version for "microsoft".
fn detect_wsl() -> bool {
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/version")
            .map(|content| content.to_lowercase().contains("microsoft"))
            .unwrap_or(false)
    }
}

/// Detect Termux by checking env vars and paths.
fn detect_termux() -> bool {
    use std::env;
    use std::path::Path;

    // Primary: TERMUX_VERSION (works all versions)
    if env::var("TERMUX_VERSION").is_ok() {
        return true;
    }
    // Modern: TERMUX__ROOTFS (v0.119.0+)
    if env::var("TERMUX__ROOTFS").is_ok() {
        return true;
    }
    // Fallback: path check
    if Path::new("/data/data/com.termux").exists() {
        return true;
    }
    // Fallback: PREFIX check
    if let Ok(prefix) = env::var("PREFIX") {
        if prefix.contains("com.termux") {
            return true;
        }
    }
    false
}

/// Default Termux node path for shebang bypass.
pub const TERMUX_NODE_PATH: &str = "/data/data/com.termux/files/usr/bin/node";

/// Replace HOME prefix with ~ for display.
pub fn shorten_path(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if path == home {
            return "~".to_string();
        }
        if let Some(rest) = path.strip_prefix(home.as_str()) {
            if rest.starts_with('/') {
                return format!("~{rest}");
            }
        }
    }
    path.to_string()
}

/// Shorten path and truncate to max_width, keeping the trailing portion visible.
///
/// Example: "/very/long/path/to/some/project" with max 30 → ".../path/to/some/project"
pub fn shorten_path_max(path: &str, max_width: usize) -> String {
    let shortened = shorten_path(path);
    if shortened.len() <= max_width || max_width < 4 {
        return shortened;
    }
    // Keep the rightmost portion (more useful than left)
    let keep = max_width - 3; // "..."
    let start = shortened.len() - keep;
    // Find a char boundary (UTF-8 safe)
    let start = (start..shortened.len())
        .find(|&i| shortened.is_char_boundary(i))
        .unwrap_or(shortened.len());
    // Find a clean break at a '/' if possible
    if let Some(slash) = shortened[start..].find('/') {
        let pos = start + slash;
        if shortened.len() - pos <= max_width - 3 {
            return format!("...{}", &shortened[pos..]);
        }
    }
    format!("...{}", &shortened[start..])
}

/// Whether running inside any AI tool (env-var check, no HcomContext needed).
///
/// Uses the same env vars as HcomContext::from_env() for tool detection.
/// For code that already has an HcomContext, prefer `ctx.is_inside_ai_tool()`.
pub fn is_inside_ai_tool() -> bool {
    use std::env;
    let is_set = |k: &str| env::var(k).is_ok();
    let is_eq = |k: &str, v: &str| env::var(k).ok().as_deref() == Some(v);
    let nonempty = |k: &str| env::var(k).ok().filter(|v| !v.is_empty()).is_some();
    // Claude (matches HcomContext: CLAUDECODE=1 || CLAUDE_ENV_FILE non-empty)
    is_eq("CLAUDECODE", "1") || nonempty("CLAUDE_ENV_FILE")
        // Gemini
        || is_eq("GEMINI_CLI", "1")
        // Codex (all 5 markers from HcomContext)
        || is_set("CODEX_SANDBOX")
        || is_set("CODEX_SANDBOX_NETWORK_DISABLED")
        || is_set("CODEX_MANAGED_BY_NPM")
        || is_set("CODEX_MANAGED_BY_BUN")
        || is_set("CODEX_THREAD_ID")
        // OpenCode
        || is_eq("OPENCODE", "1")
        // hcom-launched
        || is_eq("HCOM_LAUNCHED", "1")
}

/// Detect current AI tool from environment (no HcomContext needed).
///
/// Uses the same env vars as HcomContext::from_env() for tool detection.
pub fn detect_current_tool_from_env() -> &'static str {
    use std::env;
    let is_set = |k: &str| env::var(k).is_ok();
    let is_eq = |k: &str, v: &str| env::var(k).ok().as_deref() == Some(v);
    let nonempty = |k: &str| env::var(k).ok().filter(|v| !v.is_empty()).is_some();
    if is_eq("CLAUDECODE", "1") || nonempty("CLAUDE_ENV_FILE") {
        "claude"
    } else if is_eq("GEMINI_CLI", "1") {
        "gemini"
    } else if is_set("CODEX_SANDBOX")
        || is_set("CODEX_SANDBOX_NETWORK_DISABLED")
        || is_set("CODEX_MANAGED_BY_NPM")
        || is_set("CODEX_MANAGED_BY_BUN")
        || is_set("CODEX_THREAD_ID")
    {
        "codex"
    } else if is_eq("OPENCODE", "1") {
        "opencode"
    } else {
        "adhoc"
    }
}

fn resolve_target_dir_path(base: &Path, value: &str) -> Option<PathBuf> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    let path = PathBuf::from(trimmed);
    Some(if path.is_absolute() {
        path
    } else {
        base.join(path)
    })
}

fn cargo_target_dir_from_config(dev_root: &Path) -> Option<PathBuf> {
    for rel in [".cargo/config.toml", ".cargo/config"] {
        let path = dev_root.join(rel);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(parsed) = content.parse::<toml::Table>() else {
            continue;
        };

        if let Some(target_dir) = parsed
            .get("build")
            .and_then(|build| build.get("target-dir"))
            .and_then(|value| value.as_str())
            .and_then(|value| resolve_target_dir_path(dev_root, value))
        {
            return Some(target_dir);
        }
    }

    None
}

fn cargo_target_dir(dev_root: &Path) -> PathBuf {
    std::env::var("CARGO_TARGET_DIR")
        .ok()
        .and_then(|value| resolve_target_dir_path(dev_root, &value))
        .or_else(|| cargo_target_dir_from_config(dev_root))
        .unwrap_or_else(|| dev_root.join("target"))
}

/// Returns the best available hcom binary under `dev_root`'s cargo target dir.
///
/// Target dir resolution order:
/// 1. `CARGO_TARGET_DIR`
/// 2. `.cargo/config.toml` or `.cargo/config` `build.target-dir`
/// 3. `target/`
///
/// Prefers whichever of `release/hcom` and `debug/hcom` was modified more
/// recently, so both `cargo build` and `cargo build --release` do the right
/// thing. Falls back to debug if mtimes are unavailable. Returns `None` if
/// neither binary exists.
pub fn dev_root_binary(dev_root: &Path) -> Option<PathBuf> {
    let target_dir = cargo_target_dir(dev_root);
    let release = target_dir.join("release/hcom");
    let debug = target_dir.join("debug/hcom");

    let mtime = |p: &Path| std::fs::metadata(p).ok().and_then(|m| m.modified().ok());

    match (mtime(&release), mtime(&debug)) {
        (Some(r), Some(d)) => Some(if d >= r { debug } else { release }),
        (Some(_), None) => Some(release),
        (None, Some(_)) => Some(debug),
        (None, None) => None,
    }
}

/// Current platform name
pub fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "Darwin"
    } else if cfg!(target_os = "linux") || cfg!(target_os = "android") {
        "Linux"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    struct CargoTargetDirGuard {
        saved: Option<String>,
    }

    impl CargoTargetDirGuard {
        fn new() -> Self {
            Self {
                saved: std::env::var("CARGO_TARGET_DIR").ok(),
            }
        }
    }

    impl Drop for CargoTargetDirGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.saved {
                    Some(v) => std::env::set_var("CARGO_TARGET_DIR", v),
                    None => std::env::remove_var("CARGO_TARGET_DIR"),
                }
            }
        }
    }

    fn touch_binary(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"bin").unwrap();
    }

    #[test]
    fn test_platform_name_is_known() {
        let name = platform_name();
        assert!(
            ["Darwin", "Linux", "Windows"].contains(&name),
            "unexpected platform: {name}"
        );
    }

    #[test]
    fn test_is_wsl_returns_bool() {
        // Just ensure it doesn't panic.
        let _ = is_wsl();
    }

    #[test]
    fn test_is_termux_returns_bool() {
        let _ = is_termux();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_not_wsl_on_macos() {
        assert!(!is_wsl());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_platform_is_darwin() {
        assert_eq!(platform_name(), "Darwin");
    }

    #[test]
    #[serial]
    fn test_dev_root_binary_uses_default_target_dir() {
        let _target_guard = CargoTargetDirGuard::new();
        let dir = TempDir::new().unwrap();
        let release = dir.path().join("target/release/hcom");
        touch_binary(&release);

        unsafe {
            std::env::remove_var("CARGO_TARGET_DIR");
        }

        assert_eq!(dev_root_binary(dir.path()), Some(release));
    }

    #[test]
    #[serial]
    fn test_dev_root_binary_uses_target_dir_from_env() {
        let _target_guard = CargoTargetDirGuard::new();
        let dir = TempDir::new().unwrap();
        let custom_target = dir.path().join(".cargo-target");
        let debug = custom_target.join("debug/hcom");
        touch_binary(&debug);

        unsafe {
            std::env::set_var("CARGO_TARGET_DIR", ".cargo-target");
        }

        assert_eq!(dev_root_binary(dir.path()), Some(debug));
    }

    #[test]
    #[serial]
    fn test_dev_root_binary_uses_target_dir_from_cargo_config() {
        let _target_guard = CargoTargetDirGuard::new();
        let dir = TempDir::new().unwrap();
        let cargo_dir = dir.path().join(".cargo");
        let custom_target = dir.path().join(".hcom-build");
        let release = custom_target.join("release/hcom");
        touch_binary(&release);
        std::fs::create_dir_all(&cargo_dir).unwrap();
        std::fs::write(
            cargo_dir.join("config.toml"),
            "[build]\ntarget-dir = \".hcom-build\"\n",
        )
        .unwrap();

        unsafe {
            std::env::remove_var("CARGO_TARGET_DIR");
        }

        assert_eq!(dev_root_binary(dir.path()), Some(release));
    }
}
