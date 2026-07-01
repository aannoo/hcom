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
/// 2. `$HOME/.config` — on every platform, including Windows and macOS
///
/// OpenCode and Kilo resolve their config directory via the `xdg-basedir` npm
/// package, which has no Windows- or macOS-specific branch at all: it always
/// resolves to `~/.config` (falling back to `$XDG_CONFIG_HOME` when set) on
/// every OS. There is no `%APPDATA%` or `~/Library/Application Support`
/// involved, so hcom must not special-case Windows here either.
pub(crate) fn user_config_home() -> Option<std::path::PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(std::path::PathBuf::from(xdg));
    }
    user_home().map(|h| h.join(".config"))
}

/// Cross-platform data directory for an OpenCode-family tool (`opencode`/`kilo`),
/// i.e. where the tool keeps its SQLite session DB.
///
/// Resolution order:
/// 1. `$XDG_DATA_HOME/<tool>` (explicit override, all platforms) — trusted
///    unconditionally, with no existence probe: an explicit override always
///    wins, even if the directory hasn't been created yet.
/// 2. `~/.local/share/<tool>` — on every platform, including Windows and macOS
///
/// Like [`user_config_home`], this follows OpenCode/Kilo's use of the
/// `xdg-basedir` npm package, which always resolves to `~/.local/share`
/// (falling back to `$XDG_DATA_HOME` when set) regardless of OS. There is no
/// `%APPDATA%` or Apple-style data dir involved, so `dirs::data_dir()` is
/// never a correct candidate for this tool family on any platform.
///
/// This is the single source of truth for opencode/kilo data-dir resolution,
/// shared by the hook dispatcher, the transcript search, and `resume`.
pub(crate) fn opencode_family_data_dir(tool: &str) -> Option<std::path::PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return Some(std::path::PathBuf::from(xdg).join(tool));
    }
    user_home().map(|h| h.join(".local/share").join(tool))
}

/// Resolve the SQLite database path for an OpenCode-family tool (`opencode`/`kilo`).
///
/// Builds on [`opencode_family_data_dir`]. For `kilo`, honors `KILO_DB`: `:memory:`
/// means no on-disk DB (returns `None`), an absolute path is used as-is, and a
/// relative path is joined onto the data dir; if `KILO_DB` is unset, defaults to
/// `<data_dir>/kilo.db`. Any other tool resolves to `<data_dir>/opencode.db`.
///
/// This performs path construction only — it does not check whether the
/// resulting path exists on disk. Callers that need "exists" semantics should
/// apply their own `.exists()` check.
pub(crate) fn opencode_family_db_path(tool: &str) -> Option<std::path::PathBuf> {
    let data_dir = opencode_family_data_dir(tool)?;
    if tool == "kilo" {
        if std::env::var("KILO_DB").as_deref() == Ok(":memory:") {
            return None;
        }
        return Some(
            std::env::var("KILO_DB")
                .ok()
                .filter(|value| !value.is_empty())
                .map(std::path::PathBuf::from)
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        data_dir.join(path)
                    }
                })
                .unwrap_or_else(|| data_dir.join("kilo.db")),
        );
    }
    Some(data_dir.join("opencode.db"))
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

    #[test]
    #[serial]
    fn user_home_prefers_home_env_when_set() {
        let _guard = EnvGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();

        unsafe {
            std::env::set_var("HOME", &home);
        }

        assert_eq!(super::user_home(), Some(home));
    }

    #[test]
    #[serial]
    fn user_home_falls_through_when_home_env_empty() {
        let _guard = EnvGuard::new();

        unsafe {
            std::env::set_var("HOME", "");
        }

        // Empty HOME is treated as unset, so resolution falls through to
        // dirs::home_dir() rather than returning Some(PathBuf::from("")).
        assert_eq!(super::user_home(), dirs::home_dir());
    }

    #[test]
    #[serial]
    fn user_config_home_prefers_xdg_config_home_when_set() {
        let _guard = EnvGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg-config");
        std::fs::create_dir_all(&xdg).unwrap();

        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &xdg);
        }

        assert_eq!(super::user_config_home(), Some(xdg));
    }

    #[test]
    #[serial]
    fn user_config_home_empty_xdg_falls_back_to_dot_config() {
        let _guard = EnvGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_CONFIG_HOME", "");
        }

        // Empty XDG_CONFIG_HOME is treated as unset, so resolution falls
        // back to $HOME/.config (the unix branch, given this test's cfg gate).
        assert_eq!(super::user_config_home(), Some(home.join(".config")));
    }

    /// RAII guard for XDG_DATA_HOME, which crate::hooks::test_helpers::EnvGuard
    /// does not track.
    struct XdgDataHomeGuard(Option<String>);

    impl XdgDataHomeGuard {
        fn set(value: &str) -> Self {
            let saved = std::env::var("XDG_DATA_HOME").ok();
            unsafe {
                std::env::set_var("XDG_DATA_HOME", value);
            }
            Self(saved)
        }
    }

    impl Drop for XdgDataHomeGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.0 {
                    Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                    None => std::env::remove_var("XDG_DATA_HOME"),
                }
            }
        }
    }

    #[test]
    #[serial]
    fn opencode_family_data_dir_prefers_xdg_data_home_when_it_exists() {
        let _guard = EnvGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let xdg_data = temp.path().join("xdg-data");
        let xdg_tool_dir = xdg_data.join("opencode");
        let local_share_tool_dir = home.join(".local/share/opencode");
        std::fs::create_dir_all(&xdg_tool_dir).unwrap();
        std::fs::create_dir_all(&local_share_tool_dir).unwrap();

        unsafe {
            std::env::set_var("HOME", &home);
        }
        let _xdg_guard = XdgDataHomeGuard::set(xdg_data.to_str().unwrap());

        // XDG_DATA_HOME wins even though ~/.local/share/opencode also exists.
        assert_eq!(
            super::opencode_family_data_dir("opencode"),
            Some(xdg_tool_dir)
        );
    }

    #[test]
    #[serial]
    fn opencode_family_data_dir_trusts_nonexistent_xdg_candidate() {
        let _guard = EnvGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let local_share_tool_dir = home.join(".local/share/opencode");
        std::fs::create_dir_all(&local_share_tool_dir).unwrap();

        unsafe {
            std::env::set_var("HOME", &home);
        }
        // XDG_DATA_HOME is explicitly set but its opencode subdir does not
        // exist on disk yet. An explicit override must win unconditionally
        // rather than falling back to a stale ~/.local/share/opencode.
        let xdg_data_missing = temp.path().join("xdg-data-missing");
        let _xdg_guard = XdgDataHomeGuard::set(xdg_data_missing.to_str().unwrap());

        assert_eq!(
            super::opencode_family_data_dir("opencode"),
            Some(xdg_data_missing.join("opencode"))
        );
    }

    #[test]
    #[serial]
    fn opencode_family_data_dir_empty_xdg_falls_back_to_local_share() {
        let _guard = EnvGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let local_share_tool_dir = home.join(".local/share/opencode");
        std::fs::create_dir_all(&local_share_tool_dir).unwrap();

        unsafe {
            std::env::set_var("HOME", &home);
        }
        let _xdg_guard = XdgDataHomeGuard::set("");

        // Empty XDG_DATA_HOME is treated as unset (no candidate added for it).
        assert_eq!(
            super::opencode_family_data_dir("opencode"),
            Some(local_share_tool_dir)
        );
    }

    #[test]
    #[serial]
    fn opencode_family_data_dir_returns_local_share_even_when_it_does_not_exist() {
        let _guard = EnvGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();

        unsafe {
            std::env::set_var("HOME", &home);
        }
        let _xdg_guard = XdgDataHomeGuard::set("");

        // ~/.local/share/opencode doesn't exist under this isolated HOME, but
        // it's still the only non-override candidate (OpenCode/Kilo never use
        // dirs::data_dir() on any platform), so it's returned unconditionally.
        assert_eq!(
            super::opencode_family_data_dir("opencode"),
            Some(home.join(".local/share/opencode"))
        );
    }
}
