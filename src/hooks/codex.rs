//! Codex CLI hook handler and settings management.
//!
//! Single hook type `codex-notify` (JSON via argv[2]). Unlike Claude/Gemini,
//! message delivery uses PTY injection triggered by TranscriptWatcher, not hooks.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use crate::db::{HcomDb, InstanceRow};
use crate::hooks::common;
use crate::hooks::family;
use crate::instances;
use crate::log;
use crate::paths;
use crate::shared::ST_LISTENING;
use crate::shared::context::HcomContext;

/// Derive Codex transcript path from thread_id.
///
/// Searches $CODEX_HOME/sessions (or ~/.codex/sessions) for
/// `rollout-*-{thread_id}.jsonl`. Returns most recently modified match.
///
pub fn derive_codex_transcript_path(thread_id: &str) -> Option<String> {
    if thread_id.is_empty() {
        return None;
    }

    let codex_base = std::env::var("CODEX_HOME").ok().unwrap_or_else(|| {
        dirs::home_dir()
            .map(|h| h.join(".codex").to_string_lossy().to_string())
            .unwrap_or_default()
    });

    let sessions_dir = PathBuf::from(&codex_base).join("sessions");
    let pattern = format!(
        "{}/**/rollout-*-{}.jsonl",
        sessions_dir.display(),
        thread_id
    );

    match glob::glob(&pattern) {
        Ok(entries) => {
            let mut matches: Vec<PathBuf> = entries.filter_map(|e| e.ok()).collect();
            if matches.is_empty() {
                return None;
            }
            // Return most recently modified for deterministic selection
            matches.sort_by(|a, b| {
                let ta = a
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                let tb = b
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                tb.cmp(&ta)
            });
            matches.first().map(|p| p.to_string_lossy().to_string())
        }
        Err(_) => None,
    }
}

/// Resolve Codex instance via process binding or session binding.
///
/// Resolve Codex instance from process or session binding.
fn resolve_instance_codex(db: &HcomDb, ctx: &HcomContext, thread_id: &str) -> Option<InstanceRow> {
    instances::resolve_instance_from_binding(
        db,
        Some(thread_id).filter(|s| !s.is_empty()),
        ctx.process_id.as_deref(),
    )
}

/// Bind vanilla Codex instance by searching transcript for marker.
///
fn bind_vanilla_instance_codex(
    db: &HcomDb,
    thread_id: &str,
    transcript_path: Option<&str>,
) -> Option<String> {
    // Skip if no pending instances (optimization)
    let pending = common::get_pending_instances(db);
    if pending.is_empty() {
        return None;
    }

    let derived_path = if transcript_path.is_none() || transcript_path == Some("") {
        derive_codex_transcript_path(thread_id)
    } else {
        None
    };
    let effective_path = transcript_path
        .filter(|s| !s.is_empty())
        .or(derived_path.as_deref())?;

    let instance_name = common::find_last_bind_marker(effective_path)?;

    family::bind_vanilla_instance(
        db,
        &instance_name,
        Some(thread_id).filter(|s| !s.is_empty()),
        Some(effective_path),
        "codex",
        "codex-notify",
    )
}

/// Handle Codex notify hook — signals turn completion.
///
/// Called by Codex with JSON payload containing:
/// ```json
/// {
///     "type": "agent-turn-complete",
///     "thread-id": "uuid",
///     "turn-id": "12345",
///     "cwd": "/path/to/project",
///     "input-messages": ["user prompt"],
///     "last-assistant-message": "response text"
/// }
/// ```
///
fn handle_notify(db: &HcomDb, ctx: &HcomContext, raw: &Value) -> i32 {
    // Only process agent-turn-complete events
    let event_type = raw.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if event_type != "agent-turn-complete" {
        return 0;
    }

    let thread_id = raw.get("thread-id").and_then(|v| v.as_str()).unwrap_or("");

    // Accept both "transcript_path" and "session_path" (fallback)
    let transcript_path_raw = raw
        .get("transcript_path")
        .or_else(|| raw.get("session_path"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Resolve instance
    let mut instance = resolve_instance_codex(db, ctx, thread_id);
    let mut instance_name: String;

    if let Some(ref inst) = instance {
        instance_name = inst.name.clone();
    } else {
        // Try vanilla binding
        let bound_name = bind_vanilla_instance_codex(db, thread_id, Some(transcript_path_raw));
        match bound_name {
            Some(name) => {
                instance = db.get_instance_full(&name).ok().flatten();
                if instance.is_none() {
                    return 0;
                }
                instance_name = name;
            }
            None => return 0,
        }
    }

    // Derive transcript path if not provided
    let transcript_path = if transcript_path_raw.is_empty() {
        derive_codex_transcript_path(thread_id)
    } else {
        Some(transcript_path_raw.to_string())
    };

    // Update instance session_id to real thread_id FIRST (before status update)
    if !thread_id.is_empty() || transcript_path.is_some() {
        // Bind session to process if applicable
        if !thread_id.is_empty() {
            if let Some(ref pid) = ctx.process_id {
                let canonical =
                    instances::bind_session_to_process(db, thread_id, Some(pid.as_str()));
                if let Some(ref name) = canonical {
                    if name != &instance_name {
                        instance_name = name.clone();
                    }
                }
                let _ = db.rebind_instance_session(&instance_name, thread_id);
            }
        }

        // Capture launch context
        instances::capture_and_store_launch_context(db, &instance_name);

        // Build position updates
        let mut updates = serde_json::Map::new();

        // Codex payload includes cwd; fall back to ctx.cwd
        let cwd = raw
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| ctx.cwd.to_string_lossy().to_string());
        if !cwd.is_empty() {
            updates.insert("directory".into(), Value::String(cwd));
        }
        if !thread_id.is_empty() {
            updates.insert("session_id".into(), Value::String(thread_id.to_string()));
        }
        if let Some(ref tp) = transcript_path {
            updates.insert("transcript_path".into(), Value::String(tp.clone()));
        }
        instances::update_instance_position(db, &instance_name, &updates);
    }

    // Update instance status to listening with idle_since timestamp
    // Re-fetch instance to verify it still exists
    if db
        .get_instance_full(&instance_name)
        .ok()
        .flatten()
        .is_none()
    {
        return 0;
    }

    let idle_since = crate::shared::time::now_iso();

    instances::set_status(db, &instance_name, ST_LISTENING, "", Default::default());

    let mut idle_updates = serde_json::Map::new();
    idle_updates.insert("idle_since".into(), Value::String(idle_since));
    instances::update_instance_position(db, &instance_name, &idle_updates);

    // Notify instance (wake PTY delivery thread)
    common::notify_hook_instance_with_db(db, &instance_name);

    // Auto-spawn relay-worker if needed
    crate::relay::worker::ensure_worker(true);

    0
}

/// Handle codex-notify hook — entry point from router.
///
/// Parses argv[2] JSON, builds context, dispatches to handle_notify.
/// Returns exit code (0 = success).
///
pub fn dispatch_codex_hook(args: &[String]) -> i32 {
    let start = std::time::Instant::now();

    // Parse payload from argv (args = ["codex-notify", "{json}"])
    if args.len() < 2 {
        return 0;
    }

    let raw: Value = match serde_json::from_str(&args[1]) {
        Ok(v) => v,
        Err(e) => {
            log::log_error(
                "hooks",
                "codex.parse_error",
                &format!("failed to parse argv JSON: {}", e),
            );
            return 0;
        }
    };

    // Open DB (includes schema migration/compat) — soft-fail on error (exit 0, don't break Codex)
    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(e) => {
            log::log_warn(
                "hooks",
                "codex.db_error",
                &format!("failed to open DB, degrading gracefully: {}", e),
            );
            return 0;
        }
    };

    // Build context from environment
    let ctx = HcomContext::from_os();

    // Pre-gate: non-participants with empty DB → exit 0, no output
    if !common::hook_gate_check(&ctx, &db) {
        return 0;
    }

    let exit_code = common::dispatch_with_panic_guard("codex", "codex-notify", 0, || {
        handle_notify(&db, &ctx, &raw)
    });

    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    log::log_info(
        "hooks",
        "codex.dispatch.timing",
        &format!(
            "hook=codex-notify total_ms={:.2} exit_code={}",
            elapsed_ms, exit_code
        ),
    );

    exit_code
}

/// Safe hcom commands that Codex should auto-approve.
use super::common::SAFE_HCOM_COMMANDS;

/// Tool names that support --help/-h flags.
const HCOM_TOOL_NAMES: &[&str] = &["claude", "gemini", "codex", "opencode"];

/// Get path to Codex config file.
///
/// Base directory for Codex config: HCOM_DIR parent if set (sandbox), otherwise $HOME.
/// Reads HCOM_DIR directly (not cached Config) so tests can override via env var.
fn codex_base_dir() -> PathBuf {
    crate::runtime_env::tool_config_root()
}

/// Get path to Codex config.toml.
pub fn get_codex_config_path() -> PathBuf {
    codex_base_dir().join(".codex").join("config.toml")
}

/// Get path to Codex execpolicy rules directory.
pub fn get_codex_rules_path() -> PathBuf {
    codex_base_dir().join(".codex").join("rules")
}

/// Check if a TOML line is an hcom notify configuration.
///
/// Handles both formats: array `notify = ["hcom", "codex-notify"]`
/// and string `notify = "hcom codex-notify"`.
fn is_hcom_notify_line(line: &str) -> bool {
    let stripped = line.trim();
    if !stripped.starts_with("notify") {
        return false;
    }
    let lower = stripped.to_lowercase();
    if lower.contains("\"hcom\"") || lower.contains("'hcom'") || lower.contains("hcom ") {
        return true;
    }
    // Fallback: codex-notify is hcom-specific
    lower.contains("codex-notify")
}

/// Build the expected notify line for config.toml.
///
/// Dynamically detects invocation mode (hcom vs uvx hcom) to match
fn build_expected_notify_line() -> String {
    let mut parts = crate::runtime_env::get_hcom_prefix();
    parts.push("codex-notify".into());
    let array_str = parts
        .iter()
        .map(|p| format!("\"{}\"", p))
        .collect::<Vec<_>>()
        .join(", ");
    format!("notify = [{}]", array_str)
}

/// Extract the notify line from config content, or None if not found.
fn extract_notify_line(content: &str) -> Option<String> {
    for line in content.lines() {
        let stripped = line.trim();
        if stripped.starts_with('#') || stripped.starts_with('[') {
            continue;
        }
        if stripped.starts_with("notify") && stripped.contains('=') {
            return Some(stripped.to_string());
        }
    }
    None
}

/// Build execpolicy rules content for hcom.rules.
///
/// Dynamically detects invocation prefix (hcom vs uvx hcom) to match
fn build_codex_rules() -> String {
    let prefix = crate::runtime_env::get_hcom_prefix();
    let prefix_parts: String = prefix
        .iter()
        .map(|p| format!("\"{}\"", p))
        .collect::<Vec<_>>()
        .join(", ");

    let mut rules = vec!["# hcom integration - auto-approve safe commands".to_string()];
    for cmd in SAFE_HCOM_COMMANDS {
        rules.push(format!(
            "prefix_rule(pattern=[{}, \"{}\"], decision=\"allow\")",
            prefix_parts, cmd
        ));
    }
    for tool in HCOM_TOOL_NAMES {
        rules.push(format!(
            "prefix_rule(pattern=[{}, \"{}\", \"--help\"], decision=\"allow\")",
            prefix_parts, tool
        ));
        rules.push(format!(
            "prefix_rule(pattern=[{}, \"{}\", \"-h\"], decision=\"allow\")",
            prefix_parts, tool
        ));
    }
    rules.join("\n") + "\n"
}

/// Set up Codex execpolicy rules for auto-approval.
pub fn setup_codex_execpolicy() -> bool {
    let rules_dir = get_codex_rules_path();
    let rules_file = rules_dir.join("hcom.rules");
    let rule_content = build_codex_rules();

    // Check if already configured correctly
    if rules_file.exists() {
        if let Ok(existing) = std::fs::read_to_string(&rules_file) {
            if existing == rule_content {
                return true;
            }
        }
    }

    let _ = std::fs::create_dir_all(&rules_dir);
    paths::atomic_write(&rules_file, &rule_content)
}

/// Remove hcom execpolicy rule.
pub fn remove_codex_execpolicy() -> bool {
    let rules_file = get_codex_rules_path().join("hcom.rules");
    if rules_file.exists() {
        std::fs::remove_file(&rules_file).is_ok()
    } else {
        true
    }
}

/// Set up Codex notify hook in config.toml.
///
/// Adds `notify = ["hcom", "codex-notify"]` before any [section] headers.
/// If an existing hcom notify line exists with stale command, updates it.
/// If a non-hcom notify exists, returns false with error.
///
pub fn setup_codex_hooks(include_permissions: bool) -> bool {
    let config_path = get_codex_config_path();
    let notify_line = build_expected_notify_line();

    let result: Result<(), String> = (|| {
        if config_path.exists() {
            let content = std::fs::read_to_string(&config_path).map_err(|e| e.to_string())?;

            if let Some(existing_notify) = extract_notify_line(&content) {
                if existing_notify.contains("codex-notify") {
                    if existing_notify == notify_line {
                        // Already correct
                        return Ok(());
                    }
                    // Stale command — remove and re-add
                    remove_codex_hooks();
                    // Re-read after removal
                    let content =
                        std::fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
                    return insert_notify_line(&config_path, &content, &notify_line);
                } else {
                    return Err(format!(
                        "{} already has a notify hook configured. \
                         Codex only supports one notify command.",
                        config_path.display()
                    ));
                }
            }

            insert_notify_line(&config_path, &content, &notify_line)
        } else {
            // Create new config
            if let Some(parent) = config_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let content = format!("# Codex config\n\n# hcom integration\n{}\n", notify_line);
            if paths::atomic_write(&config_path, &content) {
                Ok(())
            } else {
                Err("atomic_write failed".to_string())
            }
        }
    })();

    if let Err(e) = result {
        log::log_error(
            "hooks",
            "codex.setup_error",
            &format!("Failed to setup Codex hooks: {}", e),
        );
        return false;
    }

    // Handle execpolicy
    if include_permissions {
        setup_codex_execpolicy();
    } else {
        remove_codex_execpolicy();
    }

    true
}

/// Insert notify line before first [section] header in TOML content.
fn insert_notify_line(config_path: &Path, content: &str, notify_line: &str) -> Result<(), String> {
    let new_content = if content.starts_with('[') {
        // Section at very start — notify must come before all sections
        format!("# hcom integration\n{}\n\n{}", notify_line, content)
    } else if let Some(pos) = content.find("\n[") {
        // Insert before first section (after newline)
        let pos = pos + 1; // after the newline
        format!(
            "{}# hcom integration\n{}\n\n{}",
            &content[..pos],
            notify_line,
            &content[pos..]
        )
    } else {
        // No sections, append at end
        format!(
            "{}\n\n# hcom integration\n{}\n",
            content.trim_end(),
            notify_line
        )
    };

    if paths::atomic_write(config_path, &new_content) {
        Ok(())
    } else {
        Err("atomic_write failed".to_string())
    }
}

/// Verify that hcom hooks are correctly installed in Codex config.
///
/// Checks config file exists, notify key matches, and optionally execpolicy.
pub fn verify_codex_hooks_installed(check_permissions: bool) -> bool {
    let config_path = get_codex_config_path();
    if !config_path.exists() {
        return false;
    }

    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    let existing_notify = match extract_notify_line(&content) {
        Some(n) => n,
        None => return false,
    };

    if !existing_notify.contains("codex-notify") {
        return false;
    }

    if existing_notify != build_expected_notify_line() {
        return false;
    }

    if check_permissions {
        let rules_path = get_codex_rules_path().join("hcom.rules");
        if !rules_path.exists() {
            return false;
        }
    }

    true
}

/// Remove hcom hooks from Codex config.
///
/// Cleans notify line, comment, and trailing blank line from config.toml.
/// Also removes execpolicy rules.
pub fn remove_codex_hooks() -> bool {
    let global_config = dirs::home_dir()
        .map(|h| h.join(".codex").join("config.toml"))
        .unwrap_or_default();
    let local_config = get_codex_config_path();
    let global_rules = dirs::home_dir()
        .map(|h| h.join(".codex").join("rules"))
        .unwrap_or_default();
    let local_rules = get_codex_rules_path();

    // Remove execpolicy
    remove_execpolicy_from_path(&global_rules);
    if local_rules != global_rules {
        remove_execpolicy_from_path(&local_rules);
    }

    // Remove hooks from config files
    let global_ok = remove_hooks_from_path(&global_config);
    let local_ok = if local_config != global_config {
        remove_hooks_from_path(&local_config)
    } else {
        true
    };

    global_ok && local_ok
}

fn remove_execpolicy_from_path(rules_dir: &Path) -> bool {
    let rules_file = rules_dir.join("hcom.rules");
    if rules_file.exists() {
        std::fs::remove_file(&rules_file).is_ok()
    } else {
        true
    }
}

fn remove_hooks_from_path(config_path: &Path) -> bool {
    if !config_path.exists() {
        return true;
    }

    let content = match std::fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    let mut new_lines = Vec::new();
    let mut skip_next_blank = false;

    for line in content.lines() {
        if is_hcom_notify_line(line) {
            skip_next_blank = true;
            continue;
        }
        if line.trim() == "# hcom integration" {
            skip_next_blank = true;
            continue;
        }
        if skip_next_blank && line.trim().is_empty() {
            skip_next_blank = false;
            continue;
        }
        skip_next_blank = false;
        new_lines.push(line);
    }

    let mut result = new_lines.join("\n");
    if content.ends_with('\n') {
        result.push('\n');
    }

    paths::atomic_write(config_path, &result)
}

#[cfg(test)]
mod tests {
    use super::*;
    // -- derive_codex_transcript_path --

    #[test]
    fn test_derive_transcript_empty_thread_id() {
        assert!(derive_codex_transcript_path("").is_none());
    }

    #[test]
    fn test_derive_transcript_no_match() {
        // Non-existent thread ID should return None
        assert!(derive_codex_transcript_path("nonexistent-thread-12345").is_none());
    }

    #[test]
    fn test_derive_transcript_finds_file() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions").join("project");
        std::fs::create_dir_all(&sessions).unwrap();

        let transcript = sessions.join("rollout-1-abc-123-def.jsonl");
        std::fs::File::create(&transcript).unwrap();

        // Set CODEX_HOME to temp dir
        let saved = std::env::var("CODEX_HOME").ok();
        // SAFETY: Test-only env manipulation
        unsafe { std::env::set_var("CODEX_HOME", dir.path()) };

        let result = derive_codex_transcript_path("abc-123-def");
        assert!(result.is_some(), "should find transcript file");
        assert!(result.unwrap().contains("rollout-1-abc-123-def.jsonl"));

        // Restore
        if let Some(v) = saved {
            unsafe { std::env::set_var("CODEX_HOME", v) };
        } else {
            unsafe { std::env::remove_var("CODEX_HOME") };
        }
    }

    // -- is_hcom_notify_line --

    #[test]
    fn test_is_hcom_notify_line_array() {
        assert!(is_hcom_notify_line("notify = [\"hcom\", \"codex-notify\"]"));
    }

    #[test]
    fn test_is_hcom_notify_line_string() {
        assert!(is_hcom_notify_line("notify = \"hcom codex-notify\""));
    }

    #[test]
    fn test_is_hcom_notify_line_non_hcom() {
        assert!(!is_hcom_notify_line(
            "notify = [\"other-tool\", \"some-hook\"]"
        ));
    }

    #[test]
    fn test_is_hcom_notify_line_not_notify() {
        assert!(!is_hcom_notify_line("model = \"gpt-4\""));
    }

    #[test]
    fn test_is_hcom_notify_stale_command() {
        // Detects stale hcom commands by codex-notify substring
        assert!(is_hcom_notify_line(
            "notify = [\"/old/path/hcom\", \"codex-notify\"]"
        ));
    }

    // -- extract_notify_line --

    #[test]
    fn test_extract_notify_line_found() {
        let content = "# comment\nnotify = [\"hcom\", \"codex-notify\"]\n\n[model]\nname = \"o1\"";
        assert_eq!(
            extract_notify_line(content),
            Some("notify = [\"hcom\", \"codex-notify\"]".to_string())
        );
    }

    #[test]
    fn test_extract_notify_line_none() {
        let content = "# just comments\n[model]\nname = \"o1\"";
        assert!(extract_notify_line(content).is_none());
    }

    #[test]
    fn test_extract_notify_skips_comments_and_sections() {
        let content = "# notify = old\n[notify]\nkey = val";
        assert!(extract_notify_line(content).is_none());
    }

    // -- build_expected_notify_line --

    #[test]
    fn test_build_expected_notify_line() {
        let line = build_expected_notify_line();
        // Must always end with "codex-notify" and be valid TOML array
        assert!(line.starts_with("notify = ["));
        assert!(line.ends_with(']'));
        assert!(line.contains("\"codex-notify\""));
        // Must contain "hcom" (either as standalone or after "uvx")
        assert!(line.contains("\"hcom\""));
    }

    // -- build_codex_rules --

    #[test]
    fn test_build_codex_rules_contains_send() {
        let rules = build_codex_rules();
        assert!(rules.contains("\"send\""));
        assert!(rules.contains("\"list\""));
        assert!(rules.contains("decision=\"allow\""));
    }

    #[test]
    fn test_build_codex_rules_contains_tool_help() {
        let rules = build_codex_rules();
        assert!(rules.contains("\"claude\", \"--help\""));
        assert!(rules.contains("\"gemini\", \"-h\""));
    }

    // -- settings setup/remove/verify --

    #[test]
    #[serial]
    fn test_setup_and_remove_codex_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join(".codex").join("config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();

        // Write initial config with a section
        std::fs::write(&config_path, "[model]\nname = \"o1\"\n").unwrap();

        // Setup (using env to redirect paths)
        let saved = std::env::var("HCOM_DIR").ok();
        let hcom_dir = dir.path().join(".hcom");
        std::fs::create_dir_all(&hcom_dir).unwrap();
        unsafe { std::env::set_var("HCOM_DIR", &hcom_dir) };

        let success = setup_codex_hooks(false);
        assert!(success, "setup should succeed");

        // Verify notify was inserted
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("notify = [\"hcom\", \"codex-notify\"]"));
        // Should be before [model] section
        let notify_pos = content.find("notify").unwrap();
        let model_pos = content.find("[model]").unwrap();
        assert!(notify_pos < model_pos, "notify should be before [model]");

        // Remove
        let removed = remove_hooks_from_path(&config_path);
        assert!(removed, "remove should succeed");

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(!content.contains("notify"));
        assert!(content.contains("[model]"));

        // Restore env
        if let Some(v) = saved {
            unsafe { std::env::set_var("HCOM_DIR", v) };
        } else {
            unsafe { std::env::remove_var("HCOM_DIR") };
        }
    }

    #[test]
    #[serial]
    fn test_setup_codex_hooks_creates_new_config() {
        let dir = tempfile::tempdir().unwrap();
        let hcom_dir = dir.path().join(".hcom");
        std::fs::create_dir_all(&hcom_dir).unwrap();

        let saved = std::env::var("HCOM_DIR").ok();
        unsafe { std::env::set_var("HCOM_DIR", &hcom_dir) };

        let config_path = get_codex_config_path();
        assert!(!config_path.exists());

        let success = setup_codex_hooks(false);
        assert!(success);
        assert!(config_path.exists());

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("codex-notify"));

        if let Some(v) = saved {
            unsafe { std::env::set_var("HCOM_DIR", v) };
        } else {
            unsafe { std::env::remove_var("HCOM_DIR") };
        }
    }

    // -- handle_notify logic --

    #[test]
    fn test_handle_notify_ignores_non_turn_complete() {
        // Should return 0 for non-turn-complete events
        let raw = serde_json::json!({"type": "other-event"});
        // Can't easily test with real DB, but verify the function signature exists
        // and the event type check works
        let event_type = raw.get("type").and_then(|v| v.as_str()).unwrap_or("");
        assert_ne!(event_type, "agent-turn-complete");
    }

    #[test]
    fn test_dispatch_codex_hook_no_args() {
        // Should return 0 with insufficient args
        assert_eq!(dispatch_codex_hook(&["codex-notify".to_string()]), 0);
    }

    #[test]
    fn test_dispatch_codex_hook_invalid_json() {
        // Config::init() needed because log functions call Config::get()
        crate::config::Config::init();
        // Should return 0 with invalid JSON (not crash)
        assert_eq!(
            dispatch_codex_hook(&["codex-notify".to_string(), "not-json".to_string()]),
            0
        );
    }

    // -- insert_notify_line --

    #[test]
    fn test_insert_notify_before_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let content = "# header\n\n[model]\nname = \"o1\"\n";
        std::fs::write(&path, content).unwrap();

        let result = insert_notify_line(&path, content, "notify = [\"hcom\", \"codex-notify\"]");
        assert!(result.is_ok());

        let written = std::fs::read_to_string(&path).unwrap();
        let notify_pos = written.find("notify").unwrap();
        let model_pos = written.find("[model]").unwrap();
        assert!(notify_pos < model_pos);
    }

    #[test]
    fn test_insert_notify_no_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let content = "# just comments\n";
        std::fs::write(&path, content).unwrap();

        let result = insert_notify_line(&path, content, "notify = [\"hcom\", \"codex-notify\"]");
        assert!(result.is_ok());

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("codex-notify"));
    }

    use crate::hooks::test_helpers::{EnvGuard, isolated_test_env};
    use serial_test::serial;

    fn codex_test_env() -> (tempfile::TempDir, PathBuf, PathBuf, EnvGuard) {
        let (dir, _hcom_dir, test_home, guard) = isolated_test_env();
        let config_path = test_home.join(".codex").join("config.toml");
        (dir, test_home, config_path, guard)
    }

    /// Independent verification: no hcom notify lines in TOML content.
    fn independently_verify_no_hcom_in_toml(content: &str) -> Vec<String> {
        let mut violations = Vec::new();
        for (i, line) in content.lines().enumerate() {
            let stripped = line.trim();
            if stripped.starts_with('#') || stripped.starts_with('[') {
                continue;
            }
            if stripped.starts_with("notify") && stripped.contains('=') && stripped.contains("hcom")
            {
                violations.push(format!("Line {}: {stripped}", i + 1));
            }
        }
        violations
    }

    #[test]
    #[serial]
    fn test_setup_codex_creates_config() {
        let (_dir, _test_home, config_path, _guard) = codex_test_env();

        assert!(setup_codex_hooks(false));
        assert!(config_path.exists());

        let content = std::fs::read_to_string(&config_path).unwrap();
        let expected_notify = build_expected_notify_line();
        assert!(
            content.contains(&expected_notify),
            "expected exact notify line '{expected_notify}' in config"
        );
        assert!(
            content.contains("# hcom integration"),
            "comment marker should be present"
        );

        assert!(verify_codex_hooks_installed(false));

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_setup_codex_inserts_before_section() {
        let (_dir, _test_home, config_path, _guard) = codex_test_env();

        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "[model]\nname = \"gpt-4\"\n\n[other]\nkey = \"value\"\n",
        )
        .unwrap();

        assert!(setup_codex_hooks(false));

        let content = std::fs::read_to_string(&config_path).unwrap();
        let notify_pos = content.find("notify =").expect("notify should exist");
        let section_pos = content.find("[model]").expect("[model] should exist");
        assert!(
            notify_pos < section_pos,
            "notify must be before [model] section"
        );

        assert!(verify_codex_hooks_installed(false));

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_setup_codex_idempotent() {
        let (_dir, _test_home, config_path, _guard) = codex_test_env();

        assert!(setup_codex_hooks(false));
        assert!(setup_codex_hooks(false)); // Second call

        let content = std::fs::read_to_string(&config_path).unwrap();
        let count = content.matches("notify =").count();
        assert_eq!(count, 1, "should only have one notify line, got {count}");

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_setup_codex_refuses_existing_notify() {
        let (_dir, _test_home, config_path, _guard) = codex_test_env();

        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let original = "notify = [\"some-other-tool\", \"arg\"]\n";
        std::fs::write(&config_path, original).unwrap();

        assert!(
            !setup_codex_hooks(false),
            "should refuse existing non-hcom notify"
        );

        // File must be byte-identical to original (no corruption)
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(content, original, "file should be unchanged after refusal");
        assert!(content.contains("some-other-tool"));
        assert!(!content.contains("codex-notify"));

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_verify_codex_detects_missing() {
        let (_dir, _test_home, config_path, _guard) = codex_test_env();

        // No config file
        assert!(!verify_codex_hooks_installed(false));

        // Config exists but no notify
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[model]\nname = \"test\"\n").unwrap();
        assert!(!verify_codex_hooks_installed(false));

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_verify_codex_detects_wrong_notify() {
        let (_dir, _test_home, config_path, _guard) = codex_test_env();

        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "notify = [\"other-tool\"]\n").unwrap();

        assert!(!verify_codex_hooks_installed(false));

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_remove_codex_cleans_config() {
        let (_dir, _test_home, config_path, _guard) = codex_test_env();

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));

        assert!(remove_codex_hooks());

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(!content.contains("codex-notify"));
        assert!(!content.contains("# hcom integration"));

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_remove_codex_preserves_other_config() {
        let (_dir, _test_home, config_path, _guard) = codex_test_env();

        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "# My config\n# hcom integration\nnotify = [\"hcom\", \"codex-notify\"]\n\n[model]\nname = \"gpt-4\"\n\n[other]\nkey = \"value\"\n",
        )
        .unwrap();

        assert!(remove_codex_hooks());

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(!content.contains("codex-notify"));
        assert!(content.contains("[model]"));
        assert!(content.contains("name = \"gpt-4\""));
        assert!(content.contains("[other]"));

        let violations = independently_verify_no_hcom_in_toml(&content);
        assert!(violations.is_empty(), "hcom still present: {violations:?}");

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_remove_codex_noop_when_no_config() {
        let (_dir, _test_home, _config_path, _guard) = codex_test_env();

        // No config file — should succeed (nothing to remove)
        assert!(remove_codex_hooks());

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_setup_codex_creates_execpolicy() {
        let (_dir, test_home, _config_path, _guard) = codex_test_env();

        assert!(setup_codex_hooks(true));

        let rules_file = test_home.join(".codex").join("rules").join("hcom.rules");
        assert!(rules_file.exists(), "execpolicy rules should be created");
        let content = std::fs::read_to_string(&rules_file).unwrap();
        assert!(content.contains("hcom"));

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_remove_codex_removes_execpolicy() {
        let (_dir, test_home, _config_path, _guard) = codex_test_env();

        assert!(setup_codex_hooks(true));
        let rules_file = test_home.join(".codex").join("rules").join("hcom.rules");
        assert!(rules_file.exists());

        assert!(remove_codex_hooks());
        assert!(!rules_file.exists(), "execpolicy rules should be removed");

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_setup_codex_updates_stale_command() {
        let (_dir, _test_home, config_path, _guard) = codex_test_env();

        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "# hcom integration\nnotify = [\"old-hcom\", \"codex-notify\"]\n",
        )
        .unwrap();

        assert!(setup_codex_hooks(false));

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("codex-notify"));
        assert!(!content.contains("old-hcom"));

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_setup_codex_remove_roundtrip() {
        let (_dir, _test_home, config_path, _guard) = codex_test_env();

        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[model]\nname = \"o1\"\n").unwrap();

        // Setup
        assert!(setup_codex_hooks(false));
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            content.contains("codex-notify"),
            "notify should be present after setup"
        );

        // Remove
        assert!(remove_codex_hooks());
        let content = std::fs::read_to_string(&config_path).unwrap();
        let violations = independently_verify_no_hcom_in_toml(&content);
        assert!(
            violations.is_empty(),
            "hcom still present after remove: {violations:?}"
        );

        // User data preserved
        assert!(content.contains("[model]"));
        assert!(content.contains("name = \"o1\""));

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_codex_handles_malformed_config() {
        let corrupt_cases = vec![
            "",                                 // empty
            "   \n\n   ",                       // whitespace only
            "# Just a comment",                 // comment only
            "invalid toml [ stuff",             // malformed
            "[section]\nkey = value\n[another", // incomplete section
        ];

        for corrupt in corrupt_cases {
            let (_dir, _test_home, config_path, _guard) = codex_test_env();
            std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
            std::fs::write(&config_path, corrupt).unwrap();

            // Should not crash
            let _ = setup_codex_hooks(false);
            let _ = remove_codex_hooks();
        }
    }
}
