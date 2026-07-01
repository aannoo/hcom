//! Shared runtime helpers for invoking hcom and locating tool config roots.

/// Cached hcom invocation prefix (computed once per process lifetime).
static HCOM_PREFIX: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
    if std::env::var("HCOM_DEV_ROOT").is_ok() {
        return vec!["hcom".into()];
    }

    if let Ok(exe) = std::env::current_exe()
        && let Ok(resolved) = exe.canonicalize()
    {
        let has_uv = resolved.components().any(|c| c.as_os_str() == "uv");
        if has_uv {
            return vec!["uvx".into(), "hcom".into()];
        }
    }

    vec!["hcom".into()]
});

/// Detect hcom invocation prefix based on execution context.
pub(crate) fn get_hcom_prefix() -> Vec<String> {
    HCOM_PREFIX.clone()
}

/// Get the base directory for tool config files (e.g. .codex/, .gemini/).
pub(crate) fn tool_config_root() -> std::path::PathBuf {
    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let (hcom_dir, _) = crate::paths::resolve_hcom_dir_from_env(&env, &cwd);
    hcom_dir
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default())
}

/// Build hcom command string for prompts, config, and hook commands.
pub(crate) fn build_hcom_command() -> String {
    get_hcom_prefix().join(" ")
}

/// Gemini / Antigravity shared config directory (`~/.gemini` or under `GEMINI_CLI_HOME`).
pub(crate) fn gemini_family_config_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("GEMINI_CLI_HOME")
        && !dir.is_empty()
    {
        return std::path::PathBuf::from(dir).join(".gemini");
    }
    tool_config_root().join(".gemini")
}

/// User home directory, honoring an explicit `HOME` override before falling back
/// to the platform default (`dirs::home_dir()` resolves `%USERPROFILE%` on Windows).
pub(crate) fn user_home() -> Option<std::path::PathBuf> {
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(std::path::PathBuf::from(home));
    }
    dirs::home_dir()
}

/// Cross-platform user config base directory.
///
/// Resolution order:
/// 1. `$XDG_CONFIG_HOME` (explicit override, all platforms)
/// 2. Unix/macOS: `$HOME/.config` — the XDG-style location the OpenCode-family
///    and Gemini Node CLIs use, including on macOS
/// 3. Windows: `dirs::config_dir()` (`%APPDATA%`)
///
/// Unix/macOS behavior is identical to the previous `xdg_config_home()`; only
/// the Windows fallback is new.
pub(crate) fn user_config_home() -> Option<std::path::PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(std::path::PathBuf::from(xdg));
    }
    #[cfg(windows)]
    {
        dirs::config_dir()
    }
    #[cfg(not(windows))]
    {
        user_home().map(|h| h.join(".config"))
    }
}

/// Cross-platform data directory for an OpenCode-family tool (`opencode`/`kilo`),
/// i.e. where the tool keeps its SQLite session DB.
///
/// Probes candidates in order and returns the first that exists on disk; if none
/// exist, returns the last candidate (platform default) so callers can surface a
/// useful "searched here" path.
///
/// Resolution order:
/// 1. `$XDG_DATA_HOME/<tool>` (explicit override, all platforms)
/// 2. `~/.local/share/<tool>` (Linux + macOS XDG-style — where these CLIs write)
/// 3. `dirs::data_dir()/<tool>` (Windows `%APPDATA%`, macOS Apple-style fallback)
///
/// This is the single source of truth for opencode/kilo data-dir resolution,
/// shared by the hook dispatcher, the transcript search, and `resume`.
pub(crate) fn opencode_family_data_dir(tool: &str) -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        candidates.push(std::path::PathBuf::from(xdg).join(tool));
    }
    if let Some(home) = user_home() {
        candidates.push(home.join(".local/share").join(tool));
    }
    if let Some(data) = dirs::data_dir() {
        candidates.push(data.join(tool));
    }

    if let Some(existing) = candidates.iter().find(|c| c.is_dir()) {
        return Some(existing.clone());
    }
    candidates.into_iter().next_back()
}

/// Set terminal title via escape codes written to /dev/tty.
pub(crate) fn set_terminal_title(instance_name: &str) {
    let title = format!("hcom: {}", instance_name);
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        use std::io::Write;
        let _ = write!(tty, "\x1b]1;{}\x07\x1b]2;{}\x07", title, title);
    }
}

// Unix-only: these assert $HOME resolution and POSIX path canonicalization
// (Windows resolves USERPROFILE and prefixes canonical paths with \\?\).
#[cfg(all(test, unix))]
mod tests {
    use crate::hooks::test_helpers::EnvGuard;
    use serial_test::serial;

    #[test]
    #[serial]
    fn tool_config_root_uses_home_when_hcom_dir_has_no_parent() {
        let _guard = EnvGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("HCOM_DIR", "/");
        }

        assert_eq!(super::tool_config_root(), home);
    }

    #[test]
    #[serial]
    fn tool_config_root_uses_parent_of_resolved_hcom_dir() {
        let _guard = EnvGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let home = temp.path().join("home");
        let sandbox = workspace.join(".sandbox");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&sandbox).unwrap();

        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&workspace).unwrap();
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("HCOM_DIR", ".sandbox/.hcom");
        }

        let root = super::tool_config_root();
        let expected = sandbox.canonicalize().unwrap();

        std::env::set_current_dir(prev_cwd).unwrap();
        assert_eq!(root, expected);
    }
}
