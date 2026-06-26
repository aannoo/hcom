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
