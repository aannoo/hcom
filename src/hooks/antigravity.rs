use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum VerifyFailReason {
    #[error("hooks.json settings unreadable or empty")]
    SettingsUnreadableOrEmpty,
    #[error("agy permissions missing or incomplete in: {0}")]
    PermissionsMissing(PathBuf),
    #[error("hooks.json missing 'hcom-lifecycle' group key")]
    HcomLifecycleKeyMissing,
    #[error("hook event '{0}' missing or empty")]
    HookEventMissing(String),
    #[error("hcom hook command '{cmd_suffix}' not found under event '{event}'")]
    HookCommandMissing { event: String, cmd_suffix: String },
    #[error("event '{0}': hcom entry has 'type' != \"command\"")]
    HookTypeFieldNotCommand(String),
    #[error("event '{event}' name mismatch: expected {expected:?}, got {actual:?}")]
    HookNameMismatch {
        event: String,
        expected: String,
        actual: String,
    },
    #[error("event '{event}' matcher mismatch: expected {expected:?}, got {actual:?}")]
    HookMatcherMismatch {
        event: String,
        expected: String,
        actual: String,
    },
    #[error("event '{event}' has no numeric 'timeout' field (canonical)")]
    HookTimeoutMissing { event: String },
    #[error("duplicate hcom hook entry for event '{0}'")]
    HookDuplicated(String),
}

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("existing hooks.json at {} could not be read: {source}", path.display())]
    ExistingReadFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("existing hooks.json at {} is not valid JSON: {source}", path.display())]
    ExistingParseFailed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("existing hooks.json at {} must be a JSON object", path.display())]
    ExistingRootNotObject { path: PathBuf },
    #[error("JSON serialization failed: {0}")]
    SerializationFailed(#[from] serde_json::Error),
    #[error("atomic write to {} failed: {source}", path.display())]
    AtomicWriteFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("post-write verify failed for {}: {reason}", path.display())]
    PostWriteVerifyFailed {
        path: PathBuf,
        #[source]
        reason: VerifyFailReason,
    },
}

/// Resolve the path to the Antigravity `hooks.json` file.
/// Under the split-config design, this resides at `~/.gemini/config/hooks.json`.
pub fn get_antigravity_hooks_path() -> PathBuf {
    antigravity_hooks_path(&crate::runtime_env::gemini_family_config_dir())
}

fn antigravity_hooks_path(gemini_dir: &Path) -> PathBuf {
    gemini_dir.join("config").join("hooks.json")
}

/// Shell wrapper for a single hcom hook subcommand (`gemini-beforeagent`, etc.).
///
/// `fallback_json` is echoed to stdout when hcom is missing, before exiting 0.
/// agy requires a `decision` JSON response on PreToolUse and Stop; PostToolUse and
/// PostInvocation accept an empty body.
///
/// The fallback is delivered base64-encoded and piped through `base64 -d` so the
/// JSON's quotes (and any apostrophes) survive the nested `sh -c '...'` pass —
/// naive interpolation gets stripped or mis-tokenized by the inner shell.
///
/// This POSIX `sh -c` form was verified to work on real Windows (agy, this
/// branch): the hook fires and a sent message is delivered into the live agy
/// session. agy resolves `sh` through an installed Git Bash even when `sh`
/// itself is not on the user's PATH, so no PowerShell variant is needed here.
/// This is the same Git Bash dependency hcom already requires for workflow
/// scripts on Windows; on a Windows box without Git Bash the hook would not
/// fire.
fn hook_sh_cmd(hcom_cmd: &str, subcmd: &str, fallback_json: &str) -> String {
    let bin = hcom_cmd.split_whitespace().next().unwrap_or("hcom");
    if fallback_json.is_empty() {
        format!(
            "sh -c 'command -v {bin} >/dev/null 2>&1 && ANTIGRAVITY_AGENT=1 exec {hcom_cmd} {subcmd} || exit 0'"
        )
    } else {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(fallback_json.as_bytes());
        format!(
            "sh -c 'command -v {bin} >/dev/null 2>&1 && ANTIGRAVITY_AGENT=1 exec {hcom_cmd} {subcmd} || {{ printf %s {b64} | base64 -d; exit 0; }}'"
        )
    }
}

/// PreInvocation sessionstart: invoke hcom; idempotent via `name_announced`
/// (bootstrap injection no-ops after first run, so re-firing per turn is safe).
fn hook_sessionstart_cmd(hcom_cmd: &str) -> String {
    hook_sh_cmd(hcom_cmd, "gemini-sessionstart", "")
}

/// Try to set up Antigravity hooks in `hooks.json`.
/// Reads existing hooks.json, merges "hcom-lifecycle" group, and preserves all other keys.
pub fn try_setup_antigravity_hooks(include_permissions: bool) -> Result<(), SetupError> {
    let hooks_path = get_antigravity_hooks_path();
    if let Some(parent) = hooks_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Load existing hooks or initialize an empty map
    let mut hooks_root = if hooks_path.exists() {
        let content = std::fs::read_to_string(&hooks_path).map_err(|source| {
            SetupError::ExistingReadFailed {
                path: hooks_path.clone(),
                source,
            }
        })?;
        let value: Value =
            serde_json::from_str(&content).map_err(|source| SetupError::ExistingParseFailed {
                path: hooks_path.clone(),
                source,
            })?;
        value
            .as_object()
            .cloned()
            .ok_or_else(|| SetupError::ExistingRootNotObject {
                path: hooks_path.clone(),
            })?
    } else {
        serde_json::Map::new()
    };

    let hcom_cmd = crate::runtime_env::build_hcom_command();

    // Fallback JSON constants for hooks where agy requires a decision response when
    // hcom is missing. PreToolUse needs `{"decision":"allow"}`; Stop needs a decision
    // field where any value other than "continue" allows the stop. PostToolUse and the
    // *Invocation lifecycle hooks accept an empty body.
    const ALLOW_JSON: &str = "{\"decision\":\"allow\"}";

    // 15s timeout: agy default is 30s; 5s was tight under cold-start + busy sqlite
    // on slower machines / CI. 15s leaves margin without leaving a stuck hook
    // blocking the agent turn for half a minute.
    const HOOK_TIMEOUT_SEC: u64 = 15;

    let hcom_lifecycle = json!({
        "PreInvocation": [
            {
                "name": "hcom-sessionstart",
                "type": "command",
                "command": hook_sessionstart_cmd(&hcom_cmd),
                "timeout": HOOK_TIMEOUT_SEC,
                "description": "Initialize hcom session"
            },
            {
                "name": "hcom-beforeagent",
                "type": "command",
                "command": hook_sh_cmd(&hcom_cmd, "gemini-beforeagent", ""),
                "timeout": HOOK_TIMEOUT_SEC,
                "description": "Deliver pending messages"
            }
        ],
        "PostInvocation": [
            {
                "name": "hcom-afteragent",
                "type": "command",
                "command": hook_sh_cmd(&hcom_cmd, "gemini-afteragent", ""),
                "timeout": HOOK_TIMEOUT_SEC,
                "description": "Signal ready for messages"
            }
        ],
        "Stop": [
            {
                "name": "hcom-sessionend",
                "type": "command",
                "command": hook_sh_cmd(&hcom_cmd, "gemini-sessionend", ALLOW_JSON),
                "timeout": HOOK_TIMEOUT_SEC,
                "description": "Disconnect from hcom"
            }
        ],
        "PreToolUse": [
            {
                "matcher": ".*",
                "hooks": [
                    {
                        "name": "hcom-beforetool",
                        "type": "command",
                        "command": hook_sh_cmd(&hcom_cmd, "gemini-beforetool", ALLOW_JSON),
                        "timeout": HOOK_TIMEOUT_SEC,
                        "description": "Track tool execution"
                    }
                ]
            }
        ],
        "PostToolUse": [
            {
                "matcher": ".*",
                "hooks": [
                    {
                        "name": "hcom-aftertool",
                        "type": "command",
                        "command": hook_sh_cmd(&hcom_cmd, "gemini-aftertool", ""),
                        "timeout": HOOK_TIMEOUT_SEC,
                        "description": "Deliver messages after tools"
                    }
                ]
            }
        ]
    });

    hooks_root.insert("hcom-lifecycle".to_string(), hcom_lifecycle);

    let json_str = serde_json::to_string_pretty(&Value::Object(hooks_root))
        .map_err(SetupError::SerializationFailed)?;

    crate::paths::atomic_write_io(&hooks_path, &json_str).map_err(|e| {
        SetupError::AtomicWriteFailed {
            path: hooks_path.clone(),
            source: e,
        }
    })?;

    // Agy stores permissions in its own settings.json under `permissions.allow`
    // using `command(...)` rules (not the gemini-cli TOML policy engine).
    if include_permissions {
        setup_antigravity_permissions();
    } else {
        remove_antigravity_permissions();
    }

    verify_hooks_at(&hooks_path, include_permissions).map_err(|reason| {
        SetupError::PostWriteVerifyFailed {
            path: hooks_path,
            reason,
        }
    })?;

    Ok(())
}

/// Verify if Antigravity hooks are correctly installed.
pub fn verify_antigravity_hooks_installed(check_permissions: bool) -> bool {
    verify_hooks_at(&get_antigravity_hooks_path(), check_permissions).is_ok()
}

/// Cleanly remove the `"hcom-lifecycle"` group key from `hooks.json` and
/// strip hcom permission rules from `~/.gemini/antigravity-cli/settings.json`.
/// Preserves other hooks.json keys, and removes the file if no other keys remain.
///
/// Returns true only when BOTH the hooks cleanup and the permission cleanup
/// succeed. Permission cleanup is attempted unconditionally \u2014 even when
/// hooks.json is missing, unreadable, or invalid \u2014 so a partially broken
/// install does not leave stale `command(hcom ...)` allow-rules behind.
pub fn remove_antigravity_hooks() -> bool {
    antigravity_cleanup_dirs().iter().all(|dir| {
        remove_hooks_lifecycle_block_at(&antigravity_hooks_path(dir))
            && remove_antigravity_permissions_at(&antigravity_settings_path(dir))
    })
}

/// Strip just the `"hcom-lifecycle"` block from hooks.json. Returns true on
/// success, including the "file absent" case. Does not touch permissions.
fn remove_hooks_lifecycle_block_at(path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut val: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let obj = match val.as_object_mut() {
        Some(o) => o,
        None => return false,
    };
    obj.remove("hcom-lifecycle");
    if obj.is_empty() {
        return std::fs::remove_file(path).is_ok();
    }
    let json_str = match serde_json::to_string_pretty(&Value::Object(obj.clone())) {
        Ok(s) => s,
        Err(_) => return false,
    };
    crate::paths::atomic_write_io(path, &json_str).is_ok()
}

fn verify_hooks_at(path: &Path, check_permissions: bool) -> Result<(), VerifyFailReason> {
    if !path.exists() {
        return Err(VerifyFailReason::SettingsUnreadableOrEmpty);
    }
    let content =
        std::fs::read_to_string(path).map_err(|_| VerifyFailReason::SettingsUnreadableOrEmpty)?;
    let val: Value =
        serde_json::from_str(&content).map_err(|_| VerifyFailReason::SettingsUnreadableOrEmpty)?;
    let root = val
        .as_object()
        .ok_or(VerifyFailReason::SettingsUnreadableOrEmpty)?;

    let lifecycle = root
        .get("hcom-lifecycle")
        .and_then(|v| v.as_object())
        .ok_or(VerifyFailReason::HcomLifecycleKeyMissing)?;

    // Check PreInvocation
    let pre_invocation = lifecycle
        .get("PreInvocation")
        .and_then(|v| v.as_array())
        .ok_or_else(|| VerifyFailReason::HookEventMissing("PreInvocation".to_string()))?;

    let mut found_sessionstart = false;
    let mut found_beforeagent = false;
    for hook in pre_invocation {
        let hook_obj = hook.as_object().ok_or_else(|| {
            VerifyFailReason::HookTypeFieldNotCommand("PreInvocation".to_string())
        })?;
        let name = hook_obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let command = hook_obj
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let hook_type = hook_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if hook_type != "command" {
            return Err(VerifyFailReason::HookTypeFieldNotCommand(
                "PreInvocation".to_string(),
            ));
        }
        if hook_obj.get("timeout").and_then(|v| v.as_u64()).is_none() {
            return Err(VerifyFailReason::HookTimeoutMissing {
                event: "PreInvocation".to_string(),
            });
        }

        if name == "hcom-sessionstart" {
            if found_sessionstart {
                return Err(VerifyFailReason::HookDuplicated(
                    "PreInvocation".to_string(),
                ));
            }
            if !command.contains("gemini-sessionstart") {
                return Err(VerifyFailReason::HookCommandMissing {
                    event: "PreInvocation".to_string(),
                    cmd_suffix: "gemini-sessionstart".to_string(),
                });
            }
            found_sessionstart = true;
        } else if name == "hcom-beforeagent" {
            if found_beforeagent {
                return Err(VerifyFailReason::HookDuplicated(
                    "PreInvocation".to_string(),
                ));
            }
            if !command.contains("gemini-beforeagent") {
                return Err(VerifyFailReason::HookCommandMissing {
                    event: "PreInvocation".to_string(),
                    cmd_suffix: "gemini-beforeagent".to_string(),
                });
            }
            found_beforeagent = true;
        }
    }
    if !found_sessionstart {
        return Err(VerifyFailReason::HookCommandMissing {
            event: "PreInvocation".to_string(),
            cmd_suffix: "gemini-sessionstart".to_string(),
        });
    }
    if !found_beforeagent {
        return Err(VerifyFailReason::HookCommandMissing {
            event: "PreInvocation".to_string(),
            cmd_suffix: "gemini-beforeagent".to_string(),
        });
    }

    // Check PostInvocation
    let post_invocation = lifecycle
        .get("PostInvocation")
        .and_then(|v| v.as_array())
        .ok_or_else(|| VerifyFailReason::HookEventMissing("PostInvocation".to_string()))?;
    let mut found_afteragent = false;
    for hook in post_invocation {
        let hook_obj = hook.as_object().ok_or_else(|| {
            VerifyFailReason::HookTypeFieldNotCommand("PostInvocation".to_string())
        })?;
        let name = hook_obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let command = hook_obj
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let hook_type = hook_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if hook_type != "command" {
            return Err(VerifyFailReason::HookTypeFieldNotCommand(
                "PostInvocation".to_string(),
            ));
        }
        if hook_obj.get("timeout").and_then(|v| v.as_u64()).is_none() {
            return Err(VerifyFailReason::HookTimeoutMissing {
                event: "PostInvocation".to_string(),
            });
        }

        if name == "hcom-afteragent" {
            if found_afteragent {
                return Err(VerifyFailReason::HookDuplicated(
                    "PostInvocation".to_string(),
                ));
            }
            if !command.contains("gemini-afteragent") {
                return Err(VerifyFailReason::HookCommandMissing {
                    event: "PostInvocation".to_string(),
                    cmd_suffix: "gemini-afteragent".to_string(),
                });
            }
            found_afteragent = true;
        }
    }
    if !found_afteragent {
        return Err(VerifyFailReason::HookCommandMissing {
            event: "PostInvocation".to_string(),
            cmd_suffix: "gemini-afteragent".to_string(),
        });
    }

    // Check Stop
    let stop = lifecycle
        .get("Stop")
        .and_then(|v| v.as_array())
        .ok_or_else(|| VerifyFailReason::HookEventMissing("Stop".to_string()))?;
    let mut found_sessionend = false;
    for hook in stop {
        let hook_obj = hook
            .as_object()
            .ok_or_else(|| VerifyFailReason::HookTypeFieldNotCommand("Stop".to_string()))?;
        let name = hook_obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let command = hook_obj
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let hook_type = hook_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if hook_type != "command" {
            return Err(VerifyFailReason::HookTypeFieldNotCommand(
                "Stop".to_string(),
            ));
        }
        if hook_obj.get("timeout").and_then(|v| v.as_u64()).is_none() {
            return Err(VerifyFailReason::HookTimeoutMissing {
                event: "Stop".to_string(),
            });
        }

        if name == "hcom-sessionend" {
            if found_sessionend {
                return Err(VerifyFailReason::HookDuplicated("Stop".to_string()));
            }
            if !command.contains("gemini-sessionend") {
                return Err(VerifyFailReason::HookCommandMissing {
                    event: "Stop".to_string(),
                    cmd_suffix: "gemini-sessionend".to_string(),
                });
            }
            found_sessionend = true;
        }
    }
    if !found_sessionend {
        return Err(VerifyFailReason::HookCommandMissing {
            event: "Stop".to_string(),
            cmd_suffix: "gemini-sessionend".to_string(),
        });
    }

    // Check PreToolUse
    let pre_tool_use = lifecycle
        .get("PreToolUse")
        .and_then(|v| v.as_array())
        .ok_or_else(|| VerifyFailReason::HookEventMissing("PreToolUse".to_string()))?;
    let mut found_beforetool = false;
    for matcher_val in pre_tool_use {
        let matcher_obj = matcher_val
            .as_object()
            .ok_or_else(|| VerifyFailReason::HookTypeFieldNotCommand("PreToolUse".to_string()))?;
        let matcher_pattern = matcher_obj
            .get("matcher")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if matcher_pattern != ".*" {
            return Err(VerifyFailReason::HookMatcherMismatch {
                event: "PreToolUse".to_string(),
                expected: ".*".to_string(),
                actual: matcher_pattern.to_string(),
            });
        }
        let hooks_arr = matcher_obj
            .get("hooks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| VerifyFailReason::HookTypeFieldNotCommand("PreToolUse".to_string()))?;
        for hook in hooks_arr {
            let hook_obj = hook.as_object().ok_or_else(|| {
                VerifyFailReason::HookTypeFieldNotCommand("PreToolUse".to_string())
            })?;
            let name = hook_obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let command = hook_obj
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let hook_type = hook_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

            if hook_type != "command" {
                return Err(VerifyFailReason::HookTypeFieldNotCommand(
                    "PreToolUse".to_string(),
                ));
            }
            if hook_obj.get("timeout").and_then(|v| v.as_u64()).is_none() {
                return Err(VerifyFailReason::HookTimeoutMissing {
                    event: "PreToolUse".to_string(),
                });
            }

            if name == "hcom-beforetool" {
                if found_beforetool {
                    return Err(VerifyFailReason::HookDuplicated("PreToolUse".to_string()));
                }
                if !command.contains("gemini-beforetool") {
                    return Err(VerifyFailReason::HookCommandMissing {
                        event: "PreToolUse".to_string(),
                        cmd_suffix: "gemini-beforetool".to_string(),
                    });
                }
                found_beforetool = true;
            }
        }
    }
    if !found_beforetool {
        return Err(VerifyFailReason::HookCommandMissing {
            event: "PreToolUse".to_string(),
            cmd_suffix: "gemini-beforetool".to_string(),
        });
    }

    // Check PostToolUse
    let post_tool_use = lifecycle
        .get("PostToolUse")
        .and_then(|v| v.as_array())
        .ok_or_else(|| VerifyFailReason::HookEventMissing("PostToolUse".to_string()))?;
    let mut found_aftertool = false;
    for matcher_val in post_tool_use {
        let matcher_obj = matcher_val
            .as_object()
            .ok_or_else(|| VerifyFailReason::HookTypeFieldNotCommand("PostToolUse".to_string()))?;
        let matcher_pattern = matcher_obj
            .get("matcher")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if matcher_pattern != ".*" {
            return Err(VerifyFailReason::HookMatcherMismatch {
                event: "PostToolUse".to_string(),
                expected: ".*".to_string(),
                actual: matcher_pattern.to_string(),
            });
        }
        let hooks_arr = matcher_obj
            .get("hooks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| VerifyFailReason::HookTypeFieldNotCommand("PostToolUse".to_string()))?;
        for hook in hooks_arr {
            let hook_obj = hook.as_object().ok_or_else(|| {
                VerifyFailReason::HookTypeFieldNotCommand("PostToolUse".to_string())
            })?;
            let name = hook_obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let command = hook_obj
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let hook_type = hook_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

            if hook_type != "command" {
                return Err(VerifyFailReason::HookTypeFieldNotCommand(
                    "PostToolUse".to_string(),
                ));
            }
            if hook_obj.get("timeout").and_then(|v| v.as_u64()).is_none() {
                return Err(VerifyFailReason::HookTimeoutMissing {
                    event: "PostToolUse".to_string(),
                });
            }

            if name == "hcom-aftertool" {
                if found_aftertool {
                    return Err(VerifyFailReason::HookDuplicated("PostToolUse".to_string()));
                }
                if !command.contains("gemini-aftertool") {
                    return Err(VerifyFailReason::HookCommandMissing {
                        event: "PostToolUse".to_string(),
                        cmd_suffix: "gemini-aftertool".to_string(),
                    });
                }
                found_aftertool = true;
            }
        }
    }
    if !found_aftertool {
        return Err(VerifyFailReason::HookCommandMissing {
            event: "PostToolUse".to_string(),
            cmd_suffix: "gemini-aftertool".to_string(),
        });
    }

    // Check permissions in agy settings.json
    if check_permissions {
        let settings_path = get_antigravity_settings_path();
        if !antigravity_permissions_complete(&settings_path) {
            return Err(VerifyFailReason::PermissionsMissing(settings_path));
        }
    }

    Ok(())
}

/// Path to the Antigravity CLI's settings.json (under `~/.gemini/antigravity-cli/`).
fn get_antigravity_settings_path() -> PathBuf {
    antigravity_settings_path(&crate::runtime_env::gemini_family_config_dir())
}

fn antigravity_settings_path(gemini_dir: &Path) -> PathBuf {
    gemini_dir.join("antigravity-cli").join("settings.json")
}

fn antigravity_cleanup_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut push_unique = |dir: PathBuf| {
        if dir.is_absolute() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };
    if let Some(home) = dirs::home_dir() {
        push_unique(home.join(".gemini"));
    }
    if let Ok(dir) = std::env::var("GEMINI_CLI_HOME")
        && !dir.is_empty()
    {
        // GEMINI_CLI_HOME is a raw prefix; gemini_family_config_dir() appends
        // .gemini, so mirror that here.
        let p = PathBuf::from(dir);
        push_unique(p.join(".gemini"));
    }
    push_unique(crate::runtime_env::gemini_family_config_dir());
    dirs
}

/// Build the list of `command(...)` rules for safe hcom commands.
fn antigravity_permission_rules() -> Vec<String> {
    let mut rules = Vec::new();
    for prefix in &["hcom", "uvx hcom"] {
        for cmd in crate::hooks::common::SAFE_HCOM_COMMANDS {
            rules.push(format!("command({} {})", prefix, cmd));
        }
    }
    rules
}

/// Merge hcom permission rules into `~/.gemini/antigravity-cli/settings.json`.
/// Preserves any other keys and pre-existing entries in `permissions.allow`.
fn setup_antigravity_permissions() -> bool {
    let path = get_antigravity_settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut root = if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str::<Value>(&s)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default(),
            Err(_) => return false,
        }
    } else {
        serde_json::Map::new()
    };

    let permissions = root
        .entry("permissions".to_string())
        .or_insert_with(|| json!({}));
    let permissions_obj = match permissions.as_object_mut() {
        Some(o) => o,
        None => return false,
    };
    let allow = permissions_obj
        .entry("allow".to_string())
        .or_insert_with(|| json!([]));
    let allow_arr = match allow.as_array_mut() {
        Some(a) => a,
        None => return false,
    };

    let wanted = antigravity_permission_rules();
    let mut existing: std::collections::HashSet<String> = allow_arr
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    for rule in &wanted {
        if existing.insert(rule.clone()) {
            allow_arr.push(json!(rule));
        }
    }

    let json_str = match serde_json::to_string_pretty(&Value::Object(root)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    crate::paths::atomic_write(&path, &json_str)
}

/// Remove hcom rules from agy settings.json. Cleans `permissions.allow` and
/// `permissions` if they become empty. Leaves the file otherwise untouched.
fn remove_antigravity_permissions() -> bool {
    remove_antigravity_permissions_at(&get_antigravity_settings_path())
}

fn remove_antigravity_permissions_at(path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let mut val: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let root = match val.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    let wanted: std::collections::HashSet<String> =
        antigravity_permission_rules().into_iter().collect();

    let mut changed = false;
    if let Some(permissions) = root.get_mut("permissions").and_then(|v| v.as_object_mut()) {
        if let Some(allow) = permissions.get_mut("allow").and_then(|v| v.as_array_mut()) {
            let before = allow.len();
            allow.retain(|v| v.as_str().is_none_or(|s| !wanted.contains(s)));
            if allow.len() != before {
                changed = true;
            }
            if allow.is_empty() {
                permissions.remove("allow");
                changed = true;
            }
        }
        if permissions.is_empty() {
            root.remove("permissions");
            changed = true;
        }
    }

    if !changed {
        return true;
    }

    let json_str = match serde_json::to_string_pretty(&val) {
        Ok(s) => s,
        Err(_) => return false,
    };
    crate::paths::atomic_write(path, &json_str)
}

/// Check that every hcom rule we install is present in agy settings.json.
fn antigravity_permissions_complete(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(val) = serde_json::from_str::<Value>(&content) else {
        return false;
    };
    let Some(allow) = val
        .get("permissions")
        .and_then(|p| p.get("allow"))
        .and_then(|a| a.as_array())
    else {
        return false;
    };
    let installed: std::collections::HashSet<&str> =
        allow.iter().filter_map(|v| v.as_str()).collect();
    antigravity_permission_rules()
        .iter()
        .all(|rule| installed.contains(rule.as_str()))
}

/// agy Stop payloads that end a turn but not the session (do not soft-stop).
const AGY_TURN_END_REASONS: &[&str] = &["NO_TOOL_CALL", "NO_TOOL_CALLS"];

/// Antigravity Stop stdin uses `terminationReason`, not Gemini's `reason`.
pub(crate) fn sessionend_reason(raw: &Value) -> String {
    raw.get("terminationReason")
        .or_else(|| raw.get("reason"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "closed".to_string())
}

/// True when agy Stop is turn-end only (session continues), not tab/process teardown.
pub(crate) fn stop_should_skip_soft_finalize(raw: &Value) -> bool {
    if raw.get("fullyIdle").and_then(|v| v.as_bool()) == Some(true) {
        return true;
    }
    let reason = raw
        .get("terminationReason")
        .or_else(|| raw.get("reason"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    AGY_TURN_END_REASONS.contains(&reason.to_ascii_uppercase().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_helpers::{EnvGuard, isolated_test_env};
    use serial_test::serial;

    fn antigravity_test_env() -> (tempfile::TempDir, PathBuf, PathBuf, EnvGuard) {
        let (dir, _hcom_dir, test_home, guard) = isolated_test_env();
        let hooks_path = test_home.join(".gemini").join("config").join("hooks.json");
        (dir, test_home, hooks_path, guard)
    }

    #[test]
    #[serial]
    fn test_setup_creates_all_lifecycle_hooks() {
        let (_dir, _test_home, hooks_path, _guard) = antigravity_test_env();

        assert!(!hooks_path.exists());
        try_setup_antigravity_hooks(false).unwrap();
        assert!(hooks_path.exists());

        // Verify with the validation function
        assert!(verify_antigravity_hooks_installed(false));
    }

    #[test]
    #[serial]
    fn test_setup_preserves_other_groups() {
        let (_dir, _test_home, hooks_path, _guard) = antigravity_test_env();

        // Write a pre-existing custom group
        if let Some(parent) = hooks_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let pre_existing = json!({
            "guard-shell": {
                "PreToolUse": [
                    {
                        "matcher": "run_command",
                        "hooks": [
                            {
                                "name": "guard-shell",
                                "type": "command",
                                "command": "python3 guard.py",
                                "description": "some description",
                                "timeout": 2000
                            }
                        ]
                    }
                ]
            }
        });
        std::fs::write(
            &hooks_path,
            serde_json::to_string_pretty(&pre_existing).unwrap(),
        )
        .unwrap();

        // Setup antigravity hooks
        try_setup_antigravity_hooks(false).unwrap();

        // Read back and verify both exist
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        let root: Value = serde_json::from_str(&content).unwrap();

        assert!(root.get("hcom-lifecycle").is_some());
        assert_eq!(
            root["guard-shell"]["PreToolUse"][0]["hooks"][0]["name"],
            "guard-shell"
        );
    }

    #[test]
    #[serial]
    fn test_setup_idempotent() {
        let (_dir, _test_home, hooks_path, _guard) = antigravity_test_env();

        try_setup_antigravity_hooks(false).unwrap();
        let content1 = std::fs::read_to_string(&hooks_path).unwrap();

        try_setup_antigravity_hooks(false).unwrap();
        let content2 = std::fs::read_to_string(&hooks_path).unwrap();

        assert_eq!(content1, content2);
        assert!(verify_antigravity_hooks_installed(false));
    }

    #[test]
    #[serial]
    fn test_remove_only_hcom_lifecycle() {
        let (_dir, _test_home, hooks_path, _guard) = antigravity_test_env();

        // Setup hooks
        try_setup_antigravity_hooks(false).unwrap();
        assert!(verify_antigravity_hooks_installed(false));

        // Remove hooks
        assert!(remove_antigravity_hooks());
        assert!(!hooks_path.exists()); // Since it was the only group, the file is deleted.

        // Write with multiple groups
        if let Some(parent) = hooks_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let pre_existing = json!({
            "guard-shell": {
                "PreToolUse": []
            }
        });
        std::fs::write(
            &hooks_path,
            serde_json::to_string_pretty(&pre_existing).unwrap(),
        )
        .unwrap();

        try_setup_antigravity_hooks(false).unwrap();
        assert!(remove_antigravity_hooks());

        assert!(hooks_path.exists());
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        let root: Value = serde_json::from_str(&content).unwrap();
        assert!(root.get("hcom-lifecycle").is_none());
        assert!(root.get("guard-shell").is_some());
    }

    #[test]
    #[serial]
    fn test_remove_also_strips_hcom_permissions() {
        let (_dir, test_home, _hooks_path, _guard) = antigravity_test_env();

        // Install hooks WITH permissions
        try_setup_antigravity_hooks(true).unwrap();
        assert!(verify_antigravity_hooks_installed(true));

        // Check permissions were written
        let settings_path = test_home
            .join(".gemini")
            .join("antigravity-cli")
            .join("settings.json");
        assert!(
            settings_path.exists(),
            "settings.json should exist after install"
        );
        let content = std::fs::read_to_string(&settings_path).unwrap();
        let val: Value = serde_json::from_str(&content).unwrap();
        let allow = val["permissions"]["allow"].as_array().unwrap();
        assert!(
            !allow.is_empty(),
            "hcom rules should be present after install"
        );

        // Remove hooks — should also strip hcom permissions
        assert!(remove_antigravity_hooks());

        // Permissions should now be gone
        if settings_path.exists() {
            let content2 = std::fs::read_to_string(&settings_path).unwrap();
            let val2: Value = serde_json::from_str(&content2).unwrap();
            // permissions key should be absent, or allow should not contain hcom rules
            let has_hcom_rules = val2
                .get("permissions")
                .and_then(|p| p.get("allow"))
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .any(|v| v.as_str().is_some_and(|s| s.contains("hcom")))
                })
                .unwrap_or(false);
            assert!(!has_hcom_rules, "hcom permission rules should be removed");
        }
    }

    #[test]
    #[serial]
    fn test_verify_detects_missing_hooks() {
        let (_dir, _test_home, _hooks_path, _guard) = antigravity_test_env();
        // File doesn't exist
        assert!(!verify_antigravity_hooks_installed(false));
    }

    #[test]
    fn test_hook_sh_cmd_includes_subcmd_and_hcom() {
        let cmd = hook_sh_cmd("hcom gemini-beforeagent", "gemini-beforeagent", "");
        assert!(cmd.contains("gemini-beforeagent"));
        assert!(cmd.contains("command -v hcom"));
        assert!(cmd.contains("ANTIGRAVITY_AGENT=1"));
        assert!(cmd.contains("hcom gemini-beforeagent"));
    }

    #[test]
    fn test_hook_sh_cmd_with_fallback_uses_base64_pipeline() {
        let cmd = hook_sh_cmd("hcom", "gemini-beforetool", "{\"decision\":\"allow\"}");
        assert!(cmd.contains("gemini-beforetool"));
        assert!(cmd.contains("base64 -d"));
    }

    #[test]
    fn test_hook_sh_cmd_without_fallback_exits_zero_only() {
        let cmd = hook_sh_cmd("hcom", "gemini-afteragent", "");
        // no printf/echo when fallback is empty
        assert!(!cmd.contains("printf"));
        assert!(!cmd.contains("base64"));
        assert!(cmd.contains("exit 0"));
    }

    /// Actually execute the generated command with a missing binary and confirm
    /// the fallback JSON survives the inner shell pass (no quote stripping).
    #[cfg(unix)]
    #[test]
    fn test_hook_sh_cmd_fallback_emits_valid_json_when_bin_missing() {
        use std::process::Command;
        // Reference an obviously-missing binary so the `||` fallback branch fires.
        let cmd = hook_sh_cmd(
            "definitely_missing_hcom_xyz123",
            "gemini-beforetool",
            "{\"decision\":\"allow\"}",
        );
        let out = Command::new("sh").arg("-c").arg(&cmd).output().unwrap();
        assert!(
            out.status.success(),
            "shell exited non-zero: {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
            panic!("fallback stdout is not valid JSON: stdout={stdout:?} err={e}")
        });
        assert_eq!(
            parsed.get("decision").and_then(|v| v.as_str()),
            Some("allow")
        );
    }

    /// Same but with an apostrophe in the fallback to exercise the inner-quote escape.
    #[cfg(unix)]
    #[test]
    fn test_hook_sh_cmd_fallback_handles_apostrophe_in_payload() {
        use std::process::Command;
        let payload = "{\"reason\":\"don't allow\"}";
        let cmd = hook_sh_cmd(
            "definitely_missing_hcom_xyz123",
            "gemini-beforetool",
            payload,
        );
        let out = Command::new("sh").arg("-c").arg(&cmd).output().unwrap();
        assert!(out.status.success());
        let stdout = String::from_utf8(out.stdout).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
            panic!("fallback stdout is not valid JSON: stdout={stdout:?} err={e}")
        });
        assert_eq!(
            parsed.get("reason").and_then(|v| v.as_str()),
            Some("don't allow")
        );
    }

    #[test]
    fn test_sessionstart_cmd_invokes_hcom_with_env() {
        let cmd = hook_sessionstart_cmd("hcom");
        assert!(cmd.contains("gemini-sessionstart"));
        assert!(cmd.contains("ANTIGRAVITY_AGENT=1"));
        // Lockfile machinery removed — sessionstart is idempotent via name_announced.
        assert!(!cmd.contains("mkdir"));
        assert!(!cmd.contains("parent_pid="));
    }

    #[test]
    #[serial]
    fn test_setup_rejects_invalid_existing_hooks_json() {
        let (_dir, _test_home, hooks_path, _guard) = antigravity_test_env();
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::write(&hooks_path, "{not json").unwrap();

        let err = try_setup_antigravity_hooks(false).unwrap_err();
        assert!(matches!(err, SetupError::ExistingParseFailed { .. }));
    }

    #[test]
    #[serial]
    fn test_setup_rejects_non_object_existing_hooks_json() {
        let (_dir, _test_home, hooks_path, _guard) = antigravity_test_env();
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::write(&hooks_path, "[]").unwrap();

        let err = try_setup_antigravity_hooks(false).unwrap_err();
        assert!(matches!(err, SetupError::ExistingRootNotObject { .. }));
    }

    #[test]
    #[serial]
    fn test_remove_reports_invalid_existing_hooks_json() {
        let (_dir, _test_home, hooks_path, _guard) = antigravity_test_env();
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::write(&hooks_path, "{not json").unwrap();

        assert!(!remove_antigravity_hooks());
    }

    // Unix-only: redirects the home dir via $HOME, but on Windows
    // `dirs::home_dir()` reads USERPROFILE and ignores it, so the home-based
    // cleanup dir points outside the test's temp tree.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn test_remove_cleans_default_and_active_hcom_dir_local_paths() {
        let _guard = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("HCOM_DIR", workspace.join(".hcom"));
            std::env::remove_var("GEMINI_CLI_HOME");
        }
        let gemini_dirs = [home.join(".gemini"), workspace.join(".gemini")];
        for gemini_dir in &gemini_dirs {
            let hooks = antigravity_hooks_path(gemini_dir);
            let settings = antigravity_settings_path(gemini_dir);
            std::fs::create_dir_all(hooks.parent().unwrap()).unwrap();
            std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
            std::fs::write(
                &hooks,
                serde_json::to_string_pretty(&json!({
                    "hcom-lifecycle": {},
                    "custom": true
                }))
                .unwrap(),
            )
            .unwrap();
            std::fs::write(
                &settings,
                serde_json::to_string_pretty(&json!({
                    "permissions": {
                        "allow": ["command(hcom send)", "custom"]
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        }

        assert!(remove_antigravity_hooks());

        for gemini_dir in gemini_dirs {
            let hooks: Value = serde_json::from_str(
                &std::fs::read_to_string(antigravity_hooks_path(&gemini_dir)).unwrap(),
            )
            .unwrap();
            assert_eq!(hooks, json!({ "custom": true }));
            let settings: Value = serde_json::from_str(
                &std::fs::read_to_string(antigravity_settings_path(&gemini_dir)).unwrap(),
            )
            .unwrap();
            assert_eq!(settings["permissions"]["allow"], json!(["custom"]));
        }
    }

    #[test]
    fn test_sessionend_reason_from_termination_reason() {
        let raw = json!({
            "terminationReason": "USER_CANCEL",
            "fullyIdle": true
        });
        assert_eq!(sessionend_reason(&raw), "user_cancel");
        assert!(stop_should_skip_soft_finalize(&raw));
    }

    #[test]
    fn test_sessionend_reason_defaults_closed() {
        let raw = json!({ "fullyIdle": false });
        assert_eq!(sessionend_reason(&raw), "closed");
        assert!(!stop_should_skip_soft_finalize(&raw));
    }

    #[test]
    fn test_no_tool_call_skips_soft_finalize_when_not_fully_idle() {
        let raw = json!({
            "terminationReason": "NO_TOOL_CALL",
            "fullyIdle": false
        });
        assert!(stop_should_skip_soft_finalize(&raw));
    }

    #[test]
    fn test_real_teardown_does_not_skip_on_unknown_reason() {
        let raw = json!({
            "terminationReason": "USER_CLOSED",
            "fullyIdle": false
        });
        assert!(!stop_should_skip_soft_finalize(&raw));
    }
}
