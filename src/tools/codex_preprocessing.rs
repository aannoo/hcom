//! Codex launch preprocessing — sandbox flags, DB access, bootstrap injection.

use crate::paths;

use super::codex_args::{merge_codex_args, resolve_codex_args};

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
        ],
        &["--sandbox=", "-s="],
    );
    if !has_sandbox {
        return tokens.to_vec();
    }

    let hcom_dir = paths::hcom_dir().to_string_lossy().to_string();
    let quoted_hcom_dir = serde_json::to_string(&hcom_dir).unwrap_or_else(|_| format!("\"{}\"", hcom_dir));
    let config_val = format!("sandbox_workspace_write.writable_roots=[{}]", quoted_hcom_dir);

    // Check if --add-dir with hcom path already exists
    let has_add_dir = if let Some(val) = spec.get_flag_value("--add-dir") {
        val.as_list().contains(&hcom_dir.as_str())
    } else {
        false
    };

    // Check if the config override already exists
    let has_config = if let Some(val) = spec.get_flag_value("-c") {
        val.as_list().contains(&config_val.as_str())
    } else {
        false
    };

    if has_add_dir && has_config {
        return tokens.to_vec(); // Both already present
    }

    let mut result = Vec::new();

    if !has_add_dir {
        result.push("--add-dir".to_string());
        result.push(hcom_dir.clone());
    }

    if !has_config {
        // ALSO inject -c sandbox_workspace_write.writable_roots=["<hcom_dir>"] as a
        // workaround for Codex sandbox profile resolution issues where --add-dir
        // is sometimes dropped.
        result.push("-c".to_string());
        result.push(config_val);
    }

    result.extend(tokens.iter().cloned());
    result
}

/// Add hcom bootstrap to codex developer_instructions.
///
/// Builds full bootstrap and adds via `-c developer_instructions=...` flag.
/// If user also provided developer_instructions, bootstrap comes first,
/// then separator, then user content.
///
/// Skip for exec/review subcommands (not interactive launch).
pub fn add_codex_developer_instructions(
    codex_args: &[String],
    bootstrap_text: &str,
) -> Vec<String> {
    let spec = resolve_codex_args(Some(codex_args), None);

    // Skip non-interactive modes. Resume/fork need fresh bootstrap because
    // the canonical instance name changes and stale embedded hcom context can
    // point the child session at the parent identity.
    if let Some(ref sub) = spec.subcommand {
        if matches!(sub.as_str(), "exec" | "e" | "review") {
            return codex_args.to_vec();
        }
    }

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
/// 3. --add-dir ~/.hcom for hcom DB writes
/// 4. Bootstrap injection via developer_instructions
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

    // Warn if mode is "none"
    if sandbox_mode == "none" {
        eprintln!("[hcom] Warning: Sandbox mode is 'none' - --add-dir ~/.hcom disabled.");
        eprintln!("[hcom] hcom commands may fail unless HCOM_DIR is within workspace.");
    }

    // 3. Ensure --add-dir ~/.hcom is present (skips if mode="none")
    args = ensure_hcom_writable(&args);

    // 4. Add bootstrap to developer_instructions
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
        // If only --add-dir is already present, it should not duplicate that flag.
        // The config override may still be injected.
        let tokens = vec!["--full-auto".to_string(), "--add-dir".to_string(), hcom_dir];
        let result = ensure_hcom_writable(&tokens);
        let add_dir_count = result.iter().filter(|t| *t == "--add-dir").count();
        assert_eq!(add_dir_count, 1);
    }

    #[test]
    #[serial]
    fn test_ensure_hcom_writable_avoids_duplicate_config() {
        init_config();
        let hcom_dir = paths::hcom_dir().to_string_lossy().to_string();
        let config_val = format!("sandbox_workspace_write.writable_roots=[\"{}\"]", hcom_dir);
        // If both are present, should return original tokens
        let tokens = vec![
            "--sandbox".to_string(),
            "workspace-write".to_string(),
            "--add-dir".to_string(),
            hcom_dir,
            "-c".to_string(),
            config_val,
        ];
        let result = ensure_hcom_writable(&tokens);
        assert_eq!(result, tokens);
    }

    #[test]
    #[serial]
    fn test_ensure_hcom_writable_adds_config_override() {
        init_config();
        let hcom_dir = paths::hcom_dir().to_string_lossy().to_string();
        let tokens = s(&["--sandbox", "workspace-write"]);
        let result = ensure_hcom_writable(&tokens);
        
        // Should contain --add-dir <hcom_dir>
        assert!(result.contains(&"--add-dir".to_string()));
        assert!(result.contains(&hcom_dir));
        
        // Should ALSO contain -c sandbox_workspace_write.writable_roots=["<hcom_dir>"]
        let config_val = format!("sandbox_workspace_write.writable_roots=[\"{}\"]", hcom_dir);
        assert!(result.contains(&"-c".to_string()));
        assert!(result.contains(&config_val));
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
    fn test_add_developer_instructions_skip_exec() {
        let args = s(&["exec", "echo", "hi"]);
        let result = add_codex_developer_instructions(&args, "BOOTSTRAP");
        assert_eq!(result, args);
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
    fn test_add_developer_instructions_preserves_subcommand() {
        let args = s(&["mcp", "-m", "o3"]);
        let result = add_codex_developer_instructions(&args, "BOOTSTRAP");
        // mcp subcommand should be first
        assert_eq!(result[0], "mcp");
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
        init_config();
        let args = s(&["-m", "o3"]);
        let result = preprocess_codex_args(&args, "BOOTSTRAP", "workspace");
        assert!(result.contains(&"--sandbox".to_string()));
        assert!(result.contains(&"workspace-write".to_string()));
        assert!(result.contains(&"--add-dir".to_string()));
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
