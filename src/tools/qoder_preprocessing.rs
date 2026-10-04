//! Qoder launch preprocessing: workspace trust.
//!
//! In a folder that is not trusted, Qoder shows a "Do you trust the files in
//! this folder?" prompt and, until it is accepted, runs no `--settings` hooks
//! (SessionStart never fires), so hcom could not bind the instance. Accepting
//! the prompt appends the folder to `permissions.trustDirectories` in
//! `<config dir>/settings.json`; this module does the same ahead of launch.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::{Value, json};

/// Apply hcom's explicit system prompt and persist it with the launch args.
pub(crate) fn apply_system_prompt(args: &mut Vec<String>, prompt: Option<&str>) {
    if let Some(prompt) = prompt {
        crate::hooks::runtime::take_flag_values(args, &["--system-prompt"]);
        crate::hooks::runtime::insert_before_separator(
            args,
            ["--system-prompt".to_string(), prompt.to_string()],
        );
    }
}

/// Last value of `--flag value` / `--flag=value` before any `--` separator.
fn flag_value<'a>(args: &'a [String], names: &[&str]) -> Option<&'a str> {
    let mut found = None;
    let mut i = 0;
    while i < args.len() && args[i] != "--" {
        let arg = args[i].as_str();
        if names.contains(&arg) {
            if let Some(value) = args.get(i + 1) {
                found = Some(value.as_str());
                i += 1;
            }
        } else if let Some((name, value)) = arg.split_once('=')
            && names.contains(&name)
        {
            found = Some(value);
        }
        i += 1;
    }
    found
}

/// Qoder's user config dir for this launch: `--config-dir`, then
/// `QODER_CONFIG_DIR` from the child's environment, then `~/.qoder`. A relative
/// value resolves against the launch dir, where qodercli starts.
fn qoder_config_dir(
    launch_dir: &Path,
    args: &[String],
    child_env: &HashMap<String, String>,
) -> PathBuf {
    let from_env = child_env
        .get("QODER_CONFIG_DIR")
        .cloned()
        .or_else(|| std::env::var("QODER_CONFIG_DIR").ok());
    match flag_value(args, &["--config-dir"])
        .map(str::to_string)
        .or(from_env)
        .filter(|dir| !dir.is_empty())
    {
        Some(dir) => launch_dir.join(dir),
        None => crate::runtime_env::tool_home().join(".qoder"),
    }
}

/// The folder Qoder treats as its workspace: `--cwd`/`-w` (relative to the
/// launch dir) when given, else the launch dir.
fn qoder_workspace(launch_dir: &Path, args: &[String]) -> PathBuf {
    match flag_value(args, &["--cwd", "-w"]) {
        Some(dir) if !dir.is_empty() => launch_dir.join(dir),
        _ => launch_dir.to_path_buf(),
    }
}

/// Compute the updated `settings.json` text that lists `folder` in
/// `permissions.trustDirectories`, keeping every other key. Returns `None` when
/// the folder or one of its ancestors is already listed (no write needed).
///
/// serde_json orders keys alphabetically; Qoder writes its own settings file
/// the same way (sorted keys, two-space indent, no trailing newline), so a
/// file Qoder wrote round-trips byte for byte apart from the added entry.
fn settings_with_trusted_folder(existing: &str, folder: &str) -> anyhow::Result<Option<String>> {
    let mut root: serde_json::Map<String, Value> = if existing.trim().is_empty() {
        serde_json::Map::new()
    } else {
        serde_json::from_str::<Value>(existing)
            .context("invalid Qoder settings.json")?
            .as_object()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Qoder settings.json must be a JSON object"))?
    };
    let permissions = root
        .entry("permissions".to_string())
        .or_insert_with(|| json!({}));
    let permissions = permissions
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Qoder settings.json `permissions` must be an object"))?;
    let trusted = permissions
        .entry("trustDirectories".to_string())
        .or_insert_with(|| json!([]));
    let trusted = trusted.as_array_mut().ok_or_else(|| {
        anyhow::anyhow!("Qoder settings.json `permissions.trustDirectories` must be an array")
    })?;
    // Qoder trusts every folder below a listed one, so an ancestor entry is
    // enough and the list does not grow by one entry per launch directory.
    let covered = trusted
        .iter()
        .filter_map(Value::as_str)
        .any(|entry| !entry.is_empty() && Path::new(folder).starts_with(entry));
    if covered {
        return Ok(None);
    }
    trusted.push(Value::String(folder.to_string()));
    Ok(Some(serde_json::to_string_pretty(&Value::Object(root))?))
}

/// Serialize hcom's own updates of Qoder's settings file, so two launches
/// trusting different folders at once don't overwrite each other's entry.
/// Held until the returned file drops. Qoder's own writes are outside its reach.
fn trust_lock() -> anyhow::Result<std::fs::File> {
    let path = crate::paths::hcom_path(&[".tmp", "qoder_trust.lock"]);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    crate::sys::fs::lock_exclusive(&file).context("failed to lock Qoder trust update")?;
    Ok(file)
}

fn read_settings(path: &Path) -> anyhow::Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    std::fs::read_to_string(path)
        .with_context(|| format!("failed to read Qoder settings {}", path.display()))
}

/// Pre-seed Qoder's workspace trust list for PTY launches. `args` and
/// `child_env` are the launch's, so the entry lands in the settings file and
/// for the folder that this qodercli will actually use.
pub(crate) fn ensure_qoder_workspace_trusted(
    launch_dir: &Path,
    args: &[String],
    child_env: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let workspace = qoder_workspace(launch_dir, args);
    let normalized = workspace.canonicalize().unwrap_or(workspace);
    let normalized_str = normalized.to_string_lossy().to_string();
    let path = qoder_config_dir(launch_dir, args, child_env).join("settings.json");

    // Common case: already trusted, nothing to write and no lock to take.
    if settings_with_trusted_folder(&read_settings(&path)?, &normalized_str)?.is_none() {
        return Ok(());
    }
    // Re-read under the lock: another launch may have written in between.
    let _lock = trust_lock()?;
    let Some(updated) = settings_with_trusted_folder(&read_settings(&path)?, &normalized_str)?
    else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    eprintln!(
        "[hcom] Auto-approving Qoder folder trust prompt for {} (config: {})",
        normalized.display(),
        path.display()
    );
    crate::paths::atomic_write_io(&path, &updated)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_replaces_native_value_before_separator() {
        let mut args = ["--system-prompt=old", "--model", "m", "--", "query"]
            .map(String::from)
            .to_vec();
        apply_system_prompt(&mut args, Some("new prompt"));
        assert_eq!(
            args,
            [
                "--model",
                "m",
                "--system-prompt",
                "new prompt",
                "--",
                "query"
            ]
            .map(String::from)
        );
        let saved = args.clone();
        apply_system_prompt(&mut args, None);
        assert_eq!(args, saved);
    }

    /// The shape Qoder itself writes (sorted keys, 2-space indent, no trailing newline).
    const QODER_WRITTEN: &str = r#"{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "command": "bash '/h/.qoder/hooks/x.sh' session",
            "timeout": 10,
            "type": "command"
          }
        ],
        "matcher": "*"
      }
    ]
  },
  "model": {
    "name": "m"
  },
  "permissions": {
    "additionalDirectories": [],
    "trustDirectories": [
      "/home/u"
    ]
  },
  "security": {
    "auth": {
      "selectedType": "qoder-browser"
    }
  }
}"#;

    #[test]
    fn trust_write_creates_permissions_in_empty_settings() {
        let out = settings_with_trusted_folder("", "/ws/a").unwrap().unwrap();
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["permissions"]["trustDirectories"], json!(["/ws/a"]));
    }

    #[test]
    fn trust_write_keeps_other_keys_and_bytes_apart_from_the_new_entry() {
        let out = settings_with_trusted_folder(QODER_WRITTEN, "/ws/a")
            .unwrap()
            .unwrap();
        let expected = QODER_WRITTEN.replace(
            "      \"/home/u\"\n",
            "      \"/home/u\",\n      \"/ws/a\"\n",
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn trust_write_is_noop_when_already_trusted() {
        assert!(
            settings_with_trusted_folder(QODER_WRITTEN, "/home/u")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn trust_write_is_noop_below_a_trusted_folder() {
        assert!(
            settings_with_trusted_folder(QODER_WRITTEN, "/home/u/project/sub")
                .unwrap()
                .is_none()
        );
        // A sibling that only shares a name prefix is not covered.
        assert!(
            settings_with_trusted_folder(QODER_WRITTEN, "/home/user2")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn trust_write_rejects_malformed_settings_instead_of_clobbering() {
        assert!(settings_with_trusted_folder("{not json", "/ws").is_err());
        assert!(settings_with_trusted_folder("[]", "/ws").is_err());
        assert!(settings_with_trusted_folder(r#"{"permissions": []}"#, "/ws").is_err());
        assert!(
            settings_with_trusted_folder(r#"{"permissions":{"trustDirectories":"x"}}"#, "/ws")
                .is_err()
        );
    }

    #[test]
    #[serial_test::serial]
    fn ensure_trusted_edits_the_effective_settings_file() {
        use crate::hooks::test_helpers::EnvGuard;
        let _guard = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("qoder-config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("settings.json"), QODER_WRITTEN).unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        unsafe {
            std::env::set_var("QODER_CONFIG_DIR", &config);
            // The trust lock lives under HCOM_DIR.
            std::env::set_var("HCOM_DIR", dir.path().join(".hcom"));
        }

        ensure_qoder_workspace_trusted(&workspace, &[], &HashMap::new()).unwrap();
        let after = std::fs::read_to_string(config.join("settings.json")).unwrap();
        let value: Value = serde_json::from_str(&after).unwrap();
        let canonical = workspace.canonicalize().unwrap();
        assert_eq!(
            value["permissions"]["trustDirectories"][1],
            canonical.to_string_lossy().as_ref()
        );
        assert_eq!(value["model"]["name"], "m");

        // Second launch: already listed, file untouched.
        ensure_qoder_workspace_trusted(&workspace, &[], &HashMap::new()).unwrap();
        assert_eq!(
            std::fs::read_to_string(config.join("settings.json")).unwrap(),
            after
        );
    }
    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn workspace_follows_the_cwd_flag() {
        let launch = Path::new("/launch");
        assert_eq!(qoder_workspace(launch, &args(&["--model", "m"])), launch);
        assert_eq!(
            qoder_workspace(launch, &args(&["--cwd", "sub"])),
            Path::new("/launch/sub")
        );
        assert_eq!(
            qoder_workspace(launch, &args(&["-w=/abs/dir", "--model", "m"])),
            Path::new("/abs/dir")
        );
        // After `--` it is the prompt, not a flag.
        assert_eq!(
            qoder_workspace(launch, &args(&["--", "--cwd", "x"])),
            launch
        );
    }

    #[test]
    #[serial_test::serial]
    fn config_dir_prefers_flag_then_child_env_then_process_env() {
        use crate::hooks::test_helpers::EnvGuard;
        let _guard = EnvGuard::new();
        let launch = Path::new("/launch");
        unsafe { std::env::set_var("QODER_CONFIG_DIR", "/from-process") };
        let child = HashMap::from([("QODER_CONFIG_DIR".to_string(), "/from-child".to_string())]);
        assert_eq!(
            qoder_config_dir(launch, &args(&["--config-dir", "rel"]), &child),
            Path::new("/launch/rel")
        );
        assert_eq!(
            qoder_config_dir(launch, &[], &child),
            Path::new("/from-child")
        );
        assert_eq!(
            qoder_config_dir(launch, &[], &HashMap::new()),
            Path::new("/from-process")
        );
    }

    #[test]
    #[serial_test::serial]
    fn ensure_trusted_targets_the_cwd_flag_folder_and_child_config_dir() {
        use crate::hooks::test_helpers::EnvGuard;
        let _guard = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::remove_var("QODER_CONFIG_DIR");
            std::env::set_var("HCOM_DIR", dir.path().join(".hcom"));
        }
        let config = dir.path().join("cfg");
        let launch = dir.path().join("launch");
        let workspace = launch.join("sub");
        std::fs::create_dir_all(&workspace).unwrap();
        let child = HashMap::from([(
            "QODER_CONFIG_DIR".to_string(),
            config.to_string_lossy().to_string(),
        )]);

        ensure_qoder_workspace_trusted(&launch, &args(&["--cwd", "sub"]), &child).unwrap();
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(config.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            value["permissions"]["trustDirectories"],
            json!([workspace.canonicalize().unwrap().to_string_lossy()])
        );
    }
}
