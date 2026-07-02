//! Omp Coding Agent hook handlers — argv-based lifecycle plus TypeScript plugin.

use std::time::Instant;

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
            "omp.register_notify_fail",
            &format!(
                "Failed to register plugin notify port for {}: {}",
                instance_name, e
            ),
        );
        return;
    }

    crate::notify::wake(db, instance_name, crate::notify::WakeKind::DELIVERY_LOOPS);
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
    let tag = db
        .get_instance_full(instance_name)
        .ok()
        .flatten()
        .and_then(|d| d.tag.clone())
        .unwrap_or_default();
    let hcom_config = crate::config::HcomConfig::load(None).unwrap_or_default();
    let relay_enabled = crate::relay::is_relay_enabled(&hcom_config);
    let effective_tag = if tag.is_empty() {
        &hcom_config.tag
    } else {
        &tag
    };
    bootstrap::get_bootstrap(
        db,
        &ctx.hcom_dir,
        instance_name,
        "omp",
        ctx.is_background,
        ctx.is_launched,
        &ctx.notes,
        effective_tag,
        relay_enabled,
        ctx.background_name.as_deref(),
    )
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
    updates.insert("tool".into(), serde_json::json!("omp"));
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
        "omp-start.bind",
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
        common::update_tool_status(db, &name, "omp", &tool_name, &input);
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

pub fn dispatch_omp_hook(hook_name: &str, argv: &[String]) -> (i32, String) {
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
        "omp",
        &hook_name_owned,
        (
            0,
            serde_json::json!({"error": "internal panic"}).to_string(),
        ),
        || match hook_name_owned.as_str() {
            "omp-start" => handle_start(&ctx, &db, &handler_argv),
            "omp-status" => handle_status(&db, &handler_argv),
            "omp-read" => handle_read(&db, &handler_argv),
            "omp-beforetool" => handle_beforetool(&db, &handler_argv),
            "omp-stop" => handle_stop(&db, &handler_argv),
            _ => (
                0,
                serde_json::json!({"error": format!("Unknown Omp hook: {}", hook_name_owned)})
                    .to_string(),
            ),
        },
    );
    log_info(
        "hooks",
        "omp.dispatch.timing",
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

pub const PLUGIN_SOURCE: &str = include_str!("../omp_plugin/hcom.ts");
const PLUGIN_FILENAME: &str = "hcom.ts";

fn current_home_dir() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default())
}

fn omp_plugin_dir() -> std::path::PathBuf {
    let tool_root = crate::runtime_env::tool_config_root();
    let home = current_home_dir();
    if tool_root == home {
        if let Ok(dir) = std::env::var("PI_CODING_AGENT_DIR")
            && !dir.is_empty()
        {
            return std::path::PathBuf::from(dir).join("extensions");
        }
        home.join(".omp").join("agent").join("extensions")
    } else {
        tool_root.join(".omp").join("extensions")
    }
}

pub fn get_omp_plugin_path() -> std::path::PathBuf {
    omp_plugin_dir().join(PLUGIN_FILENAME)
}

pub fn extension_inject_args() -> Vec<String> {
    vec![
        "-e".to_string(),
        get_omp_plugin_path().to_string_lossy().to_string(),
    ]
}

/// Remove hcom's managed OMP extension injection (`-e <hcom.ts>` /
/// `--extension …`, incl. the `=` forms) from a stored or replayed launch-arg
/// vector, preserving every user-supplied extension and its ordering. An entry
/// is treated as managed when its path is the current plugin path, an existing
/// hcom-owned file, or — for a moved/missing managed file — a narrow lexical
/// match (basename `hcom.ts` directly under an `extensions` directory).
///
/// Idempotent. Callers strip stored args before snapshotting and reinjecting so
/// a stale plugin path from an older hcom/config layout is not replayed
/// alongside the freshly injected current path (which could fail startup or load
/// hcom twice). A genuine `-e other.ts` user extension always survives.
pub fn strip_managed_extension_args(args: &mut Vec<String>) {
    let current = get_omp_plugin_path();
    let is_managed = |value: &str| -> bool {
        let path = std::path::Path::new(value);
        if path == current.as_path() {
            return true;
        }
        if is_hcom_owned(path) {
            return true;
        }
        // Moved/missing managed file only: basename hcom.ts under an `extensions`
        // dir. Gated on !exists so an EXISTING user `-e …/extensions/hcom.ts`
        // with unrelated contents (which is_hcom_owned already rejected) is kept
        // — only exact-current and hcom-owned files are removed when present.
        !path.exists()
            && path.file_name().and_then(|n| n.to_str()) == Some(PLUGIN_FILENAME)
            && path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                == Some("extensions")
    };
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let tok = args[i].as_str();
        // Two-token forms: `-e PATH` / `--extension PATH`.
        if (tok == "-e" || tok == "--extension") && i + 1 < args.len() {
            if is_managed(&args[i + 1]) {
                i += 2;
                continue;
            }
            out.push(args[i].clone());
            out.push(args[i + 1].clone());
            i += 2;
            continue;
        }
        // Equals forms: `--extension=PATH` / `-e=PATH`.
        if let Some(value) = tok
            .strip_prefix("--extension=")
            .or_else(|| tok.strip_prefix("-e="))
            && is_managed(value)
        {
            i += 1;
            continue;
        }
        out.push(args[i].clone());
        i += 1;
    }
    *args = out;
}

fn plugin_matches_source(path: &std::path::Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(content) => content == PLUGIN_SOURCE,
        Err(_) => false,
    }
}

pub fn verify_omp_plugin_installed() -> bool {
    plugin_matches_source(&get_omp_plugin_path())
}

fn is_hcom_owned(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path)
        .map(|content| content.contains("customType: \"hcom-bootstrap\""))
        .unwrap_or(false)
}

pub fn install_omp_plugin() -> std::io::Result<bool> {
    let target_dir = omp_plugin_dir();
    let target = target_dir.join(PLUGIN_FILENAME);
    std::fs::create_dir_all(&target_dir)?;
    if target.is_symlink() || target.exists() {
        if !plugin_matches_source(&target) && !is_hcom_owned(&target) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "A non-hcom hcom.ts file already exists and will not be overwritten",
            ));
        }
        std::fs::remove_file(&target)?;
    }
    std::fs::write(&target, PLUGIN_SOURCE)?;
    Ok(true)
}

pub fn ensure_omp_plugin_installed() -> bool {
    if verify_omp_plugin_installed() {
        return true;
    }
    install_omp_plugin().unwrap_or(false)
}

pub fn remove_omp_plugin() -> std::io::Result<()> {
    let path = get_omp_plugin_path();
    if (path.exists() || path.is_symlink())
        && (plugin_matches_source(&path) || is_hcom_owned(&path))
    {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::{ST_ACTIVE, ST_LISTENING};
    use std::io::ErrorKind;
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn setup_test_db() -> (HcomDb, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!(
            "test_omp_hooks_{}_{}.db",
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
        row.insert("tool".into(), serde_json::json!("omp"));
        row.insert("status".into(), serde_json::json!(status));
        row.insert("status_context".into(), serde_json::json!(""));
        row.insert("status_detail".into(), serde_json::json!(""));
        row.insert("created_at".into(), serde_json::json!(1.0));
        db.save_instance_named(name, &row).unwrap();
    }

    fn bind_probe() -> TcpListener {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        listener
    }

    fn await_connect(listener: &TcpListener, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match listener.accept() {
                Ok(_) => return true,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return false,
            }
        }
    }

    #[test]
    fn strip_managed_extension_removes_only_hcom_entry() {
        let current = get_omp_plugin_path().to_string_lossy().to_string();

        // Current managed path (two-token) is removed; user extension survives.
        let mut args = vec![
            "--model".into(),
            "opus".into(),
            "-e".into(),
            current.clone(),
            "-e".into(),
            "/home/u/mine.ts".into(),
        ];
        strip_managed_extension_args(&mut args);
        assert_eq!(
            args,
            vec!["--model", "opus", "-e", "/home/u/mine.ts"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );

        // Idempotent.
        let once = args.clone();
        strip_managed_extension_args(&mut args);
        assert_eq!(args, once);

        // Legacy/moved managed path (missing file) via lexical fallback:
        // basename hcom.ts under an `extensions` dir, incl. the `=` form.
        let mut legacy = vec![
            "--extension=/old/place/.omp/agent/extensions/hcom.ts".into(),
            "--extension".into(),
            "/old/place/extensions/hcom.ts".into(),
            "--extension=/home/u/other.ts".into(),
        ];
        strip_managed_extension_args(&mut legacy);
        assert_eq!(legacy, vec!["--extension=/home/u/other.ts".to_string()]);

        // A user extension merely named hcom.ts but NOT under `extensions/` is
        // preserved (narrow matcher).
        let mut keep = vec!["-e".into(), "/home/u/project/hcom.ts".into()];
        strip_managed_extension_args(&mut keep);
        assert_eq!(
            keep,
            vec!["-e".to_string(), "/home/u/project/hcom.ts".to_string()]
        );

        // An EXISTING non-hcom file at extensions/hcom.ts must survive: the
        // lexical fallback applies only to moved/missing files.
        let dir = tempfile::tempdir().unwrap();
        let ext_dir = dir.path().join("extensions");
        std::fs::create_dir_all(&ext_dir).unwrap();
        let user_file = ext_dir.join("hcom.ts");
        std::fs::write(&user_file, "export default () => {}; // not hcom").unwrap();
        let user_arg = user_file.to_string_lossy().to_string();
        let mut existing = vec!["-e".into(), user_arg.clone()];
        strip_managed_extension_args(&mut existing);
        assert_eq!(existing, vec!["-e".to_string(), user_arg]);
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
    fn plugin_delivery_reports_active_edge() {
        assert!(PLUGIN_SOURCE.contains("reportStatus(ctx, \"active\""));
        assert!(PLUGIN_SOURCE.contains("`deliver:${sender}`"));
    }

    // The embedded plugin is include_str!'d and never tsc'd, so these guard the
    // delivery-correctness invariants that upstream API/lifecycle drift silently
    // broke before (see PR review). They pin behavior, not just strings.

    #[test]
    fn plugin_acks_transform_submission_in_before_agent_start() {
        // omp applies the bodyless-wake transform inline (no source:"extension"
        // re-emit), so the transform-path ack must happen in before_agent_start.
        // Without it pendingAckId stays set and deliverPending jams forever.
        let idx = PLUGIN_SOURCE
            .find("pi.on(\"before_agent_start\"")
            .expect("before_agent_start handler present");
        assert!(
            PLUGIN_SOURCE[idx..].contains("ackPending(\"before_agent_start\")"),
            "before_agent_start must ack the inline transform submission"
        );
        assert!(PLUGIN_SOURCE.contains("if (pendingAckId !== null) await ackPending"));
    }

    #[test]
    fn plugin_replays_wakes_dropped_during_in_flight_window() {
        // In-flight/pending-ack wakes must be queued and replayed, not dropped.
        assert!(PLUGIN_SOURCE.contains("deliveryPending"));
        assert!(PLUGIN_SOURCE.contains("schedulePendingDelivery"));
        assert!(PLUGIN_SOURCE.contains("drainPendingDelivery"));
        // ackPending drains so the transform-path ack replays queued wakes.
        let idx = PLUGIN_SOURCE
            .find("async function ackPending")
            .expect("ackPending present");
        assert!(PLUGIN_SOURCE[idx..].contains("drainPendingDelivery(\"post_ack_wake\")"));
    }

    #[test]
    fn plugin_keeps_ack_gate_until_command_succeeds() {
        let idx = PLUGIN_SOURCE
            .find("async function ackPending")
            .expect("ackPending present");
        let ack = &PLUGIN_SOURCE[idx..];
        let command = ack.find("await hcom([\"omp-read\"").expect("ack command");
        let clear = ack.find("pendingAckId = null").expect("pending ack clear");
        assert!(
            command < clear,
            "pendingAckId must remain set while the ack command is in flight"
        );
        assert!(ack.contains("if (result.code !== 0)"));
        assert!(ack.contains("plugin.delivery_ack_failed"));
        assert!(PLUGIN_SOURCE.contains("ackInFlight"));
        assert!(PLUGIN_SOURCE.contains("await ackPending(\"reconcile\")"));
    }

    #[test]
    fn plugin_rebinds_identity_on_session_branch() {
        // /branch mints a new session id and emits only session_branch (not
        // session_switch); the plugin must reset+rebind or delivery dies.
        let idx = PLUGIN_SOURCE
            .find("pi.on(\"session_branch\"")
            .expect("session_branch handler present");
        let handler = &PLUGIN_SOURCE[idx..];
        assert!(handler.contains("resetBinding()"));
        assert!(handler.contains("bindIdentity(ctx)"));
    }

    #[test]
    fn start_handler_registering_plugin_notify_wakes_pty_delivery_loop() {
        let (db, path) = setup_test_db();
        let temp = tempfile::TempDir::new().unwrap();
        save_test_instance(&db, "luna", ST_ACTIVE);
        db.set_process_binding("pid-omp", "", "luna").unwrap();

        let pty_listener = bind_probe();
        let pty_port = pty_listener.local_addr().unwrap().port();
        db.upsert_notify_endpoint("luna", "pty", pty_port).unwrap();

        let plugin_listener = bind_probe();
        let plugin_port = plugin_listener.local_addr().unwrap().port();

        let env = std::collections::HashMap::from([
            ("HCOM_PROCESS_ID".to_string(), "pid-omp".to_string()),
            ("HCOM_LAUNCHED".to_string(), "1".to_string()),
            ("HCOM_TOOL".to_string(), "omp".to_string()),
        ]);
        let ctx = HcomContext::from_env(&env, temp.path().to_path_buf());

        let (code, output) = handle_start(
            &ctx,
            &db,
            &[
                "--session-id".to_string(),
                "sid-omp".to_string(),
                "--notify-port".to_string(),
                plugin_port.to_string(),
                "--cwd".to_string(),
                temp.path().to_string_lossy().to_string(),
            ],
        );

        assert_eq!(code, 0);
        let response: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(response.get("name").and_then(|v| v.as_str()), Some("luna"));
        assert!(db.has_notify_endpoint_kind("luna", "plugin"));

        let stored_plugin_port: i64 = db
            .conn()
            .query_row(
                "SELECT port FROM notify_endpoints WHERE instance = 'luna' AND kind = 'plugin'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_plugin_port, i64::from(plugin_port));
        assert!(
            await_connect(&pty_listener, Duration::from_millis(500)),
            "successful plugin bind must wake the PTY delivery loop so launch readiness is observed promptly"
        );

        drop(plugin_listener);
        cleanup(path);
    }

    #[test]
    fn plugin_notify_registration_failure_does_not_wake_delivery_loop() {
        let (db, path) = setup_test_db();
        save_test_instance(&db, "luna", ST_ACTIVE);

        let pty_listener = bind_probe();
        let pty_port = pty_listener.local_addr().unwrap().port();
        db.upsert_notify_endpoint("luna", "pty", pty_port).unwrap();

        db.conn()
            .execute_batch(
                "CREATE TRIGGER fail_plugin_notify_insert
                 BEFORE INSERT ON notify_endpoints
                 WHEN NEW.kind = 'plugin'
                 BEGIN
                   SELECT RAISE(ABORT, 'plugin registration blocked');
                 END;",
            )
            .unwrap();

        let plugin_listener = bind_probe();
        let plugin_port = plugin_listener.local_addr().unwrap().port();
        upsert_plugin_notify_endpoint(&db, "luna", plugin_port);

        assert!(!db.has_notify_endpoint_kind("luna", "plugin"));
        assert!(
            !await_connect(&pty_listener, Duration::from_millis(100)),
            "failed plugin bind must not wake delivery loops"
        );

        drop(plugin_listener);
        cleanup(path);
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

    #[test]
    fn start_handler_uses_central_binding_for_existing_session() {
        let (db, path) = setup_test_db();
        let temp = tempfile::TempDir::new().unwrap();

        let mut canonical = serde_json::Map::new();
        canonical.insert("name".into(), serde_json::json!("miso"));
        canonical.insert("tool".into(), serde_json::json!("omp"));
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
        placeholder.insert("tool".into(), serde_json::json!("omp"));
        placeholder.insert("status".into(), serde_json::json!("pending"));
        placeholder.insert("status_context".into(), serde_json::json!("new"));
        placeholder.insert("status_detail".into(), serde_json::json!(""));
        placeholder.insert("created_at".into(), serde_json::json!(1.0));
        db.save_instance_named("temp", &placeholder).unwrap();
        db.set_process_binding("pid-123", "", "temp").unwrap();

        let env = std::collections::HashMap::from([
            ("HCOM_PROCESS_ID".to_string(), "pid-123".to_string()),
            ("HCOM_LAUNCHED".to_string(), "1".to_string()),
            ("HCOM_TOOL".to_string(), "omp".to_string()),
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

    // ── Plugin install/remove safety ──────────────────────────────────

    /// Helper: run a closure with a temp HOME + HCOM_DIR (via isolated_test_env),
    /// Runs a test with isolated HCOM_DIR and HOME, Config reset,
    /// and PI_CODING_AGENT_DIR explicitly unset so the default ~/.omp path is used.
    fn with_isolated_omp_env(f: impl FnOnce(&std::path::Path)) {
        let (_dir, _hcom, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        unsafe {
            std::env::remove_var("PI_CODING_AGENT_DIR");
        }
        f(&home);
    }
    #[test]
    fn plugin_dir_respects_pi_coding_agent_dir() {
        let (_dir, _hcom, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let custom = home.join("custom-omp");
        unsafe {
            std::env::set_var("PI_CODING_AGENT_DIR", &custom);
        }

        let path = get_omp_plugin_path();
        assert_eq!(path, custom.join("extensions").join("hcom.ts"));
    }

    #[test]
    fn extension_inject_args_contains_absolute_plugin_path() {
        with_isolated_omp_env(|_| {
            let args = extension_inject_args();
            assert_eq!(args.len(), 2);
            assert_eq!(args[0], "-e");
            let path = std::path::Path::new(&args[1]);
            assert!(path.is_absolute());
            assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("hcom.ts"));
        });
    }

    #[test]
    fn plugin_source_uses_omp_cli_commands_only() {
        assert!(PLUGIN_SOURCE.contains("[\"omp-read\""));
        assert!(PLUGIN_SOURCE.contains("[\"omp-status\""));
        assert!(PLUGIN_SOURCE.contains("[\"omp-stop\""));
        assert!(!PLUGIN_SOURCE.contains("[\"pi-read\""));
        assert!(!PLUGIN_SOURCE.contains("[\"pi-status\""));
        assert!(!PLUGIN_SOURCE.contains("[\"pi-stop\""));
    }

    #[test]
    fn plugin_source_matches_omp_input_result_shape() {
        assert!(PLUGIN_SOURCE.contains("return {}"));
        assert!(PLUGIN_SOURCE.contains("return { text:"));
        assert!(PLUGIN_SOURCE.contains("return { handled: true }"));
        assert!(!PLUGIN_SOURCE.contains("action: \"continue\""));
        assert!(!PLUGIN_SOURCE.contains("action: \"transform\""));
        assert!(!PLUGIN_SOURCE.contains("action: \"handled\""));
        assert!(!PLUGIN_SOURCE.contains("streamingBehavior"));
    }

    #[test]
    fn plugin_source_handles_omp_session_switch_and_shutdown_shape() {
        assert!(PLUGIN_SOURCE.contains("pi.on(\"session_switch\""));
        assert!(PLUGIN_SOURCE.contains("\"--reason\", \"shutdown\""));
        assert!(!PLUGIN_SOURCE.contains("event.reason"));
    }

    #[test]
    fn install_writes_plugin_source() {
        with_isolated_omp_env(|_| {
            assert!(install_omp_plugin().unwrap());
            let content = std::fs::read_to_string(get_omp_plugin_path()).unwrap();
            assert_eq!(content, PLUGIN_SOURCE);
            assert!(verify_omp_plugin_installed());
        });
    }

    #[test]
    fn install_refuses_to_overwrite_non_hcom_file() {
        with_isolated_omp_env(|_| {
            let path = get_omp_plugin_path();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "// user's custom plugin").unwrap();

            let result = install_omp_plugin();
            assert!(result.is_err());
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "// user's custom plugin",
            );
            assert!(!verify_omp_plugin_installed());
        });
    }

    #[test]
    fn install_upgrades_stale_hcom_owned_plugin() {
        with_isolated_omp_env(|_| {
            let path = get_omp_plugin_path();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            // Old hcom plugin: has the ownership marker but doesn't match current source.
            std::fs::write(&path, r#"const x = customType: "hcom-bootstrap";"#).unwrap();

            assert!(install_omp_plugin().unwrap());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), PLUGIN_SOURCE,);
            assert!(verify_omp_plugin_installed());
        });
    }

    #[test]
    fn remove_deletes_hcom_plugin() {
        with_isolated_omp_env(|_| {
            install_omp_plugin().unwrap();
            let path = get_omp_plugin_path();
            assert!(path.exists());

            remove_omp_plugin().unwrap();
            assert!(!path.exists());
        });
    }

    #[test]
    fn remove_preserves_non_hcom_file() {
        with_isolated_omp_env(|_| {
            let path = get_omp_plugin_path();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "// user's custom plugin").unwrap();

            remove_omp_plugin().unwrap();
            assert!(path.exists(), "non-hcom file must not be removed");
        });
    }
}
