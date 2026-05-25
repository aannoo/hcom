//! Codex launch preprocessing — sandbox flags, DB access, bootstrap injection.

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{Result, bail};

use crate::paths;

use super::codex_args::{merge_codex_args, resolve_codex_args};

const BYPASS_HOOK_TRUST_FLAG: &str = "--dangerously-bypass-hook-trust";
const BYPASS_HOOK_TRUST_MIN_VERSION: (u64, u64, u64) = (0, 131, 0);

/// Sandbox modes aligned with Codex TUI presets.
///
/// - `workspace`: Default — --sandbox workspace-write (interactive: on-request approvals)
/// - `untrusted`: Workspace writes, approval before untrusted commands
/// - `danger-full-access`: Full Access — --dangerously-bypass-approvals-and-sandbox
/// - `none`: Raw codex, user's own settings (hcom may not work)
///
/// Codex 0.128.0 removed `--full-auto` from the TUI (it was sugar for
/// workspace-write + on-failure approvals). The current shape — --sandbox
/// workspace-write with default on-request approvals — matches the prior
/// behavior closely enough for the TUI flow.
pub fn get_sandbox_flags(mode: &str) -> Vec<String> {
    // Seatbelt blocks Unix sockets by default, breaking tmux/kitty terminal launches.
    // network_access=true adds (allow system-socket) to the seatbelt profile.
    let net = vec![
        "-c".to_string(),
        "sandbox_workspace_write.network_access=true".to_string(),
    ];

    match mode {
        "workspace" => {
            let mut flags = vec!["--sandbox".to_string(), "workspace-write".to_string()];
            flags.extend(net);
            flags
        }
        "untrusted" => {
            // Read-only-equivalent UX for hcom: codex's actual read-only sandbox
            // can't be used (hcom needs DB writes), so we keep workspace-write FS
            // and gate every non-safe command on user approval via -a untrusted.
            let mut flags = vec![
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "-a".to_string(),
                "untrusted".to_string(),
            ];
            flags.extend(net);
            flags
        }
        "danger-full-access" => {
            vec!["--dangerously-bypass-approvals-and-sandbox".to_string()]
        }
        "none" => vec![],
        // Default to workspace
        _ => {
            let mut flags = vec!["--sandbox".to_string(), "workspace-write".to_string()];
            flags.extend(net);
            flags
        }
    }
}

/// Ensure --add-dir ~/.hcom is present so hcom can write to its DB.
///
/// Codex's --add-dir flag is IGNORED in read-only sandbox mode, but required
/// for workspace-write mode to allow hcom DB writes.
///
/// If no sandbox flags are present (mode="none"), skip adding --add-dir
/// since user is using codex's own folder settings.
pub fn ensure_hcom_writable(tokens: &[String]) -> Vec<String> {
    let spec = resolve_codex_args(Some(tokens), None);

    // If no sandbox flags, assume mode="none" — skip --add-dir
    let has_sandbox = spec.has_flag(
        &[
            "--sandbox",
            "-s",
            "--dangerously-bypass-approvals-and-sandbox",
            "--full-auto",
            "--yolo",
        ],
        &["--sandbox=", "-s="],
    );
    if !has_sandbox {
        return tokens.to_vec();
    }

    let hcom_dir = paths::hcom_dir().to_string_lossy().to_string();

    // Check if --add-dir with hcom path already exists
    for (i, token) in spec.clean_tokens.iter().enumerate() {
        if token == "--add-dir"
            && i + 1 < spec.clean_tokens.len()
            && spec.clean_tokens[i + 1] == hcom_dir
        {
            return tokens.to_vec(); // Already present
        }
    }

    let add_dir_tokens = vec!["--add-dir".to_string(), hcom_dir];
    let add_dir_spec = resolve_codex_args(Some(&add_dir_tokens), None);
    merge_codex_args(&add_dir_spec, &spec).rebuild_tokens(true, true)
}

fn parse_codex_cli_version(output: &str) -> Option<(u64, u64, u64)> {
    output
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .filter_map(|token| {
            let mut parts = token.split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next()?.parse().ok()?;
            let patch = parts.next()?.parse().ok()?;
            Some((major, minor, patch))
        })
        .next_back()
}

fn codex_supports_bypass_hook_trust() -> bool {
    if let Ok(version) = std::env::var("HCOM_TEST_CODEX_CLI_VERSION") {
        return parse_codex_cli_version(&version)
            .is_some_and(|version| version >= BYPASS_HOOK_TRUST_MIN_VERSION);
    }

    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let output = match std::process::Command::new("codex")
            .arg("--version")
            .output()
        {
            Ok(output) => output,
            Err(e) => {
                crate::log::log_warn(
                    "codex",
                    "codex.version_failed",
                    &format!(
                        "could not run codex --version; skipping {BYPASS_HOOK_TRUST_FLAG}: {e}"
                    ),
                );
                return false;
            }
        };
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        parse_codex_cli_version(&text)
            .is_some_and(|version| version >= BYPASS_HOOK_TRUST_MIN_VERSION)
    })
}

/// Resolve `CODEX_HOME` the same way Codex itself does: env var if set and
/// non-empty, otherwise `~/.codex`.
fn resolve_codex_home() -> Option<(PathBuf, bool)> {
    if let Ok(val) = std::env::var("CODEX_HOME")
        && !val.is_empty()
    {
        return Some((PathBuf::from(val), true));
    }
    dirs::home_dir().map(|h| (h.join(".codex"), false))
}

/// Probe whether `CODEX_HOME` is writable before launching codex.
///
/// When hcom is invoked from inside a sandboxed parent codex (e.g.
/// `--sandbox workspace-write`), seatbelt/landlock is inherited by the entire
/// process chain. The child codex then fails to init its state DB
/// (SQLITE_READONLY) and hangs on an interactive "Repair Codex local data
/// now? [y/N]:" prompt with no human to answer.
///
/// Catching this synchronously and exiting non-zero with a permission-denied
/// message lets the parent codex's existing sandbox-escalation flow ("approve
/// to run unsandboxed?") trigger naturally on the failed shell command,
/// instead of leaving a brick agent behind.
pub fn ensure_codex_home_writable() -> Result<()> {
    let Some((codex_home, explicit_env)) = resolve_codex_home() else {
        return Ok(());
    };
    let probe_dir = if codex_home.exists() {
        codex_home.as_path()
    } else if explicit_env {
        return Ok(());
    } else {
        let Some(parent) = codex_home.ancestors().find(|p| p.exists()) else {
            return Ok(());
        };
        parent
    };
    let probe = probe_dir.join(".hcom_writable_probe");
    match std::fs::write(&probe, b"") {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => {
            use std::io::ErrorKind;
            let denied = matches!(
                e.kind(),
                ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
            );
            if !denied {
                return Ok(());
            }
            bail!(
                "Operation not permitted: cannot write to CODEX_HOME ({}): {}\n\
                 The current process is running inside a sandbox that denies writes \
                 to the codex state directory. If this hcom command was invoked by \
                 a sandboxed agent (e.g. codex --sandbox workspace-write), approve \
                 it to run unsandboxed and retry.",
                codex_home.display(),
                e
            );
        }
    }
}

/// Add Codex's runtime hook-trust bypass when supported.
///
/// hcom installs native Codex hooks automatically, but Codex 0.131.0+ also
/// requires unmanaged hooks to be trusted before they run. hcom normally writes
/// exact trust state for its own hooks; the bypass flag is only a launch-time
/// fallback when that state is missing and self-heal fails.
pub fn add_hook_trust_bypass_if_supported(codex_args: &[String]) -> Vec<String> {
    if !codex_supports_bypass_hook_trust() {
        return codex_args.to_vec();
    }

    // This is the launch-time guardrail. Cheap status/verify paths only inspect
    // local metadata, but before opening Codex we ask Codex for authoritative
    // currentHash values and rewrite hcom's trust entries if needed.
    match crate::hooks::codex::ensure_codex_hcom_hooks_trusted() {
        Ok(()) if crate::hooks::codex::codex_hcom_hooks_trusted_locally() => {
            return codex_args.to_vec();
        }
        Ok(()) => crate::log::log_warn(
            "codex",
            "codex.hook_trust_self_heal_incomplete",
            "Codex hook trust self-heal completed but trusted state still looks incomplete; falling back to hook-trust bypass",
        ),
        Err(e) => crate::log::log_warn(
            "codex",
            "codex.hook_trust_self_heal_failed",
            &format!("Codex hook trust self-heal failed; falling back to hook-trust bypass: {e}"),
        ),
    }

    // Codex's bypass flag is invocation-wide for unmanaged hooks, not scoped
    // to hcom's hooks. Prefer exact trust state and use this only as fallback.
    let bypass_flag = vec![BYPASS_HOOK_TRUST_FLAG.to_string()];
    let bypass_spec = resolve_codex_args(Some(&bypass_flag), None);
    let cli_spec = resolve_codex_args(Some(codex_args), None);
    merge_codex_args(&bypass_spec, &cli_spec).rebuild_tokens(true, true)
}

/// Add hcom bootstrap to codex developer_instructions.
///
/// Builds full bootstrap and adds via `-c developer_instructions=...` flag.
/// If user also provided developer_instructions, bootstrap comes first,
/// then separator, then user content.
///
pub fn add_codex_developer_instructions(
    codex_args: &[String],
    bootstrap_text: &str,
) -> Vec<String> {
    let spec = resolve_codex_args(Some(codex_args), None);

    // Check if developer_instructions already exists in -c flags
    let mut existing_dev_instructions: Option<String> = None;
    let mut skip_indexes = std::collections::HashSet::new();

    let mut i = 0;
    while i < spec.clean_tokens.len() {
        let token = &spec.clean_tokens[i];
        // Handle -c=developer_instructions=value or --config=developer_instructions=value
        if token.starts_with("-c=developer_instructions=")
            || token.starts_with("--config=developer_instructions=")
        {
            let eq_count = token.matches('=').count();
            existing_dev_instructions = Some(if eq_count >= 2 {
                token.splitn(3, '=').nth(2).unwrap_or("").to_string()
            } else {
                String::new()
            });
            skip_indexes.insert(i);
            break;
        }
        // Handle -c developer_instructions=value (space syntax)
        if (token == "-c" || token == "--config") && i + 1 < spec.clean_tokens.len() {
            let next = &spec.clean_tokens[i + 1];
            if next.starts_with("developer_instructions=") {
                existing_dev_instructions =
                    Some(next.split_once('=').map_or("", |(_, v)| v).to_string());
                skip_indexes.insert(i);
                skip_indexes.insert(i + 1);
                break;
            }
        }
        i += 1;
    }

    // Build combined developer instructions
    let combined = if let Some(existing) = existing_dev_instructions {
        format!("{}\n---\n{}", bootstrap_text, existing)
    } else {
        bootstrap_text.to_string()
    };

    let positional_set: std::collections::HashSet<usize> =
        spec.positional_indexes.iter().copied().collect();
    let remaining_tokens: Vec<String> = spec
        .clean_tokens
        .iter()
        .enumerate()
        .filter(|(idx, _)| !skip_indexes.contains(idx))
        .filter(|(idx, _)| {
            !matches!(spec.subcommand.as_deref(), Some("resume" | "fork"))
                || !positional_set.contains(idx)
        })
        .map(|(_, token)| token.clone())
        .collect();

    let mut result = Vec::new();
    if let Some(ref sub) = spec.subcommand {
        result.push(sub.clone());
        if matches!(sub.as_str(), "resume" | "fork") {
            result.extend(spec.positional_tokens.iter().cloned());
        }
    }
    result.push("-c".to_string());
    result.push(format!("developer_instructions={}", combined));
    result.extend(remaining_tokens);

    result
}

/// Remove any Codex `developer_instructions=...` config entries.
///
/// Resume/fork should not carry the previous instance's embedded hcom session
/// block because it hard-codes the original instance name. A fresh bootstrap is
/// injected later for the new instance.
pub fn strip_codex_developer_instructions(codex_args: &[String]) -> Vec<String> {
    let spec = resolve_codex_args(Some(codex_args), None);
    let mut result = Vec::new();
    let mut i = 0;

    while i < spec.clean_tokens.len() {
        let token = &spec.clean_tokens[i];

        if token.starts_with("-c=developer_instructions=")
            || token.starts_with("--config=developer_instructions=")
        {
            i += 1;
            continue;
        }

        if (token == "-c" || token == "--config") && i + 1 < spec.clean_tokens.len() {
            let next = &spec.clean_tokens[i + 1];
            if next.starts_with("developer_instructions=") {
                i += 2;
                continue;
            }
        }

        result.push(token.clone());
        i += 1;
    }

    if let Some(ref sub) = spec.subcommand {
        let mut with_sub = vec![sub.clone()];
        with_sub.extend(result);
        with_sub
    } else {
        result
    }
}

/// Preprocess Codex CLI arguments for hcom integration.
///
/// Applies:
/// 1. Strip stale developer_instructions (resume/fork only — they carry old identity)
/// 2. Sandbox flags based on mode
/// 3. Runtime hook-trust bypass for Codex versions that require unmanaged hook trust
/// 4. --add-dir ~/.hcom for hcom DB writes
/// 5. Bootstrap injection via developer_instructions
pub fn preprocess_codex_args(
    codex_args: &[String],
    bootstrap_text: &str,
    sandbox_mode: &str,
) -> Vec<String> {
    // 1. Strip stale developer_instructions for resume/fork only.
    //    Fresh launches may have user system_prompt in developer_instructions
    //    that add_codex_developer_instructions will merge with bootstrap.
    let spec = resolve_codex_args(Some(codex_args), None);
    let codex_args = if matches!(spec.subcommand.as_deref(), Some("resume" | "fork")) {
        strip_codex_developer_instructions(codex_args)
    } else {
        codex_args.to_vec()
    };

    // 2. Inject sandbox flags based on mode. Treat hcom's profile as the
    // lower-precedence side of the normal Codex arg merge so a user-provided
    // sandbox/approval flag overrides the whole sandbox group without
    // dropping repeatable hcom config such as the network seatbelt tweak.
    let sandbox_flags = get_sandbox_flags(sandbox_mode);
    let mut args: Vec<String> = if !sandbox_flags.is_empty() {
        let sandbox_spec = resolve_codex_args(Some(&sandbox_flags), None);
        let cli_spec = resolve_codex_args(Some(&codex_args), None);
        merge_codex_args(&sandbox_spec, &cli_spec).rebuild_tokens(true, true)
    } else {
        codex_args
    };

    // 3. Codex 0.131.0+ requires unmanaged hooks to be trusted. hcom's Codex
    // hooks are launch-managed by hcom, but Codex sees them as user hooks, so
    // use Codex's runtime automation flag when available.
    args = add_hook_trust_bypass_if_supported(&args);

    // Warn if mode is "none"
    if sandbox_mode == "none" {
        eprintln!("[hcom] Warning: Sandbox mode is 'none' - --add-dir ~/.hcom disabled.");
        eprintln!("[hcom] hcom commands may fail unless HCOM_DIR is within workspace.");
    }

    // 4. Ensure --add-dir ~/.hcom is present (skips if mode="none")
    args = ensure_hcom_writable(&args);

    // 5. Add bootstrap to developer_instructions
    args = add_codex_developer_instructions(&args, bootstrap_text);

    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| i.to_string()).collect()
    }

    fn write_trusted_hcom_codex_hooks(codex_home: &std::path::Path) {
        let hooks_path = codex_home.join("hooks.json");
        std::fs::create_dir_all(codex_home).unwrap();
        std::fs::write(
            &hooks_path,
            serde_json::json!({
                "hooks": {
                    "PreToolUse": [{
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "command": "hcom codex-pretooluse"}]
                    }],
                    "PostToolUse": [{
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "command": "hcom codex-posttooluse"}]
                    }],
                    "SessionStart": [{
                        "matcher": "startup|resume|clear",
                        "hooks": [{"type": "command", "command": "hcom codex-sessionstart"}]
                    }],
                    "UserPromptSubmit": [{
                        "hooks": [{"type": "command", "command": "hcom codex-userpromptsubmit"}]
                    }],
                    "Stop": [{
                        "hooks": [{"type": "command", "command": "hcom codex-stop"}]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        std::fs::write(
            codex_home.join("config.toml"),
            "[features]\nhooks = true\n\n",
        )
        .unwrap();
        crate::hooks::codex::ensure_codex_hcom_hooks_trusted().unwrap();
    }

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, original }
        }

        fn remove(key: &'static str) -> Self {
            let original = std::env::var(key).ok();
            unsafe { std::env::remove_var(key) };
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(value) = self.original.as_ref() {
                unsafe { std::env::set_var(self.key, value) };
            } else {
                unsafe { std::env::remove_var(self.key) };
            }
        }
    }

    fn init_config() {
        // Config::init is idempotent-ish but needs to be called before paths::hcom_dir()
        crate::config::Config::init();
    }

    #[test]
    fn test_sandbox_flags_workspace() {
        let flags = get_sandbox_flags("workspace");
        assert!(flags.contains(&"--sandbox".to_string()));
        assert!(flags.contains(&"workspace-write".to_string()));
        assert!(flags.contains(&"sandbox_workspace_write.network_access=true".to_string()));
    }

    #[test]
    fn test_sandbox_flags_untrusted() {
        let flags = get_sandbox_flags("untrusted");
        assert!(flags.contains(&"--sandbox".to_string()));
        assert!(flags.contains(&"workspace-write".to_string()));
        assert!(flags.contains(&"-a".to_string()));
        assert!(flags.contains(&"untrusted".to_string()));
    }

    #[test]
    fn test_sandbox_flags_danger() {
        let flags = get_sandbox_flags("danger-full-access");
        assert_eq!(
            flags,
            vec!["--dangerously-bypass-approvals-and-sandbox".to_string()]
        );
    }

    #[test]
    fn test_sandbox_flags_none() {
        let flags = get_sandbox_flags("none");
        assert!(flags.is_empty());
    }

    #[test]
    fn test_sandbox_flags_unknown_defaults_to_workspace() {
        let flags = get_sandbox_flags("bogus");
        assert!(flags.contains(&"--sandbox".to_string()));
        assert!(flags.contains(&"workspace-write".to_string()));
    }

    #[test]
    #[serial]
    fn test_ensure_hcom_writable_adds_dir() {
        init_config();
        // --full-auto is still recognized as a sandbox-active marker for
        // back-compat with user-provided args, even though hcom no longer emits it.
        let tokens = s(&["--full-auto"]);
        let result = ensure_hcom_writable(&tokens);
        assert_eq!(result[0], "--add-dir");
        assert!(result.len() > 2);
    }

    #[test]
    #[serial]
    fn test_ensure_hcom_writable_treats_yolo_as_sandbox_active() {
        init_config();
        let tokens = s(&["--yolo"]);
        let result = ensure_hcom_writable(&tokens);
        assert_eq!(result[0], "--add-dir");
        assert!(result.contains(&"--yolo".to_string()));
    }

    #[test]
    fn test_ensure_hcom_writable_skips_no_sandbox() {
        // No sandbox flags → mode="none" → skip (doesn't use paths)
        let tokens = s(&["-m", "o3"]);
        let result = ensure_hcom_writable(&tokens);
        assert_eq!(result, tokens);
    }

    #[test]
    #[serial]
    fn test_ensure_hcom_writable_no_duplicate() {
        init_config();
        let hcom_dir = paths::hcom_dir().to_string_lossy().to_string();
        let tokens = vec!["--full-auto".to_string(), "--add-dir".to_string(), hcom_dir];
        let result = ensure_hcom_writable(&tokens);
        let add_dir_count = result.iter().filter(|t| *t == "--add-dir").count();
        assert_eq!(add_dir_count, 1);
    }

    #[test]
    #[serial]
    fn test_ensure_codex_home_writable_probes_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let _codex_home_guard = EnvGuard::set("CODEX_HOME", dir.path().to_string_lossy().as_ref());

        ensure_codex_home_writable().unwrap();

        assert!(!dir.path().join(".hcom_writable_probe").exists());
    }

    #[test]
    #[serial]
    fn test_ensure_codex_home_writable_skips_missing_explicit_home() {
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join("missing-codex-home");
        let _codex_home_guard = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy().as_ref());

        ensure_codex_home_writable().unwrap();

        assert!(!codex_home.exists());
        assert!(!dir.path().join(".hcom_writable_probe").exists());
    }

    #[test]
    #[serial]
    fn test_ensure_codex_home_writable_probes_parent_when_default_home_missing() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let _codex_home_guard = EnvGuard::remove("CODEX_HOME");
        let _home_guard = EnvGuard::set("HOME", home.to_string_lossy().as_ref());

        ensure_codex_home_writable().unwrap();

        assert!(!home.join(".codex").exists());
        assert!(!home.join(".hcom_writable_probe").exists());
    }

    #[test]
    #[serial]
    fn test_add_hook_trust_bypass_supported() {
        let _guard = EnvGuard::set("HCOM_TEST_CODEX_CLI_VERSION", "codex 0.131.0");
        let dir = tempfile::tempdir().unwrap();
        let _codex_home_guard = EnvGuard::set("CODEX_HOME", dir.path().to_string_lossy().as_ref());
        let args = s(&["-m", "o3"]);
        let result = add_hook_trust_bypass_if_supported(&args);
        assert!(result.contains(&BYPASS_HOOK_TRUST_FLAG.to_string()));
        assert_eq!(
            result
                .iter()
                .filter(|t| *t == BYPASS_HOOK_TRUST_FLAG)
                .count(),
            1
        );
    }

    #[test]
    #[serial]
    fn test_add_hook_trust_bypass_skips_when_hcom_hooks_trusted() {
        let _version_guard = EnvGuard::set("HCOM_TEST_CODEX_CLI_VERSION", "codex 0.131.0");
        let dir = tempfile::tempdir().unwrap();
        let _codex_home_guard = EnvGuard::set("CODEX_HOME", dir.path().to_string_lossy().as_ref());
        write_trusted_hcom_codex_hooks(dir.path());

        let args = s(&["-m", "o3"]);
        let result = add_hook_trust_bypass_if_supported(&args);
        assert!(!result.contains(&BYPASS_HOOK_TRUST_FLAG.to_string()));
    }

    #[test]
    #[serial]
    fn test_add_hook_trust_bypass_self_heals_version_mismatch() {
        let _version_guard = EnvGuard::set("HCOM_TEST_CODEX_CLI_VERSION", "codex 0.131.0");
        let dir = tempfile::tempdir().unwrap();
        let _codex_home_guard = EnvGuard::set("CODEX_HOME", dir.path().to_string_lossy().as_ref());
        write_trusted_hcom_codex_hooks(dir.path());
        let config_path = dir.path().join("config.toml");
        let stale = std::fs::read_to_string(&config_path)
            .unwrap()
            .replace("0.131.0", "0.130.0");
        std::fs::write(&config_path, stale).unwrap();

        let args = s(&["-m", "o3"]);
        let result = add_hook_trust_bypass_if_supported(&args);
        assert!(!result.contains(&BYPASS_HOOK_TRUST_FLAG.to_string()));
        let healed = std::fs::read_to_string(config_path).unwrap();
        assert!(healed.contains("hcom_codex_cli_version = \"0.131.0\""));
    }

    #[test]
    #[serial]
    fn test_add_hook_trust_bypass_self_heals_stale_trusted_hash() {
        let _version_guard = EnvGuard::set("HCOM_TEST_CODEX_CLI_VERSION", "codex 0.131.0");
        let dir = tempfile::tempdir().unwrap();
        let _codex_home_guard = EnvGuard::set("CODEX_HOME", dir.path().to_string_lossy().as_ref());
        write_trusted_hcom_codex_hooks(dir.path());
        let config_path = dir.path().join("config.toml");
        let stale = std::fs::read_to_string(&config_path)
            .unwrap()
            .replace("sha256:test-0", "sha256:stale");
        std::fs::write(&config_path, stale).unwrap();

        let args = s(&["-m", "o3"]);
        let result = add_hook_trust_bypass_if_supported(&args);
        assert!(!result.contains(&BYPASS_HOOK_TRUST_FLAG.to_string()));
        let healed = std::fs::read_to_string(config_path).unwrap();
        assert!(healed.contains("sha256:test-0"));
        assert!(!healed.contains("sha256:stale"));
    }

    #[test]
    #[serial]
    fn test_add_hook_trust_bypass_falls_back_when_self_heal_fails() {
        let _version_guard = EnvGuard::set("HCOM_TEST_CODEX_CLI_VERSION", "codex 0.131.0");
        let _hooks_guard = EnvGuard::set("HCOM_TEST_CODEX_HOOKS_LIST_JSON", "__fail__");
        let dir = tempfile::tempdir().unwrap();
        let _codex_home_guard = EnvGuard::set("CODEX_HOME", dir.path().to_string_lossy().as_ref());

        let args = s(&["-m", "o3"]);
        let result = add_hook_trust_bypass_if_supported(&args);
        assert!(result.contains(&BYPASS_HOOK_TRUST_FLAG.to_string()));
    }

    #[test]
    #[serial]
    fn test_add_hook_trust_bypass_no_duplicate_when_user_supplied() {
        let _guard = EnvGuard::set("HCOM_TEST_CODEX_CLI_VERSION", "codex 0.131.0");
        let args = s(&[BYPASS_HOOK_TRUST_FLAG, "-m", "o3"]);
        let result = add_hook_trust_bypass_if_supported(&args);
        assert_eq!(
            result
                .iter()
                .filter(|t| *t == BYPASS_HOOK_TRUST_FLAG)
                .count(),
            1
        );
    }

    #[test]
    #[serial]
    fn test_add_hook_trust_bypass_unsupported() {
        let _guard = EnvGuard::set("HCOM_TEST_CODEX_CLI_VERSION", "codex 0.130.0");
        let args = s(&["-m", "o3"]);
        let result = add_hook_trust_bypass_if_supported(&args);
        assert!(!result.contains(&BYPASS_HOOK_TRUST_FLAG.to_string()));
    }

    #[test]
    #[serial]
    fn test_add_hook_trust_bypass_keeps_resume_session_first() {
        let _guard = EnvGuard::set("HCOM_TEST_CODEX_CLI_VERSION", "codex 0.131.0");
        let dir = tempfile::tempdir().unwrap();
        let _codex_home_guard = EnvGuard::set("CODEX_HOME", dir.path().to_string_lossy().as_ref());
        let args = s(&["resume", "thread-1", "--model", "gpt-5"]);
        let result = add_hook_trust_bypass_if_supported(&args);
        assert_eq!(result[0], "resume");
        assert_eq!(result[1], "thread-1");
        assert!(result.contains(&BYPASS_HOOK_TRUST_FLAG.to_string()));
    }

    #[test]
    fn test_parse_codex_cli_version_uses_last_version_like_token() {
        assert_eq!(
            parse_codex_cli_version("codex build 1.2.3 0.131.0"),
            Some((0, 131, 0))
        );
    }

    #[test]
    fn test_add_developer_instructions_basic() {
        let args = s(&["-m", "o3"]);
        let result = add_codex_developer_instructions(&args, "BOOTSTRAP");
        assert_eq!(result[0], "-c");
        assert_eq!(result[1], "developer_instructions=BOOTSTRAP");
        assert!(result.contains(&"-m".to_string()));
    }

    #[test]
    fn test_add_developer_instructions_keeps_resume() {
        let args = s(&["resume"]);
        let result = add_codex_developer_instructions(&args, "BOOTSTRAP");
        assert_eq!(result[0], "resume");
        assert_eq!(result[1], "-c");
        assert_eq!(result[2], "developer_instructions=BOOTSTRAP");
    }

    #[test]
    fn test_add_developer_instructions_keeps_resume_session_first() {
        let args = s(&["resume", "thread-1", "--model", "gpt-5"]);
        let result = add_codex_developer_instructions(&args, "BOOTSTRAP");
        assert_eq!(result[0], "resume");
        assert_eq!(result[1], "thread-1");
        assert_eq!(result[2], "-c");
        assert_eq!(result[3], "developer_instructions=BOOTSTRAP");
        assert_eq!(result[4], "--model");
        assert_eq!(result[5], "gpt-5");
    }

    #[test]
    fn test_add_developer_instructions_keeps_fork_session_first_with_existing_config() {
        let args = s(&[
            "fork",
            "thread-1",
            "-c",
            "developer_instructions=OLD",
            "--model",
            "gpt-5",
        ]);
        let result = add_codex_developer_instructions(&args, "BOOTSTRAP");
        assert_eq!(result[0], "fork");
        assert_eq!(result[1], "thread-1");
        assert_eq!(result[2], "-c");
        assert!(result[3].contains("BOOTSTRAP"));
        assert!(result[3].contains("OLD"));
        assert_eq!(result[4], "--model");
        assert_eq!(result[5], "gpt-5");
    }

    #[test]
    fn test_add_developer_instructions_merge_existing() {
        let args = s(&["-c", "developer_instructions=USER_NOTES", "-m", "o3"]);
        let result = add_codex_developer_instructions(&args, "BOOTSTRAP");
        assert!(result[1].contains("BOOTSTRAP"));
        assert!(result[1].contains("USER_NOTES"));
        assert!(result[1].contains("---"));
        let di_count = result
            .iter()
            .filter(|t| t.starts_with("developer_instructions="))
            .count();
        assert_eq!(di_count, 1);
    }

    #[test]
    fn test_add_developer_instructions_preserves_fork_subcommand() {
        let args = s(&["fork", "-m", "o3"]);
        let result = add_codex_developer_instructions(&args, "BOOTSTRAP");
        assert_eq!(result[0], "fork");
        assert_eq!(result[1], "-c");
    }

    #[test]
    fn test_strip_developer_instructions_space_syntax() {
        let args = s(&["fork", "-c", "developer_instructions=OLD", "--model", "o3"]);
        let result = strip_codex_developer_instructions(&args);
        assert_eq!(result, s(&["fork", "--model", "o3"]));
    }

    #[test]
    fn test_strip_developer_instructions_equals_syntax() {
        let args = s(&[
            "resume",
            "--config=developer_instructions=OLD",
            "--full-auto",
        ]);
        let result = strip_codex_developer_instructions(&args);
        assert_eq!(result, s(&["resume", "--full-auto"]));
    }

    #[test]
    #[serial]
    fn test_preprocess_codex_args_full_pipeline() {
        let _guard = EnvGuard::set("HCOM_TEST_CODEX_CLI_VERSION", "codex 0.131.0");
        let dir = tempfile::tempdir().unwrap();
        let _codex_home_guard = EnvGuard::set("CODEX_HOME", dir.path().to_string_lossy().as_ref());
        init_config();
        let args = s(&["-m", "o3"]);
        let result = preprocess_codex_args(&args, "BOOTSTRAP", "workspace");
        assert!(result.contains(&"--sandbox".to_string()));
        assert!(result.contains(&"workspace-write".to_string()));
        assert!(result.contains(&"--add-dir".to_string()));
        assert!(result.contains(&BYPASS_HOOK_TRUST_FLAG.to_string()));
        assert!(result.iter().any(|t| t.contains("developer_instructions=")));
    }

    #[test]
    #[serial]
    fn test_preprocess_resume_keeps_session_before_hook_trust_bypass() {
        let _guard = EnvGuard::set("HCOM_TEST_CODEX_CLI_VERSION", "codex 0.131.0");
        let dir = tempfile::tempdir().unwrap();
        let _codex_home_guard = EnvGuard::set("CODEX_HOME", dir.path().to_string_lossy().as_ref());
        init_config();
        let args = s(&["resume", "thread-1", "--model", "gpt-5"]);
        let result = preprocess_codex_args(&args, "BOOTSTRAP", "workspace");
        assert_eq!(result[0], "resume");
        assert_eq!(result[1], "thread-1");
        assert!(result.contains(&BYPASS_HOOK_TRUST_FLAG.to_string()));
        assert!(result.iter().any(|t| t.contains("developer_instructions=")));
    }

    #[test]
    #[serial]
    fn test_preprocess_user_sandbox_overrides_hcom_default() {
        init_config();
        let args = s(&["--sandbox", "danger-full-access", "-m", "o3"]);
        let result = preprocess_codex_args(&args, "BOOTSTRAP", "workspace");
        assert_eq!(result.iter().filter(|t| *t == "--sandbox").count(), 1);
        assert!(result.contains(&"danger-full-access".to_string()));
        assert!(!result.contains(&"workspace-write".to_string()));
        assert!(result.contains(&"--add-dir".to_string()));
        assert!(result.contains(&"sandbox_workspace_write.network_access=true".to_string()));
    }

    #[test]
    #[serial]
    fn test_preprocess_yolo_overrides_hcom_default_sandbox() {
        init_config();
        let args = s(&["--yolo", "-m", "o3"]);
        let result = preprocess_codex_args(&args, "BOOTSTRAP", "workspace");

        assert!(result.contains(&"--yolo".to_string()));
        assert!(!result.contains(&"--sandbox".to_string()));
        assert!(!result.contains(&"workspace-write".to_string()));
        assert!(result.contains(&"sandbox_workspace_write.network_access=true".to_string()));
        assert!(result.contains(&"--add-dir".to_string()));
    }

    #[test]
    #[serial]
    fn test_preprocess_user_approval_overrides_hcom_approval_default() {
        init_config();
        let args = s(&["-a", "on-request", "-m", "o3"]);
        let result = preprocess_codex_args(&args, "BOOTSTRAP", "untrusted");
        assert_eq!(result.iter().filter(|t| *t == "-a").count(), 1);
        assert!(result.contains(&"on-request".to_string()));
        assert!(!result.contains(&"untrusted".to_string()));
        assert!(result.contains(&"sandbox_workspace_write.network_access=true".to_string()));
    }

    #[test]
    fn test_preprocess_codex_args_none_mode() {
        let args = s(&["-m", "o3"]);
        let result = preprocess_codex_args(&args, "BOOTSTRAP", "none");
        assert!(!result.contains(&"--sandbox".to_string()));
        assert!(!result.contains(&"--add-dir".to_string()));
        assert!(result.iter().any(|t| t.contains("developer_instructions=")));
    }

    #[test]
    #[serial]
    fn test_preprocess_strips_stale_on_resume() {
        init_config();
        let args = s(&[
            "resume",
            "-c",
            "developer_instructions=STALE_BOOTSTRAP",
            "-m",
            "o3",
        ]);
        let result = preprocess_codex_args(&args, "FRESH", "workspace");
        let di: Vec<&String> = result
            .iter()
            .filter(|t| t.starts_with("developer_instructions="))
            .collect();
        assert_eq!(di.len(), 1);
        assert!(di[0].contains("FRESH"));
        assert!(!di[0].contains("STALE"));
    }

    #[test]
    #[serial]
    fn test_preprocess_preserves_user_instructions_on_fresh_launch() {
        init_config();
        let args = s(&["-c", "developer_instructions=USER_NOTES", "-m", "o3"]);
        let result = preprocess_codex_args(&args, "BOOTSTRAP", "workspace");
        let di: Vec<&String> = result
            .iter()
            .filter(|t| t.starts_with("developer_instructions="))
            .collect();
        assert_eq!(di.len(), 1);
        assert!(di[0].contains("BOOTSTRAP"));
        assert!(di[0].contains("USER_NOTES"));
    }
}
