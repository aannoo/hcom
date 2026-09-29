//! Pi Coding Agent hook handlers — argv-based lifecycle plus TypeScript plugin.

use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::bootstrap;
use crate::db::HcomDb;
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instances;
use crate::log::{log_error, log_info};
use crate::shared::ST_LISTENING;
use crate::shared::context::HcomContext;

use super::common;
use super::common::finalize_session;
use super::runtime::{self, LaunchCtx, PerRunAdapter, RuntimeInjection};

fn parse_flag(argv: &[String], flag: &str) -> Option<String> {
    argv.iter()
        .position(|a| a == flag)
        .and_then(|i| argv.get(i + 1))
        .cloned()
}

fn has_flag(argv: &[String], flag: &str) -> bool {
    argv.iter().any(|a| a == flag)
}

fn upsert_plugin_notify_endpoint(db: &HcomDb, instance_name: &str, port: u16) {
    if let Err(e) = db.upsert_notify_endpoint(instance_name, "plugin", port) {
        log_error(
            "native",
            "pi.register_notify_fail",
            &format!(
                "Failed to register plugin notify port for {}: {}",
                instance_name, e
            ),
        );
    }
}

fn initialize_last_event_id(db: &HcomDb, instance_name: &str) {
    if let Ok(Some(existing)) = db.get_instance_full(instance_name)
        && existing.last_event_id == 0
    {
        let launch_event_id: Option<i64> = std::env::var("HCOM_LAUNCH_EVENT_ID")
            .ok()
            .and_then(|s| s.parse().ok());
        let current_max = db.get_last_event_id();
        let new_id = match launch_event_id {
            Some(lei) if lei <= current_max => lei,
            _ => current_max,
        };
        let mut updates = serde_json::Map::new();
        updates.insert("last_event_id".into(), serde_json::json!(new_id));
        instances::update_instance_position(db, instance_name, &updates);
    }
}

fn bootstrap_for(ctx: &HcomContext, db: &HcomDb, instance_name: &str) -> String {
    bootstrap::get_bootstrap(db, ctx, instance_name, "pi")
}

fn handle_start(ctx: &HcomContext, db: &HcomDb, argv: &[String]) -> (i32, String) {
    // Plugin RPC returns JSON errors on exit 0 so the extension can handle
    // setup failures without Pi treating the hook itself as failed.
    let session_id = match parse_flag(argv, "--session-id") {
        Some(sid) => sid,
        None => return (0, r#"{"error":"Missing --session-id"}"#.to_string()),
    };
    let transcript_path = parse_flag(argv, "--transcript-path");
    let cwd = parse_flag(argv, "--cwd");
    let notify_port: Option<u16> = parse_flag(argv, "--notify-port").and_then(|s| s.parse().ok());

    let process_id = match &ctx.process_id {
        Some(pid) => pid.clone(),
        None => return (0, r#"{"error":"HCOM_PROCESS_ID not set"}"#.to_string()),
    };

    let instance_name =
        match instance_binding::bind_session_to_process(db, &session_id, Some(&process_id)) {
            Some(name) => name,
            None => {
                return (
                    0,
                    r#"{"error":"No instance bound to this process"}"#.to_string(),
                );
            }
        };

    initialize_last_event_id(db, &instance_name);
    lifecycle::set_status(
        db,
        &instance_name,
        ST_LISTENING,
        "start",
        Default::default(),
    );
    instance_binding::capture_and_store_launch_context(db, &instance_name);

    let mut updates = serde_json::Map::new();
    updates.insert("tool".into(), serde_json::json!("pi"));
    updates.insert("session_id".into(), serde_json::json!(&session_id));
    if let Some(path) = transcript_path.as_ref().filter(|p| !p.is_empty()) {
        updates.insert("transcript_path".into(), serde_json::json!(path));
    }
    let cwd_value = cwd
        .as_deref()
        .filter(|p| !p.is_empty())
        .or_else(|| ctx.cwd.to_str());
    if let Some(cwd) = cwd_value {
        updates.insert("directory".into(), serde_json::json!(cwd));
    }
    instances::update_instance_position(db, &instance_name, &updates);
    if let Some(port) = notify_port {
        upsert_plugin_notify_endpoint(db, &instance_name, port);
    }
    log_info(
        "hooks",
        "pi-start.bind",
        &format!("instance={} session_id={}", instance_name, session_id),
    );
    crate::relay::worker::ensure_worker(true);

    let response = serde_json::json!({
        "name": instance_name,
        "session_id": session_id,
        "bootstrap": bootstrap_for(ctx, db, &instance_name),
    });
    (0, response.to_string())
}

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
    let was_listening = db
        .get_instance_full(&name)
        .ok()
        .flatten()
        .is_some_and(|inst| inst.status == ST_LISTENING);

    lifecycle::set_status(
        db,
        &name,
        &status,
        &context,
        lifecycle::StatusUpdate {
            detail: &detail,
            ..Default::default()
        },
    );
    if status == ST_LISTENING && !was_listening {
        crate::notify::wake(db, &name, &[]);
    }
    (0, r#"{"ok":true}"#.to_string())
}

fn handle_read(db: &HcomDb, argv: &[String]) -> (i32, String) {
    let name = match parse_flag(argv, "--name") {
        Some(n) => n,
        None => return (0, r#"{"error":"Missing --name"}"#.to_string()),
    };
    let format_mode = has_flag(argv, "--format");
    let check_mode = has_flag(argv, "--check");
    let ack_mode = has_flag(argv, "--ack");

    let raw_messages = db.get_unread_messages(&name);
    let messages: Vec<Value> = raw_messages.iter().map(common::message_to_value).collect();

    if format_mode {
        if messages.is_empty() {
            return (0, String::new());
        }
        let deliver = common::limit_delivery_messages(&messages);
        return (
            0,
            common::format_messages_json_for_instance(db, &deliver, &name),
        );
    }
    if ack_mode {
        if let Some(up_to) = parse_flag(argv, "--up-to") {
            let Ok(ack_id) = up_to.parse::<i64>() else {
                return (
                    0,
                    serde_json::json!({"error": format!("Invalid --up-to: {}", up_to)}).to_string(),
                );
            };
            let mut updates = serde_json::Map::new();
            updates.insert("last_event_id".into(), serde_json::json!(ack_id));
            instances::update_instance_position(db, &name, &updates);
            return (0, serde_json::json!({"acked_to": ack_id}).to_string());
        }
        if messages.is_empty() {
            return (0, r#"{"acked":0}"#.to_string());
        }
        let ack_id = messages
            .iter()
            .filter_map(|m| m.get("event_id").and_then(|v| v.as_i64()))
            .max()
            .filter(|id| *id > 0)
            .unwrap_or_else(|| db.get_last_event_id());
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
    (
        0,
        serde_json::to_string(&messages).unwrap_or_else(|_| "[]".to_string()),
    )
}

fn handle_beforetool(db: &HcomDb, argv: &[String]) -> (i32, String) {
    let name = match parse_flag(argv, "--name") {
        Some(n) => n,
        None => return (0, r#"{"decision":"allow"}"#.to_string()),
    };
    let tool_name = parse_flag(argv, "--tool").unwrap_or_default();
    let input = parse_flag(argv, "--input-json")
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !tool_name.is_empty() {
        common::update_tool_status(db, &name, "pi", &tool_name, &input);
    }
    (0, r#"{"decision":"allow"}"#.to_string())
}

fn handle_stop(db: &HcomDb, argv: &[String]) -> (i32, String) {
    let name = match parse_flag(argv, "--name") {
        Some(n) => n,
        None => return (0, r#"{"error":"Missing --name"}"#.to_string()),
    };
    let reason = parse_flag(argv, "--reason").unwrap_or_else(|| "unknown".to_string());
    finalize_session(db, &name, &reason, None);
    (0, r#"{"ok":true}"#.to_string())
}

pub fn dispatch_pi_hook(hook_name: &str, argv: &[String]) -> (i32, String) {
    let start = Instant::now();
    let ctx = HcomContext::from_os();
    crate::paths::ensure_hcom_directories_at(&ctx.hcom_dir);
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
    if !common::hook_gate_check(&ctx, &db) {
        return (0, String::new());
    }
    let handler_argv: Vec<String> = if !argv.is_empty() && argv[0] == hook_name {
        argv[1..].to_vec()
    } else {
        argv.to_vec()
    };
    let hook_name_owned = hook_name.to_string();
    let handler_start = Instant::now();
    let (exit_code, output) = common::dispatch_with_panic_guard(
        "pi",
        &hook_name_owned,
        (
            0,
            serde_json::json!({"error": "internal panic"}).to_string(),
        ),
        || match hook_name_owned.as_str() {
            "pi-start" => handle_start(&ctx, &db, &handler_argv),
            "pi-status" => handle_status(&db, &handler_argv),
            "pi-read" => handle_read(&db, &handler_argv),
            "pi-beforetool" => handle_beforetool(&db, &handler_argv),
            "pi-stop" => handle_stop(&db, &handler_argv),
            _ => (
                0,
                serde_json::json!({"error": format!("Unknown Pi hook: {}", hook_name_owned)})
                    .to_string(),
            ),
        },
    );
    log_info(
        "hooks",
        "pi.dispatch.timing",
        &format!(
            "hook={} handler_ms={:.2} total_ms={:.2} exit_code={}",
            hook_name,
            handler_start.elapsed().as_secs_f64() * 1000.0,
            start.elapsed().as_secs_f64() * 1000.0,
            exit_code
        ),
    );
    (exit_code, output)
}

pub const PLUGIN_SOURCE: &str = include_str!("../pi_plugin/hcom.ts");
const PLUGIN_FILENAME: &str = "hcom.ts";

pub static PER_RUN: PerRunAdapter = PerRunAdapter {
    prepare: prepare_per_run,
    cleanup_legacy: cleanup_legacy_per_run,
    ensure_permissions: None,
    managed_value_flags: &["-e", "--extension"],
    strip_legacy_args: None,
};

fn current_home_dir() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default())
}

fn pi_plugin_dir() -> std::path::PathBuf {
    let tool_root = crate::runtime_env::tool_config_root();
    let home = current_home_dir();
    if tool_root == home {
        if let Ok(dir) = std::env::var("PI_CODING_AGENT_DIR")
            && !dir.is_empty()
        {
            return std::path::PathBuf::from(dir).join("extensions");
        }
        home.join(".pi").join("agent").join("extensions")
    } else {
        tool_root.join(".pi").join("extensions")
    }
}

pub fn get_pi_plugin_path() -> std::path::PathBuf {
    pi_plugin_dir().join(PLUGIN_FILENAME)
}

fn effective_plugin_path(ctx: &LaunchCtx) -> std::path::PathBuf {
    let agent_dir = ctx.path_var("PI_CODING_AGENT_DIR").unwrap_or_else(|| {
        ctx.var("HOME")
            .map(std::path::PathBuf::from)
            .or_else(dirs::home_dir)
            .unwrap_or_default()
            .join(".pi")
            .join("agent")
    });
    agent_dir.join("extensions").join(PLUGIN_FILENAME)
}

fn prepare_per_run(ctx: &LaunchCtx) -> Result<RuntimeInjection> {
    let path = runtime::publish_file("pi", PLUGIN_FILENAME, PLUGIN_SOURCE.as_bytes())
        .context("Cannot publish Pi runtime extension")?;
    let mut args = ctx.args.clone();
    runtime::insert_before_separator(
        &mut args,
        ["-e".to_string(), path.to_string_lossy().into_owned()],
    );
    Ok(RuntimeInjection {
        args,
        env: Vec::new(),
    })
}

/// Legacy installs went to the launch's agent dir, or to `<tool root>/.pi/`
/// under a project-local HCOM_DIR ([`get_pi_plugin_path`]); check both.
fn cleanup_legacy_per_run(ctx: &LaunchCtx) -> Result<()> {
    let mut paths = vec![effective_plugin_path(ctx)];
    let installer_path = get_pi_plugin_path();
    if !paths.contains(&installer_path) {
        paths.push(installer_path);
    }
    crate::hooks::runtime::remove_owned_files(paths, is_hcom_owned)
}

/// True when `path` holds an hcom Pi plugin: the current source or any earlier
/// version (all carry the `hcom-bootstrap` message type).
pub fn is_hcom_owned(path: &std::path::Path) -> std::io::Result<bool> {
    crate::hooks::runtime::file_is_hcom_owned(path, |content| {
        content == PLUGIN_SOURCE || content.contains("customType: \"hcom-bootstrap\"")
    })
}

/// Remove hcom's plugin file. A user file with the same name is left alone.
pub fn remove_pi_plugin() -> std::io::Result<()> {
    let path = get_pi_plugin_path();
    if is_hcom_owned(&path)? {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::{ST_ACTIVE, ST_LISTENING};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::time::Duration;

    fn setup_test_db() -> (HcomDb, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!(
            "test_pi_hooks_{}_{}.db",
            std::process::id(),
            test_id
        ));

        let db = HcomDb::open_at(&db_path).unwrap();
        (db, db_path)
    }

    fn cleanup(path: PathBuf) {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    fn save_test_instance(db: &HcomDb, name: &str, status: &str) {
        let mut row = serde_json::Map::new();
        row.insert("name".into(), serde_json::json!(name));
        row.insert("tool".into(), serde_json::json!("pi"));
        row.insert("status".into(), serde_json::json!(status));
        row.insert("status_context".into(), serde_json::json!(""));
        row.insert("status_detail".into(), serde_json::json!(""));
        row.insert("created_at".into(), serde_json::json!(1.0));
        db.save_instance_named(name, &row).unwrap();
    }

    #[test]
    fn plugin_bootstraps_via_hidden_message() {
        assert!(PLUGIN_SOURCE.contains("before_agent_start"));
        assert!(PLUGIN_SOURCE.contains("customType: \"hcom-bootstrap\""));
        assert!(PLUGIN_SOURCE.contains("display: false"));
        assert!(!PLUGIN_SOURCE.contains("text: `${bootstrapText}\\n\\n${event.text}`"));
    }

    #[test]
    fn plugin_reconcile_does_not_report_active_polling_status() {
        assert!(!PLUGIN_SOURCE.contains(
            "reportStatus(currentCtx, currentCtx.isIdle() ? \"listening\" : \"active\")"
        ));
        assert!(PLUGIN_SOURCE.contains("pi.on(\"agent_end\""));
        assert!(PLUGIN_SOURCE.contains("IDLE_DEBOUNCE_MS"));
        assert!(PLUGIN_SOURCE.contains("currentCtx?.isIdle()"));
        assert!(!PLUGIN_SOURCE.contains("pi.on(\"turn_end\", async (_event, ctx) => {\n\t\tcurrentCtx = ctx;\n\t\tawait reportStatus(ctx, \"listening\");"));
    }

    #[test]
    fn plugin_stops_only_on_quit() {
        assert!(PLUGIN_SOURCE.contains("event.reason === undefined || event.reason === \"quit\""));
        assert!(PLUGIN_SOURCE.contains("if (instanceName && terminal)"));
    }

    #[test]
    fn plugin_delivery_reports_active_edge() {
        assert!(PLUGIN_SOURCE.contains("reportStatus(ctx, \"active\""));
        assert!(PLUGIN_SOURCE.contains("`deliver:${sender}`"));
    }

    #[test]
    fn status_handler_wakes_plugin_only_when_entering_listening() {
        let (db, path) = setup_test_db();
        save_test_instance(&db, "luna", ST_LISTENING);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        db.upsert_notify_endpoint("luna", "plugin", port).unwrap();

        let argv = vec![
            "--name".to_string(),
            "luna".to_string(),
            "--status".to_string(),
            ST_LISTENING.to_string(),
        ];
        let (code, _) = handle_status(&db, &argv);
        assert_eq!(code, 0);
        std::thread::sleep(Duration::from_millis(20));
        assert!(listener.accept().is_err());

        let mut updates = serde_json::Map::new();
        updates.insert("status".into(), serde_json::json!(ST_ACTIVE));
        instances::update_instance_position(&db, "luna", &updates);

        let (code, _) = handle_status(&db, &argv);
        assert_eq!(code, 0);
        let mut accepted = false;
        for _ in 0..10 {
            if listener.accept().is_ok() {
                accepted = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(accepted);

        cleanup(path);
    }

    /// reload/new keep the process alive and skip pi-stop, so the next pi-start
    /// must rebind the same identity through the process binding.
    #[test]
    fn start_handler_keeps_identity_across_same_process_session_restart() {
        let (db, path) = setup_test_db();
        let temp = tempfile::TempDir::new().unwrap();
        save_test_instance(&db, "soba", ST_LISTENING);
        db.set_process_binding("pid-1", "", "soba").unwrap();

        let env = std::collections::HashMap::from([
            ("HCOM_PROCESS_ID".to_string(), "pid-1".to_string()),
            ("HCOM_LAUNCHED".to_string(), "1".to_string()),
            ("HCOM_TOOL".to_string(), "pi".to_string()),
        ]);
        let ctx = HcomContext::from_env(&env, temp.path().to_path_buf());
        let start = |sid: &str| {
            let (code, output) = handle_start(&ctx, &db, &["--session-id".into(), sid.into()]);
            assert_eq!(code, 0);
            serde_json::from_str::<Value>(&output).unwrap()["name"]
                .as_str()
                .map(str::to_string)
        };

        // initial bind, /reload (same session), /new (fresh session)
        for sid in ["sid-1", "sid-1", "sid-2"] {
            assert_eq!(start(sid).as_deref(), Some("soba"));
            let inst = db.get_instance_full("soba").unwrap().unwrap();
            assert_eq!(inst.session_id.as_deref(), Some(sid));
            assert_eq!(
                db.get_session_binding(sid).unwrap().as_deref(),
                Some("soba")
            );
        }

        cleanup(path);
    }

    #[test]
    fn start_handler_uses_central_binding_for_existing_session() {
        let (db, path) = setup_test_db();
        let temp = tempfile::TempDir::new().unwrap();

        let mut canonical = serde_json::Map::new();
        canonical.insert("name".into(), serde_json::json!("miso"));
        canonical.insert("tool".into(), serde_json::json!("pi"));
        canonical.insert("session_id".into(), serde_json::json!("sid-123"));
        canonical.insert("status".into(), serde_json::json!(ST_LISTENING));
        canonical.insert("status_context".into(), serde_json::json!(""));
        canonical.insert("status_detail".into(), serde_json::json!(""));
        canonical.insert("last_event_id".into(), serde_json::json!(42));
        canonical.insert("created_at".into(), serde_json::json!(1.0));
        db.save_instance_named("miso", &canonical).unwrap();
        db.rebind_session("sid-123", "miso").unwrap();

        let mut placeholder = serde_json::Map::new();
        placeholder.insert("name".into(), serde_json::json!("temp"));
        placeholder.insert("tool".into(), serde_json::json!("pi"));
        placeholder.insert("status".into(), serde_json::json!("pending"));
        placeholder.insert("status_context".into(), serde_json::json!("new"));
        placeholder.insert("status_detail".into(), serde_json::json!(""));
        placeholder.insert("created_at".into(), serde_json::json!(1.0));
        db.save_instance_named("temp", &placeholder).unwrap();
        db.set_process_binding("pid-123", "", "temp").unwrap();

        let env = std::collections::HashMap::from([
            ("HCOM_PROCESS_ID".to_string(), "pid-123".to_string()),
            ("HCOM_LAUNCHED".to_string(), "1".to_string()),
            ("HCOM_TOOL".to_string(), "pi".to_string()),
        ]);
        let ctx = HcomContext::from_env(&env, temp.path().to_path_buf());

        let (code, output) = handle_start(
            &ctx,
            &db,
            &[
                "--session-id".to_string(),
                "sid-123".to_string(),
                "--cwd".to_string(),
                temp.path().to_string_lossy().to_string(),
            ],
        );
        assert_eq!(code, 0);
        let response: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(response.get("name").and_then(|v| v.as_str()), Some("miso"));
        assert!(db.get_instance_full("temp").unwrap().is_none());
        assert_eq!(
            db.get_process_binding("pid-123").unwrap(),
            Some("miso".to_string())
        );

        let rebound = db.get_instance_full("miso").unwrap().unwrap();
        assert_eq!(rebound.last_event_id, 42);
        assert_eq!(rebound.directory, temp.path().to_string_lossy());

        cleanup(path);
    }

    #[test]
    fn test_is_hcom_owned_matches_hcom_plugins_only() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("current.ts");
        let older = dir.path().join("older.ts");
        let user = dir.path().join("user.ts");
        std::fs::write(&current, PLUGIN_SOURCE).unwrap();
        std::fs::write(&older, "sendMessage({ customType: \"hcom-bootstrap\" })").unwrap();
        std::fs::write(&user, "export default function mine() {}").unwrap();
        assert!(is_hcom_owned(&current).unwrap());
        assert!(is_hcom_owned(&older).unwrap());
        assert!(!is_hcom_owned(&user).unwrap());
        assert!(!is_hcom_owned(&dir.path().join("missing.ts")).unwrap());
        // Something unreadable in the way is an error, not "not ours".
        assert!(is_hcom_owned(dir.path()).is_err());
    }

    #[test]
    #[serial_test::serial]
    fn per_run_injection_uses_runtime_extension_before_separator() {
        let (_dir, hcom, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let ctx = LaunchCtx {
            tool: crate::tool::Tool::Pi,
            env: [("HOME".to_string(), home.to_string_lossy().into_owned())]
                .into_iter()
                .collect(),
            cwd: home,
            args: vec!["--model".into(), "x".into(), "--".into(), "prompt".into()],
            auto_approve: false,
        };
        let injection = (PER_RUN.prepare)(&ctx).unwrap();
        assert_eq!(&injection.args[..2], &["--model", "x"]);
        assert_eq!(injection.args[2], "-e");
        let path = std::path::Path::new(&injection.args[3]);
        assert!(path.starts_with(hcom.join("integrations").join("pi")));
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("hcom.ts"));
        assert_eq!(&injection.args[4..], &["--", "prompt"]);
    }

    #[test]
    fn per_run_cleanup_uses_launch_env_and_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = dir.path().join("work");
        let plugin = cwd
            .join("relative-agent")
            .join("extensions")
            .join("hcom.ts");
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        std::fs::write(&plugin, PLUGIN_SOURCE).unwrap();

        let ctx = LaunchCtx {
            tool: crate::tool::Tool::Pi,
            env: [
                ("HOME".to_string(), home.to_string_lossy().into_owned()),
                (
                    "PI_CODING_AGENT_DIR".to_string(),
                    "relative-agent".to_string(),
                ),
            ]
            .into_iter()
            .collect(),
            cwd,
            args: Vec::new(),
            auto_approve: false,
        };
        (PER_RUN.cleanup_legacy)(&ctx).unwrap();
        assert!(!plugin.exists());
    }
}
