use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum VerifyFailReason {
    #[error("hooks.json settings unreadable or empty")]
    SettingsUnreadableOrEmpty,
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

/// Helper to get the Gemini configuration directory.
pub fn gemini_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("GEMINI_CLI_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join(".gemini");
    }
    dirs::home_dir()
        .map(|h| h.join(".gemini"))
        .unwrap_or_else(|| PathBuf::from(".gemini"))
}

/// Resolve the path to the Antigravity `hooks.json` file.
/// Under the split-config design, this resides at `~/.gemini/config/hooks.json`.
pub fn get_antigravity_hooks_path() -> PathBuf {
    gemini_config_dir().join("config").join("hooks.json")
}

/// Try to set up Antigravity hooks in `hooks.json`.
/// Reads existing hooks.json, merges "hcom-lifecycle" group, and preserves all other keys.
pub fn try_setup_antigravity_hooks(_include_permissions: bool) -> Result<(), SetupError> {
    let hooks_path = get_antigravity_hooks_path();
    if let Some(parent) = hooks_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Load existing hooks or initialize an empty map
    let mut hooks_root = if hooks_path.exists() {
        let content = std::fs::read_to_string(&hooks_path).unwrap_or_default();
        serde_json::from_str::<serde_json::Map<String, Value>>(&content).unwrap_or_default()
    } else {
        serde_json::Map::new()
    };

    let hcom_cmd = crate::runtime_env::build_hcom_command();
    let bin = hcom_cmd.split_whitespace().next().unwrap_or("hcom");

    let hcom_lifecycle = json!({
        "PreInvocation": [
            {
                "name": "hcom-sessionstart",
                "type": "command",
                "command": format!("sh -c 'if [ ! -f /tmp/agy-session-$PPID.lock ]; then touch /tmp/agy-session-$PPID.lock && command -v {bin} >/dev/null 2>&1 && exec {hcom_cmd} gemini-sessionstart || exit 0; fi'"),
                "timeout": 5000,
                "description": "Initialize hcom session"
            },
            {
                "name": "hcom-beforeagent",
                "type": "command",
                "command": format!("sh -c 'command -v {bin} >/dev/null 2>&1 && exec {hcom_cmd} gemini-beforeagent || exit 0'"),
                "timeout": 5000,
                "description": "Deliver pending messages"
            }
        ],
        "PostInvocation": [
            {
                "name": "hcom-afteragent",
                "type": "command",
                "command": format!("sh -c 'command -v {bin} >/dev/null 2>&1 && exec {hcom_cmd} gemini-afteragent || exit 0'"),
                "timeout": 5000,
                "description": "Signal ready for messages"
            }
        ],
        "Stop": [
            {
                "name": "hcom-partner-teardown",
                "type": "command",
                "command": format!("sh -c 'command -v {bin} >/dev/null 2>&1 && exec {hcom_cmd} run partner-teardown || exit 0'"),
                "timeout": 5000,
                "description": "Teardown partner session"
            },
            {
                "name": "hcom-sessionend",
                "type": "command",
                "command": format!("sh -c 'command -v {bin} >/dev/null 2>&1 && exec {hcom_cmd} gemini-sessionend || exit 0'"),
                "timeout": 5000,
                "description": "Disconnect from hcom"
            },
            {
                "name": "hcom-lockfile-cleanup",
                "type": "command",
                "command": "rm -f /tmp/agy-session-$PPID.lock",
                "timeout": 5000,
                "description": "Clean up session lockfile"
            }
        ],
        "PreToolUse": [
            {
                "matcher": ".*",
                "hooks": [
                    {
                        "name": "hcom-beforetool",
                        "type": "command",
                        "command": format!("sh -c 'command -v {bin} >/dev/null 2>&1 && exec {hcom_cmd} gemini-beforetool || exit 0'"),
                        "timeout": 5000,
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
                        "command": format!("sh -c 'command -v {bin} >/dev/null 2>&1 && exec {hcom_cmd} gemini-aftertool || exit 0'"),
                        "timeout": 5000,
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

    verify_hooks_at(&hooks_path).map_err(|reason| SetupError::PostWriteVerifyFailed {
        path: hooks_path,
        reason,
    })?;

    Ok(())
}

/// Verify if Antigravity hooks are correctly installed.
pub fn verify_antigravity_hooks_installed(_check_permissions: bool) -> bool {
    verify_hooks_at(&get_antigravity_hooks_path()).is_ok()
}

/// Cleanly remove the `"hcom-lifecycle"` group key from `hooks.json`.
/// Preserves other keys, and removes the file if no other keys remain.
pub fn remove_antigravity_hooks() -> bool {
    let path = get_antigravity_hooks_path();
    if !path.exists() {
        return true;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return true,
    };
    let mut val: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return true,
    };
    let obj = match val.as_object_mut() {
        Some(o) => o,
        None => return true,
    };
    obj.remove("hcom-lifecycle");
    if obj.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        let json_str = match serde_json::to_string_pretty(&Value::Object(obj.clone())) {
            Ok(s) => s,
            Err(_) => return false,
        };
        if crate::paths::atomic_write_io(&path, &json_str).is_err() {
            return false;
        }
    }
    true
}

fn verify_hooks_at(path: &Path) -> Result<(), VerifyFailReason> {
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
            if !command.contains("gemini-sessionstart")
                || !command.contains("agy-session-$PPID.lock")
            {
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
    let mut found_partner_teardown = false;
    let mut found_sessionend = false;
    let mut found_lockfile_cleanup = false;
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

        if name == "hcom-partner-teardown" {
            if found_partner_teardown {
                return Err(VerifyFailReason::HookDuplicated("Stop".to_string()));
            }
            if !command.contains("partner-teardown") {
                return Err(VerifyFailReason::HookCommandMissing {
                    event: "Stop".to_string(),
                    cmd_suffix: "partner-teardown".to_string(),
                });
            }
            found_partner_teardown = true;
        } else if name == "hcom-sessionend" {
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
        } else if name == "hcom-lockfile-cleanup" {
            if found_lockfile_cleanup {
                return Err(VerifyFailReason::HookDuplicated("Stop".to_string()));
            }
            if !command.contains("agy-session-$PPID.lock") {
                return Err(VerifyFailReason::HookCommandMissing {
                    event: "Stop".to_string(),
                    cmd_suffix: "agy-session-$PPID.lock".to_string(),
                });
            }
            found_lockfile_cleanup = true;
        }
    }
    if !found_partner_teardown {
        return Err(VerifyFailReason::HookCommandMissing {
            event: "Stop".to_string(),
            cmd_suffix: "partner-teardown".to_string(),
        });
    }
    if !found_sessionend {
        return Err(VerifyFailReason::HookCommandMissing {
            event: "Stop".to_string(),
            cmd_suffix: "gemini-sessionend".to_string(),
        });
    }
    if !found_lockfile_cleanup {
        return Err(VerifyFailReason::HookCommandMissing {
            event: "Stop".to_string(),
            cmd_suffix: "agy-session-$PPID.lock".to_string(),
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

    Ok(())
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
    fn test_verify_detects_missing_hooks() {
        let (_dir, _test_home, _hooks_path, _guard) = antigravity_test_env();
        // File doesn't exist
        assert!(!verify_antigravity_hooks_installed(false));
    }
}
