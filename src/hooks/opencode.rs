//! OpenCode hook handlers — argv-based lifecycle management (start, status, read, stop).

use std::time::Instant;

use serde_json::Value;

use crate::bootstrap;
use crate::db::HcomDb;
use crate::instances;
use crate::log::{log_error, log_info};
use crate::messages;
use crate::shared::ST_LISTENING;
use crate::shared::constants::MAX_MESSAGES_PER_DELIVERY;
use crate::shared::context::HcomContext;

use super::common;
use super::common::finalize_session;

/// Extract `--flag value` from argv. Returns None if not found.
fn parse_flag(argv: &[String], flag: &str) -> Option<String> {
    argv.iter()
        .position(|a| a == flag)
        .and_then(|i| argv.get(i + 1))
        .cloned()
}

/// Check if a bare flag exists in argv (no value).
fn has_flag(argv: &[String], flag: &str) -> bool {
    argv.iter().any(|a| a == flag)
}

/// Upsert plugin notify endpoint in DB.
fn upsert_plugin_notify_endpoint(db: &HcomDb, instance_name: &str, port: u16) {
    if let Err(e) = db.upsert_notify_endpoint(instance_name, "plugin", port) {
        log_error(
            "native",
            "opencode.register_notify_fail",
            &format!(
                "Failed to register plugin notify port for {}: {}",
                instance_name, e
            ),
        );
    }
}

/// Send TCP wake to ALL of an instance's notify endpoints.
///
/// Used by status handler when instance becomes listening.
/// Queries all kinds (pty, hook, plugin) and sends a brief TCP connect to each.
fn notify_all_endpoints(db: &HcomDb, instance_name: &str) {
    instances::notify_instance_endpoints(db, instance_name, &[]);
}

/// Get path to OpenCode's SQLite database.
///
/// OpenCode uses XDG_DATA_HOME/opencode/opencode.db.
/// Default XDG_DATA_HOME is ~/.local/share on Linux/macOS.
fn get_opencode_db_path() -> Option<String> {
    let xdg_data = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{}/.local/share", home)
    });
    let db_path = std::path::PathBuf::from(&xdg_data)
        .join("opencode")
        .join("opencode.db");
    if db_path.exists() {
        Some(db_path.to_string_lossy().to_string())
    } else {
        None
    }
}

/// Handle opencode-start: bind session to process, set listening status.
///
/// Called by OpenCode plugin on session.created event.
/// Expects: hcom opencode-start --session-id <id> [--notify-port <port>]
///
/// Returns JSON: {"name": "<instance>", "session_id": "<id>", "bootstrap": "..."}
fn handle_start(ctx: &HcomContext, db: &HcomDb, argv: &[String]) -> (i32, String) {
    let session_id = match parse_flag(argv, "--session-id") {
        Some(sid) => sid,
        None => return (0, r#"{"error":"Missing --session-id"}"#.to_string()),
    };

    let notify_port: Option<u16> = parse_flag(argv, "--notify-port").and_then(|s| s.parse().ok());

    let process_id = match &ctx.process_id {
        Some(pid) => pid.clone(),
        None => return (0, r#"{"error":"HCOM_PROCESS_ID not set"}"#.to_string()),
    };

    // Re-binding detection: session already bound (compaction or reconnect)
    if let Ok(Some(existing_name)) = db.get_session_binding(&session_id) {
        let mut rebind_updates = serde_json::Map::new();
        rebind_updates.insert("name_announced".into(), serde_json::json!(false));
        rebind_updates.insert("session_id".into(), serde_json::json!(&session_id));

        if let Some(db_path) = get_opencode_db_path() {
            rebind_updates.insert("transcript_path".into(), serde_json::json!(db_path));
        }

        instances::update_instance_position(db, &existing_name, &rebind_updates);
        instances::set_status(
            db,
            &existing_name,
            ST_LISTENING,
            "start",
            Default::default(),
        );

        let hcom_config = crate::config::HcomConfig::load(None).unwrap_or_default();
        let bootstrap_text = bootstrap::get_bootstrap(
            db,
            &ctx.hcom_dir,
            &existing_name,
            "opencode",
            ctx.is_background,
            ctx.is_launched,
            &ctx.notes,
            &hcom_config.tag,
            crate::relay::is_relay_enabled(&hcom_config),
            ctx.background_name.as_deref(),
        );

        if let Some(port) = notify_port {
            upsert_plugin_notify_endpoint(db, &existing_name, port);
        }

        log_info(
            "hooks",
            "opencode-start.rebind",
            &format!("instance={} session_id={}", existing_name, session_id),
        );

        let mut result = serde_json::json!({
            "name": existing_name,
            "session_id": session_id,
        });
        result["bootstrap"] = Value::String(bootstrap_text);
        return (0, serde_json::to_string(&result).unwrap_or_default());
    }

    // Normal binding path
    let instance_name = match instances::bind_session_to_process(db, &session_id, Some(&process_id))
    {
        Some(name) => name,
        None => {
            return (
                0,
                r#"{"error":"No instance bound to this process"}"#.to_string(),
            );
        }
    };

    // Rebind session and initialize
    if let Err(e) = db.rebind_instance_session(&instance_name, &session_id) {
        log_error(
            "hooks",
            "hook.error",
            &format!("hook=opencode-start op=rebind_session err={}", e),
        );
    }

    // Initialize last_event_id BEFORE set_status() — set_status triggers
    // notify_instance() which TCP-wakes the plugin's deliverPendingToIdle().
    // If last_event_id is still 0, ALL historical events get delivered.
    if let Ok(Some(existing)) = db.get_instance_full(&instance_name) {
        if existing.last_event_id == 0 {
            let launch_event_id: Option<i64> = std::env::var("HCOM_LAUNCH_EVENT_ID")
                .ok()
                .and_then(|s| s.parse().ok());
            let current_max = db.get_last_event_id();
            let new_id = match launch_event_id {
                Some(lei) if lei <= current_max => lei,
                _ => current_max,
            };
            let mut id_updates = serde_json::Map::new();
            id_updates.insert("last_event_id".into(), serde_json::json!(new_id));
            instances::update_instance_position(db, &instance_name, &id_updates);
        }
    }

    instances::set_status(
        db,
        &instance_name,
        ST_LISTENING,
        "start",
        Default::default(),
    );

    // Capture launch context (preserves pane_id/terminal_preset from Rust PTY)
    instances::capture_and_store_launch_context(db, &instance_name);

    // Update instance position
    let mut updates = serde_json::Map::new();
    updates.insert("session_id".into(), serde_json::json!(&session_id));
    if let Some(db_path) = get_opencode_db_path() {
        updates.insert("transcript_path".into(), serde_json::json!(db_path));
    }
    if !ctx.cwd.as_os_str().is_empty() {
        updates.insert(
            "directory".into(),
            serde_json::json!(ctx.cwd.to_string_lossy()),
        );
    }
    instances::update_instance_position(db, &instance_name, &updates);

    // Register TCP notify endpoint
    if let Some(port) = notify_port {
        upsert_plugin_notify_endpoint(db, &instance_name, port);
    }

    // Build bootstrap text
    let tag = db
        .get_instance_full(&instance_name)
        .ok()
        .flatten()
        .and_then(|d| d.tag.clone())
        .unwrap_or_default();

    let hcom_config = crate::config::HcomConfig::load(None).unwrap_or_default();
    let relay_enabled = crate::relay::is_relay_enabled(&hcom_config);
    // Use config tag as fallback when instance has no tag
    let effective_tag = if tag.is_empty() {
        &hcom_config.tag
    } else {
        &tag
    };
    let bootstrap_text = bootstrap::get_bootstrap(
        db,
        &ctx.hcom_dir,
        &instance_name,
        "opencode",
        ctx.is_background,
        ctx.is_launched,
        &ctx.notes,
        effective_tag,
        relay_enabled,
        ctx.background_name.as_deref(),
    );

    // Auto-spawn relay-worker now that an instance is active
    crate::relay::worker::ensure_worker(true);

    let mut response = serde_json::json!({
        "name": instance_name,
        "session_id": session_id,
    });
    response["bootstrap"] = Value::String(bootstrap_text);
    (0, serde_json::to_string(&response).unwrap_or_default())
}

/// Handle opencode-status: update instance status.
///
/// Called by OpenCode plugin on session.status and session.idle events.
/// Expects: hcom opencode-status --name <name> --status <status> [--context <ctx>] [--detail <d>]
fn handle_status(db: &HcomDb, argv: &[String]) -> (i32, String) {
    let name = match parse_flag(argv, "--name") {
        Some(n) => n,
        None => return (0, r#"{"error":"Missing --name or --status"}"#.to_string()),
    };
    let status = match parse_flag(argv, "--status") {
        Some(s) => s,
        None => return (0, r#"{"error":"Missing --name or --status"}"#.to_string()),
    };

    let context = parse_flag(argv, "--context").unwrap_or_default();
    let detail = parse_flag(argv, "--detail").unwrap_or_default();

    instances::set_status(
        db,
        &name,
        &status,
        &context,
        instances::StatusUpdate {
            detail: &detail,
            ..Default::default()
        },
    );

    // Wake delivery thread if instance is now listening
    if status == ST_LISTENING {
        notify_all_endpoints(db, &name);
    }

    (0, r#"{"ok":true}"#.to_string())
}

/// Handle opencode-read: fetch pending messages, check, format, or ack.
///
/// Modes:
/// - Default: Return pending messages as JSON array (does NOT advance cursor)
/// - --format: Return formatted text (same format as Claude/Gemini delivery)
/// - --check: Return "true" or "false" string
/// - --ack --up-to <id>: Advance cursor to explicit event_id
/// - --ack (no --up-to): Advance cursor to max pending event_id (legacy)
fn handle_read(db: &HcomDb, argv: &[String]) -> (i32, String) {
    let name = match parse_flag(argv, "--name") {
        Some(n) => n,
        None => return (0, r#"{"error":"Missing --name"}"#.to_string()),
    };

    let format_mode = has_flag(argv, "--format");
    let check_mode = has_flag(argv, "--check");
    let ack_mode = has_flag(argv, "--ack");

    // Fetch unread messages (without advancing cursor)
    let raw_messages = db.get_unread_messages(&name);

    // Convert db::Message to serde_json::Value
    let messages: Vec<Value> = raw_messages.iter().map(common::message_to_value).collect();

    if format_mode {
        if messages.is_empty() {
            return (0, String::new());
        }
        let deliver = if messages.len() > MAX_MESSAGES_PER_DELIVERY {
            &messages[..MAX_MESSAGES_PER_DELIVERY]
        } else {
            &messages
        };
        let get_instance_data = common::make_instance_lookup(db);
        let hints = common::load_config_hints();
        let get_config_hints = || hints.clone();
        let tip_checker = common::make_tip_checker(db);
        let formatted = messages::format_messages_json(
            deliver,
            &name,
            &get_instance_data,
            &get_config_hints,
            Some(&tip_checker),
        );
        return (0, formatted);
    }

    if ack_mode {
        let up_to = parse_flag(argv, "--up-to");
        if let Some(up_to_str) = up_to {
            // Explicit ack position
            let ack_id: i64 = match up_to_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    return (
                        0,
                        serde_json::json!({"error": format!("Invalid --up-to: {}", up_to_str)})
                            .to_string(),
                    );
                }
            };
            let mut updates = serde_json::Map::new();
            updates.insert("last_event_id".into(), serde_json::json!(ack_id));
            instances::update_instance_position(db, &name, &updates);
            return (0, serde_json::json!({"acked_to": ack_id}).to_string());
        }
        // Legacy: ack all pending
        if messages.is_empty() {
            return (0, r#"{"acked":0}"#.to_string());
        }
        let last_id = messages
            .iter()
            .filter_map(|m| m.get("event_id").and_then(|v| v.as_i64()))
            .max()
            .unwrap_or(0);
        // Fallback: when all event_ids are 0, use db max
        let ack_id = if last_id > 0 {
            last_id
        } else {
            db.get_last_event_id()
        };
        if ack_id > 0 {
            let mut updates = serde_json::Map::new();
            updates.insert("last_event_id".into(), serde_json::json!(ack_id));
            instances::update_instance_position(db, &name, &updates);
        }
        return (0, serde_json::json!({"acked": messages.len()}).to_string());
    }

    if check_mode {
        return (
            0,
            if messages.is_empty() { "false" } else { "true" }.to_string(),
        );
    }

    // Default: return raw JSON array
    (
        0,
        serde_json::to_string(&messages).unwrap_or_else(|_| "[]".to_string()),
    )
}

/// Handle opencode-stop: finalize session and clean up instance.
///
/// Called by OpenCode plugin on session.deleted event.
/// Expects: hcom opencode-stop --name <name> [--reason <reason>]
fn handle_stop(db: &HcomDb, argv: &[String]) -> (i32, String) {
    let name = match parse_flag(argv, "--name") {
        Some(n) => n,
        None => return (0, r#"{"error":"Missing --name"}"#.to_string()),
    };
    let reason = parse_flag(argv, "--reason").unwrap_or_else(|| "unknown".to_string());

    finalize_session(db, &name, &reason, None);

    (0, r#"{"ok":true}"#.to_string())
}

/// Dispatch an OpenCode hook by name.
///
/// Returns (exit_code, stdout_output).
/// All OpenCode hooks return exit 0 (no blocking behavior).
pub fn dispatch_opencode_hook(hook_name: &str, argv: &[String]) -> (i32, String) {
    let start = Instant::now();

    // Build context
    let ctx = HcomContext::from_os();

    // Ensure hcom directories exist before opening DB.
    // On clean HOME/HCOM_DIR the DB parent dir won't exist yet.
    crate::paths::ensure_hcom_directories_at(&ctx.hcom_dir);

    // Open DB (includes schema migration/compat)
    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(e) => {
            log_error(
                "hooks",
                "hook.error",
                &format!("hook={} op=db_open err={}", hook_name, e),
            );
            return (
                0,
                serde_json::json!({"error": format!("DB open failed: {}", e)}).to_string(),
            );
        }
    };

    // Pre-gate: non-participants with empty DB → exit 0, no output
    if !common::hook_gate_check(&ctx, &db) {
        return (0, String::new());
    }

    // Strip hook name from argv to get handler args
    // argv comes as: ["opencode-start", "--session-id", "abc", ...]
    let handler_argv: Vec<String> = if !argv.is_empty() && argv[0] == hook_name {
        argv[1..].to_vec()
    } else {
        argv.to_vec()
    };

    let handler_start = Instant::now();
    let hook_name_owned = hook_name.to_string();

    let (exit_code, output) = common::dispatch_with_panic_guard(
        "opencode",
        &hook_name_owned,
        (
            0,
            serde_json::json!({"error": "internal panic"}).to_string(),
        ),
        || match hook_name_owned.as_str() {
            "opencode-start" => handle_start(&ctx, &db, &handler_argv),
            "opencode-status" => handle_status(&db, &handler_argv),
            "opencode-read" => handle_read(&db, &handler_argv),
            "opencode-stop" => handle_stop(&db, &handler_argv),
            _ => (
                0,
                serde_json::json!({"error": format!("Unknown OpenCode hook: {}", hook_name_owned)})
                    .to_string(),
            ),
        },
    );

    let handler_ms = handler_start.elapsed().as_secs_f64() * 1000.0;
    let total_ms = start.elapsed().as_secs_f64() * 1000.0;
    log_info(
        "hooks",
        "opencode.dispatch.timing",
        &format!(
            "hook={} handler_ms={:.2} total_ms={:.2} exit_code={}",
            hook_name, handler_ms, total_ms, exit_code
        ),
    );

    (exit_code, output)
}

/// Embedded hcom.ts plugin source (compiled into the binary).
pub const PLUGIN_SOURCE: &str = include_str!("../opencode_plugin/hcom.ts");

const PLUGIN_FILENAME: &str = "hcom.ts";

/// Resolve XDG_CONFIG_HOME with fallback to ~/.config.
fn xdg_config_home() -> String {
    std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{}/.config", home)
    })
}

/// Get the canonical plugin install directory.
///
/// Uses XDG_CONFIG_HOME with fallback to ~/.config.
pub fn get_global_plugin_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(xdg_config_home())
        .join("opencode")
        .join("plugins")
}

/// Get the canonical install path for the hcom.ts plugin.
pub fn get_opencode_plugin_path() -> std::path::PathBuf {
    get_global_plugin_dir().join(PLUGIN_FILENAME)
}

/// Scan all directories where hcom.ts plugin might exist.
///
/// Checks both plugin/ and plugins/ under:
/// - ~/.config/opencode/ (or XDG equivalent)
/// - OPENCODE_CONFIG_DIR if set
fn scan_plugin_dirs() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();

    let opencode_base = std::path::PathBuf::from(xdg_config_home()).join("opencode");
    candidates.push(opencode_base.join("plugin"));
    candidates.push(opencode_base.join("plugins"));

    if let Ok(custom_dir) = std::env::var("OPENCODE_CONFIG_DIR") {
        let custom_base = std::path::PathBuf::from(custom_dir);
        candidates.push(custom_base.join("plugin"));
        candidates.push(custom_base.join("plugins"));
    }

    candidates.into_iter().filter(|d| d.exists()).collect()
}

/// Check if hcom.ts plugin is installed in any OpenCode plugin directory.
pub fn verify_opencode_plugin_installed() -> bool {
    if get_opencode_plugin_path().exists() {
        return true;
    }
    scan_plugin_dirs()
        .iter()
        .any(|d| d.join(PLUGIN_FILENAME).exists())
}

/// Install the hcom.ts plugin to the canonical plugin directory.
///
/// Creates ~/.config/opencode/plugins/ if needed.
/// Writes the embedded plugin source directly (no file copy needed).
pub fn install_opencode_plugin() -> std::io::Result<bool> {
    let target_dir = get_global_plugin_dir();
    let target = target_dir.join(PLUGIN_FILENAME);

    std::fs::create_dir_all(&target_dir)?;

    // Remove stale symlinks before writing
    if target.is_symlink() || target.exists() {
        std::fs::remove_file(&target)?;
    }

    std::fs::write(&target, PLUGIN_SOURCE)?;
    Ok(true)
}

/// Remove hcom.ts from ALL OpenCode plugin directories.
pub fn remove_opencode_plugin() -> std::io::Result<()> {
    let mut paths = vec![get_opencode_plugin_path()];
    for d in scan_plugin_dirs() {
        let p = d.join(PLUGIN_FILENAME);
        if !paths.contains(&p) {
            paths.push(p);
        }
    }
    for p in paths {
        if p.exists() {
            std::fs::remove_file(&p)?;
        }
    }
    Ok(())
}

/// Check if installed plugin matches the embedded source.
fn plugin_is_current() -> bool {
    let installed = get_opencode_plugin_path();
    match std::fs::read_to_string(&installed) {
        Ok(content) => content == PLUGIN_SOURCE,
        Err(_) => false,
    }
}

/// Ensure the hcom.ts plugin is installed and up to date.
///
/// Used by the launcher for auto-install on first launch.
pub fn ensure_plugin_installed() -> bool {
    if verify_opencode_plugin_installed() && plugin_is_current() {
        return true;
    }
    install_opencode_plugin().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    /// Create a fresh test DB in a temp directory with schema initialized.
    fn test_db() -> (tempfile::TempDir, HcomDb) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        (dir, db)
    }

    // ── Argv parsing ──

    #[test]
    fn test_parse_flag_found() {
        let argv = sv(&["--session-id", "abc", "--notify-port", "12345"]);
        assert_eq!(parse_flag(&argv, "--session-id"), Some("abc".to_string()));
        assert_eq!(
            parse_flag(&argv, "--notify-port"),
            Some("12345".to_string())
        );
    }

    #[test]
    fn test_parse_flag_not_found() {
        let argv = sv(&["--session-id", "abc"]);
        assert_eq!(parse_flag(&argv, "--name"), None);
    }

    #[test]
    fn test_parse_flag_at_end() {
        // Flag at end with no value
        let argv = sv(&["--session-id"]);
        assert_eq!(parse_flag(&argv, "--session-id"), None);
    }

    #[test]
    fn test_has_flag() {
        let argv = sv(&["--name", "foo", "--format", "--check"]);
        assert!(has_flag(&argv, "--format"));
        assert!(has_flag(&argv, "--check"));
        assert!(!has_flag(&argv, "--ack"));
    }

    // ── Plugin management ──

    #[test]
    fn test_plugin_source_not_empty() {
        assert!(!PLUGIN_SOURCE.is_empty());
        assert!(PLUGIN_SOURCE.contains("HcomPlugin"));
    }

    #[test]
    fn test_get_global_plugin_dir() {
        let dir = get_global_plugin_dir();
        assert!(dir.ends_with("opencode/plugins"));
    }

    #[test]
    fn test_get_opencode_plugin_path() {
        let path = get_opencode_plugin_path();
        assert!(path.ends_with("hcom.ts"));
    }

    #[test]
    fn test_plugin_filename_constant() {
        assert_eq!(PLUGIN_FILENAME, "hcom.ts");
    }

    // ── Transcript path ──

    #[test]
    fn test_get_opencode_db_path_missing() {
        // In test env, opencode db won't exist
        // This tests the "not found" path
        let result = get_opencode_db_path();
        // Could be Some or None depending on environment, just verify it doesn't panic
        let _ = result;
    }

    // ── Handler tests (unit-level, isolated DB) ──

    #[test]
    fn test_handle_start_missing_session_id() {
        crate::config::Config::init();
        let ctx = HcomContext::from_os();
        let (_dir, db) = test_db();
        let argv = sv(&[]);
        let (code, output) = handle_start(&ctx, &db, &argv);
        assert_eq!(code, 0);
        assert!(output.contains("Missing --session-id"));
    }

    #[test]
    fn test_handle_status_missing_name() {
        let (_dir, db) = test_db();
        let argv = sv(&["--status", "listening"]);
        let (code, output) = handle_status(&db, &argv);
        assert_eq!(code, 0);
        assert!(output.contains("Missing --name or --status"));
    }

    #[test]
    fn test_handle_status_missing_status() {
        let (_dir, db) = test_db();
        let argv = sv(&["--name", "test"]);
        let (code, output) = handle_status(&db, &argv);
        assert_eq!(code, 0);
        assert!(output.contains("Missing --name or --status"));
    }

    #[test]
    fn test_handle_read_missing_name() {
        let (_dir, db) = test_db();
        let argv = sv(&[]);
        let (code, output) = handle_read(&db, &argv);
        assert_eq!(code, 0);
        assert!(output.contains("Missing --name"));
    }

    #[test]
    fn test_handle_read_check_empty() {
        let (_dir, db) = test_db();
        let argv = sv(&["--name", "testinst", "--check"]);
        let (code, output) = handle_read(&db, &argv);
        assert_eq!(code, 0);
        assert_eq!(output, "false");
    }

    #[test]
    fn test_handle_read_default_empty() {
        let (_dir, db) = test_db();
        let argv = sv(&["--name", "testinst"]);
        let (code, output) = handle_read(&db, &argv);
        assert_eq!(code, 0);
        assert_eq!(output, "[]");
    }

    #[test]
    fn test_handle_read_format_empty() {
        let (_dir, db) = test_db();
        let argv = sv(&["--name", "testinst", "--format"]);
        let (code, output) = handle_read(&db, &argv);
        assert_eq!(code, 0);
        assert_eq!(output, "");
    }

    #[test]
    fn test_handle_read_ack_empty() {
        let (_dir, db) = test_db();
        let argv = sv(&["--name", "testinst", "--ack"]);
        let (code, output) = handle_read(&db, &argv);
        assert_eq!(code, 0);
        assert!(output.contains("\"acked\":0") || output.contains("\"acked\": 0"));
    }

    #[test]
    fn test_handle_read_ack_up_to() {
        let (_dir, db) = test_db();
        let argv = sv(&["--name", "testinst", "--ack", "--up-to", "42"]);
        let (code, output) = handle_read(&db, &argv);
        assert_eq!(code, 0);
        assert!(output.contains("42"));
    }

    #[test]
    fn test_handle_read_ack_invalid_up_to() {
        let (_dir, db) = test_db();
        let argv = sv(&["--name", "testinst", "--ack", "--up-to", "abc"]);
        let (code, output) = handle_read(&db, &argv);
        assert_eq!(code, 0);
        assert!(output.contains("Invalid --up-to"));
    }

    #[test]
    fn test_handle_stop_missing_name() {
        let (_dir, db) = test_db();
        let argv = sv(&[]);
        let (code, output) = handle_stop(&db, &argv);
        assert_eq!(code, 0);
        assert!(output.contains("Missing --name"));
    }

    #[test]
    fn test_handle_stop_nonexistent() {
        crate::config::Config::init();
        let (_dir, db) = test_db();
        // finalize_session on nonexistent instance should be no-op
        let argv = sv(&["--name", "testinst", "--reason", "test"]);
        let (code, output) = handle_stop(&db, &argv);
        assert_eq!(code, 0);
        assert!(output.contains("\"ok\":true") || output.contains("\"ok\": true"));
    }

    #[test]
    fn test_handle_status_updates_status() {
        crate::config::Config::init();
        let (_dir, db) = test_db();
        // Create an instance first
        let _ = db.conn().execute(
            "INSERT INTO instances (name, tool, status, status_context, status_time, created_at) VALUES ('testinst', 'opencode', 'active', 'new', 0, 0)",
            [],
        );
        let argv = sv(&[
            "--name",
            "testinst",
            "--status",
            "listening",
            "--context",
            "idle",
        ]);
        let (code, output) = handle_status(&db, &argv);
        assert_eq!(code, 0);
        assert!(output.contains("\"ok\":true") || output.contains("\"ok\": true"));
    }

    #[test]
    fn test_handle_read_with_messages() {
        let (_dir, db) = test_db();
        // Create instance with last_event_id = 0
        let _ = db.conn().execute(
            "INSERT INTO instances (name, tool, status, status_context, status_time, created_at, last_event_id) VALUES ('testinst', 'opencode', 'listening', 'start', 0, 0, 0)",
            [],
        );
        // Insert a message event
        let _ = db.conn().execute(
            "INSERT INTO events (type, timestamp, instance, data) VALUES ('message', '2026-01-01T00:00:00Z', 'luna', '{\"from\":\"luna\",\"text\":\"hello\",\"scope\":\"broadcast\"}')",
            [],
        );
        // Default mode: raw JSON array
        let argv = sv(&["--name", "testinst"]);
        let (code, output) = handle_read(&db, &argv);
        assert_eq!(code, 0);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["from"], "luna");

        // Check mode
        let argv = sv(&["--name", "testinst", "--check"]);
        let (code, output) = handle_read(&db, &argv);
        assert_eq!(code, 0);
        assert_eq!(output, "true");
    }

    // ── Dispatcher (unit tests for routing logic, not full integration) ──

    #[test]
    fn test_dispatch_routes_correctly() {
        // Verify the match arms exist and route correctly via direct handler calls.
        // Full dispatch_opencode_hook() requires runtime (Config, DB) — tested via parity tests.
        let (_dir, db) = test_db();
        // Missing name → error JSON
        let (code, output) = handle_stop(&db, &sv(&[]));
        assert_eq!(code, 0);
        assert!(output.contains("Missing --name"));
    }
}
