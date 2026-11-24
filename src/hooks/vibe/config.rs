//! `$VIBE_HOME/hooks.toml` management.
//!
//! Vibe has no per-run hooks flag, so hcom keeps its rows in the user hooks
//! file (like Kimi). The handlers do nothing unless Vibe was launched by hcom.

use std::path::{Path, PathBuf};

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table};

pub const HOOK_TIMEOUT_SECS: i64 = 30;

/// `(row name, Vibe hook type, hcom hook command)`. Row names must be unique
/// across Vibe's hooks files.
pub(crate) const VIBE_HOOK_COMMANDS: &[(&str, &str, &str)] = &[
    ("hcom-session-start", "session_start", "vibe-sessionstart"),
    ("hcom-pre-agent", "pre_agent", "vibe-preagent"),
    ("hcom-pre-tool", "pre_tool", "vibe-pretool"),
    ("hcom-post-tool", "post_tool", "vibe-posttool"),
    ("hcom-notification", "notification", "vibe-notification"),
    ("hcom-session-end", "session_end", "vibe-sessionend"),
];

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("existing Vibe hooks file at {} could not be read: {source}", path.display())]
    ExistingReadFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("existing Vibe hooks file at {} is not valid TOML: {source}", path.display())]
    ExistingParseFailed {
        path: PathBuf,
        #[source]
        source: toml_edit::TomlError,
    },
    #[error("failed to create Vibe config directory {}: {source}", path.display())]
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
    #[error("post-write Vibe hook verification failed for {}", .0.display())]
    PostWriteVerifyFailed(PathBuf),
}

/// Vibe's data root (`VIBE_HOME`, default `~/.vibe`).
pub(crate) fn vibe_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("VIBE_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    crate::runtime_env::tool_home().join(".vibe")
}

pub fn get_vibe_hooks_path() -> PathBuf {
    vibe_config_dir().join("hooks.toml")
}

pub(crate) fn build_vibe_hook_command(command: &str) -> String {
    let mut parts = crate::runtime_env::get_hcom_prefix();
    parts.push(command.to_string());
    parts.join(" ")
}

fn is_hcom_vibe_command(command: &str) -> bool {
    let suffixes: Vec<&str> = VIBE_HOOK_COMMANDS.iter().map(|(_, _, c)| *c).collect();
    crate::hooks::runtime::is_hcom_command(command, &suffixes)
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
    crate::paths::atomic_write_io(path, &doc.to_string()).map_err(|source| {
        SetupError::AtomicWriteFailed {
            path: path.to_path_buf(),
            source,
        }
    })
}

/// Drop hcom's rows from `[[hooks]]`, keeping every user row in order.
fn strip_hcom_rows(arr: &mut ArrayOfTables) {
    let mut kept = ArrayOfTables::new();
    for table in arr.iter() {
        let is_hcom = table
            .get("command")
            .and_then(|v| v.as_str())
            .is_some_and(is_hcom_vibe_command);
        if !is_hcom {
            kept.push(table.clone());
        }
    }
    *arr = kept;
}

pub(crate) fn merge_hcom_hooks(doc: &mut DocumentMut) {
    let hooks = doc
        .entry("hooks")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let Item::ArrayOfTables(arr) = hooks else {
        return;
    };
    strip_hcom_rows(arr);
    for (name, hook_type, command) in VIBE_HOOK_COMMANDS {
        let mut table = Table::new();
        table.insert("name", toml_edit::value(*name));
        table.insert("type", toml_edit::value(*hook_type));
        table.insert(
            "command",
            toml_edit::value(build_vibe_hook_command(command)),
        );
        table.insert("timeout", toml_edit::value(HOOK_TIMEOUT_SECS));
        arr.push(table);
    }
}

pub(crate) fn remove_hcom_hooks(doc: &mut DocumentMut) {
    let Some(Item::ArrayOfTables(arr)) = doc.get_mut("hooks") else {
        return;
    };
    strip_hcom_rows(arr);
    if arr.is_empty() {
        doc.remove("hooks");
    }
}

pub(crate) fn verify_hooks_at(path: &Path) -> bool {
    let Ok(doc) = read_toml_document(path) else {
        return false;
    };
    let Some(Item::ArrayOfTables(arr)) = doc.get("hooks") else {
        return false;
    };
    VIBE_HOOK_COMMANDS.iter().all(|(name, hook_type, command)| {
        let expected = build_vibe_hook_command(command);
        arr.iter().any(|table| {
            table.get("name").and_then(|v| v.as_str()) == Some(*name)
                && table.get("type").and_then(|v| v.as_str()) == Some(*hook_type)
                && table.get("command").and_then(|v| v.as_str()) == Some(expected.as_str())
        })
    })
}

/// Cleans ~/.vibe, $VIBE_HOME and legacy `<HCOM_DIR parent>/.vibe`.
pub fn remove_vibe_hooks() -> bool {
    crate::runtime_env::tool_config_cleanup_dirs(".vibe", "VIBE_HOME")
        .iter()
        .filter(|dir| !remove_vibe_hooks_at(&dir.join("hooks.toml")))
        .count()
        == 0
}

fn remove_vibe_hooks_at(path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    match read_toml_document(path) {
        Ok(mut doc) => {
            remove_hcom_hooks(&mut doc);
            write_toml(path, &doc).is_ok()
        }
        Err(_) => false,
    }
}

/// Vibe has no permission rules hcom could add, so `_include_permissions` is
/// ignored.
pub fn try_setup_vibe_hooks(_include_permissions: bool) -> Result<(), SetupError> {
    let path = get_vibe_hooks_path();
    let mut doc = read_toml_document(&path)?;
    merge_hcom_hooks(&mut doc);
    write_toml(&path, &doc)?;
    if !verify_hooks_at(&path) {
        return Err(SetupError::PostWriteVerifyFailed(path));
    }
    Ok(())
}

pub fn verify_vibe_hooks_installed(_check_permissions: bool) -> bool {
    verify_hooks_at(&get_vibe_hooks_path())
}
