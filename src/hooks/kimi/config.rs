use std::path::{Path, PathBuf};

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table};

use crate::hooks::common;

pub const HOOK_TIMEOUT_SECS: i64 = 30;
pub(crate) const KIMI_HOOK_COMMANDS: &[(&str, &str)] = &[
    ("SessionStart", "kimi-sessionstart"),
    ("UserPromptSubmit", "kimi-userpromptsubmit"),
    ("PreToolUse", "kimi-pretooluse"),
    ("PostToolUse", "kimi-posttooluse"),
    ("PermissionRequest", "kimi-permissionrequest"),
    ("PermissionResult", "kimi-permissionresult"),
    ("Stop", "kimi-stop"),
    ("SessionEnd", "kimi-sessionend"),
    ("SubagentStart", "kimi-subagentstart"),
    ("SubagentStop", "kimi-subagentstop"),
    ("Notification", "kimi-notification"),
];

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("existing Kimi config at {} could not be read: {source}", path.display())]
    ExistingReadFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("existing Kimi config at {} is not valid TOML: {source}", path.display())]
    ExistingParseFailed {
        path: PathBuf,
        #[source]
        source: toml_edit::TomlError,
    },
    #[error("failed to create Kimi config directory {}: {source}", path.display())]
    DirCreateFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("atomic write to {} failed: {source}", path.display())]
    AtomicWriteFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("post-write Kimi hook verification failed for {}", .0.display())]
    PostWriteVerifyFailed(PathBuf),
}

/// Kimi's data root: config.toml, sessions, credentials all live here.
/// Overridden via `KIMI_CODE_HOME` (kimi does not honor any other dir variable),
/// defaulting to `~/.kimi-code`.
pub(crate) fn kimi_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("KIMI_CODE_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    crate::runtime_env::tool_config_root().join(".kimi-code")
}

pub fn get_kimi_settings_path() -> PathBuf {
    kimi_config_dir().join("config.toml")
}

pub(crate) fn build_kimi_hook_command(command: &str) -> String {
    let mut parts = crate::runtime_env::get_hcom_prefix();
    parts.push(command.to_string());
    parts.join(" ")
}

pub(crate) fn is_hcom_kimi_command(command: &str) -> bool {
    let trimmed = command.trim();
    ["hcom", "uvx hcom"].iter().any(|prefix| {
        KIMI_HOOK_COMMANDS
            .iter()
            .any(|(_, suffix)| trimmed == format!("{prefix} {suffix}"))
    })
}

fn read_toml_document(path: &Path) -> Result<DocumentMut, SetupError> {
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    let content =
        std::fs::read_to_string(path).map_err(|source| SetupError::ExistingReadFailed {
            path: path.to_path_buf(),
            source,
        })?;
    content
        .parse::<DocumentMut>()
        .map_err(|source| SetupError::ExistingParseFailed {
            path: path.to_path_buf(),
            source,
        })
}

fn write_toml(path: &Path, doc: &DocumentMut) -> Result<(), SetupError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| SetupError::DirCreateFailed {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let content = doc.to_string();
    crate::paths::atomic_write_io(path, &content).map_err(|source| SetupError::AtomicWriteFailed {
        path: path.to_path_buf(),
        source,
    })
}

pub(crate) fn merge_hcom_hooks(doc: &mut DocumentMut) {
    let hooks_item = doc
        .entry("hooks")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));

    if let Item::ArrayOfTables(arr) = hooks_item {
        let mut filtered = ArrayOfTables::new();
        for i in 0..arr.len() {
            if let Some(table) = arr.get(i) {
                let keep = table
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(|cmd| !is_hcom_kimi_command(cmd))
                    .unwrap_or(true);
                if keep {
                    filtered.push(table.clone());
                }
            }
        }
        *arr = filtered;

        for (event, command_suffix) in KIMI_HOOK_COMMANDS {
            let mut table = Table::new();
            table.insert("event", toml_edit::value(*event));
            table.insert(
                "command",
                toml_edit::value(build_kimi_hook_command(command_suffix)),
            );
            table.insert("timeout", toml_edit::value(HOOK_TIMEOUT_SECS));
            arr.push(table);
        }
    }
}

fn remove_hcom_hooks(doc: &mut DocumentMut) {
    let Some(hooks_item) = doc.get_mut("hooks") else {
        return;
    };
    let Item::ArrayOfTables(arr) = hooks_item else {
        return;
    };
    let mut filtered = ArrayOfTables::new();
    for i in 0..arr.len() {
        if let Some(table) = arr.get(i) {
            let keep = table
                .get("command")
                .and_then(|v| v.as_str())
                .map(|cmd| !is_hcom_kimi_command(cmd))
                .unwrap_or(true);
            if keep {
                filtered.push(table.clone());
            }
        }
    }
    *arr = filtered;
    if arr.is_empty() {
        doc.remove("hooks");
    }
}

/// Allow-rule patterns hcom installs (current hcom command prefix).
pub(crate) fn kimi_permission_patterns() -> Vec<String> {
    let prefix = crate::runtime_env::build_hcom_command();
    common::SAFE_HCOM_COMMANDS
        .iter()
        .map(|command| format!("Bash({prefix} {command}*)"))
        .collect()
}

/// All patterns hcom may have written (both `hcom` and `uvx hcom` prefixes), so
/// removal/re-merge can recognize and strip stale managed rules.
fn all_kimi_permission_patterns() -> Vec<String> {
    let mut patterns = Vec::new();
    for prefix in ["hcom", "uvx hcom"] {
        for command in common::SAFE_HCOM_COMMANDS {
            patterns.push(format!("Bash({prefix} {command}*)"));
        }
    }
    patterns
}

fn is_hcom_permission_pattern(pattern: &str) -> bool {
    all_kimi_permission_patterns()
        .iter()
        .any(|managed| managed == pattern)
}

/// Get a `&mut ArrayOfTables` for `[[permission.rules]]`, creating the parent
/// `[permission]` table on demand. Returns `None` if `permission`/`rules` exist
/// but are not tables of the expected shape (leave a user's odd config alone).
fn permission_rules_mut(doc: &mut DocumentMut) -> Option<&mut ArrayOfTables> {
    let permission = doc
        .entry("permission")
        .or_insert_with(|| Item::Table(Table::new()));
    let Item::Table(permission) = permission else {
        return None;
    };
    let rules = permission
        .entry("rules")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    match rules {
        Item::ArrayOfTables(arr) => Some(arr),
        _ => None,
    }
}

pub(crate) fn merge_hcom_permissions(doc: &mut DocumentMut) {
    let Some(arr) = permission_rules_mut(doc) else {
        return;
    };

    // Rebuild with hcom allow-rules first (first-match-wins ordering), then the
    // user's existing non-hcom rules. This makes the merge idempotent and keeps
    // hcom's allows ahead of any broad user `ask`/`deny` on `Bash`.
    let mut rebuilt = ArrayOfTables::new();
    for pattern in kimi_permission_patterns() {
        let mut table = Table::new();
        table.insert("decision", toml_edit::value("allow"));
        table.insert("pattern", toml_edit::value(pattern));
        table.insert("reason", toml_edit::value("hcom auto-approve"));
        rebuilt.push(table);
    }
    for i in 0..arr.len() {
        if let Some(table) = arr.get(i) {
            let is_managed = table
                .get("pattern")
                .and_then(|v| v.as_str())
                .map(is_hcom_permission_pattern)
                .unwrap_or(false);
            if !is_managed {
                rebuilt.push(table.clone());
            }
        }
    }
    *arr = rebuilt;
}

pub(crate) fn remove_hcom_permissions(doc: &mut DocumentMut) {
    let Some(Item::Table(permission)) = doc.get_mut("permission") else {
        return;
    };
    if let Some(Item::ArrayOfTables(arr)) = permission.get_mut("rules") {
        let mut filtered = ArrayOfTables::new();
        for i in 0..arr.len() {
            if let Some(table) = arr.get(i) {
                let is_managed = table
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .map(is_hcom_permission_pattern)
                    .unwrap_or(false);
                if !is_managed {
                    filtered.push(table.clone());
                }
            }
        }
        *arr = filtered;
        if arr.is_empty() {
            permission.remove("rules");
        }
    }
    if permission.is_empty() {
        doc.remove("permission");
    }
}

fn verify_permissions_at(path: &Path) -> bool {
    let Ok(doc) = read_toml_document(path) else {
        return false;
    };
    let Some(Item::Table(permission)) = doc.get("permission") else {
        return false;
    };
    let Some(Item::ArrayOfTables(arr)) = permission.get("rules") else {
        return false;
    };
    let present: Vec<&str> = (0..arr.len())
        .filter_map(|i| arr.get(i))
        .filter(|t| t.get("decision").and_then(|v| v.as_str()) == Some("allow"))
        .filter_map(|t| t.get("pattern").and_then(|v| v.as_str()))
        .collect();
    kimi_permission_patterns()
        .iter()
        .all(|expected| present.iter().any(|p| p == expected))
}

fn verify_hooks_at(path: &Path) -> bool {
    let Ok(doc) = read_toml_document(path) else {
        return false;
    };
    let Some(Item::ArrayOfTables(arr)) = doc.get("hooks") else {
        return false;
    };
    KIMI_HOOK_COMMANDS.iter().all(|(event, command_suffix)| {
        let expected_cmd = build_kimi_hook_command(command_suffix);
        (0..arr.len()).any(|i| {
            arr.get(i).is_some_and(|table| {
                table.get("event").and_then(|v| v.as_str()) == Some(*event)
                    && table.get("command").and_then(|v| v.as_str()) == Some(&expected_cmd)
                    && table.get("timeout").and_then(|v| v.as_integer()).is_some()
            })
        })
    })
}

pub fn remove_kimi_hooks() -> bool {
    let path = get_kimi_settings_path();
    if !path.exists() {
        return true;
    }
    match read_toml_document(&path) {
        Ok(mut doc) => {
            remove_hcom_hooks(&mut doc);
            remove_hcom_permissions(&mut doc);
            write_toml(&path, &doc).is_ok()
        }
        Err(_) => false,
    }
}

pub fn try_setup_kimi_hooks(include_permissions: bool) -> Result<(), SetupError> {
    let path = get_kimi_settings_path();
    let mut doc = read_toml_document(&path)?;
    merge_hcom_hooks(&mut doc);
    if include_permissions {
        merge_hcom_permissions(&mut doc);
    } else {
        remove_hcom_permissions(&mut doc);
    }
    write_toml(&path, &doc)?;
    if !verify_hooks_at(&path) {
        return Err(SetupError::PostWriteVerifyFailed(path.clone()));
    }
    if include_permissions && !verify_permissions_at(&path) {
        return Err(SetupError::PostWriteVerifyFailed(path));
    }
    Ok(())
}

pub fn verify_kimi_hooks_installed(check_permissions: bool) -> bool {
    let path = get_kimi_settings_path();
    verify_hooks_at(&path) && (!check_permissions || verify_permissions_at(&path))
}
