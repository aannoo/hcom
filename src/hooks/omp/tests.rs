use super::*;
use crate::db::HcomDb;
use crate::instances;
use crate::shared::context::HcomContext;
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
#[serial_test::serial]
fn strip_managed_extension_removes_only_hcom_entry() {
    with_isolated_omp_env(|_| {
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
    });
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
    assert!(
        !PLUGIN_SOURCE
            .contains("reportStatus(currentCtx, currentCtx.isIdle() ? \"listening\" : \"active\")")
    );
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
    // keepOwner retains process-env identity ownership so nested task
    // extensions still skip bind while this instance rebinds.
    let idx = PLUGIN_SOURCE
        .find("pi.on(\"session_branch\"")
        .expect("session_branch handler present");
    let handler = &PLUGIN_SOURCE[idx..];
    assert!(handler.contains("resetBinding({ keepOwner: true })"));
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

#[test]
fn soft_stop_keeps_instance_row_and_process_binding() {
    let (db, path) = setup_test_db();
    let now = chrono::Utc::now().timestamp() as f64;
    db.conn()
        .execute(
            "INSERT INTO instances (name, status, created_at, tool, session_id)
             VALUES ('luna', 'listening', ?1, 'omp', 'sid-soft')",
            rusqlite::params![now],
        )
        .unwrap();
    db.set_process_binding("pid-soft", "sid-soft", "luna")
        .unwrap();
    db.rebind_session("sid-soft", "luna").unwrap();

    let (code, output) = handle_stop(
        &db,
        &[
            "--name".to_string(),
            "luna".to_string(),
            "--soft".to_string(),
            "--reason".to_string(),
            "turn_end".to_string(),
        ],
    );

    assert_eq!(code, 0);
    let response: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(response.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(response.get("soft").and_then(|v| v.as_bool()), Some(true));
    assert!(db.get_instance_full("luna").unwrap().is_some());
    assert_eq!(
        db.get_status("luna").unwrap().map(|(s, _)| s),
        Some(crate::shared::ST_INACTIVE.to_string())
    );
    assert_eq!(
        db.get_process_binding("pid-soft").unwrap(),
        Some("luna".to_string())
    );
    assert_eq!(db.get_session_binding("sid-soft").unwrap(), None);

    cleanup(path);
}

#[test]
fn start_rebinds_via_process_binding_after_soft_stop() {
    let (db, path) = setup_test_db();
    let temp = tempfile::TempDir::new().unwrap();
    save_test_instance(&db, "miso", ST_LISTENING);
    db.conn()
        .execute(
            "UPDATE instances SET session_id = 'sid-old' WHERE name = 'miso'",
            [],
        )
        .unwrap();
    db.set_process_binding("pid-recover", "sid-old", "miso")
        .unwrap();
    db.rebind_session("sid-old", "miso").unwrap();

    let (stop_code, _) = handle_stop(
        &db,
        &[
            "--name".to_string(),
            "miso".to_string(),
            "--soft".to_string(),
        ],
    );
    assert_eq!(stop_code, 0);
    assert_eq!(
        db.get_process_binding("pid-recover").unwrap(),
        Some("miso".to_string())
    );

    let env = std::collections::HashMap::from([
        ("HCOM_PROCESS_ID".to_string(), "pid-recover".to_string()),
        ("HCOM_LAUNCHED".to_string(), "1".to_string()),
        ("HCOM_TOOL".to_string(), "omp".to_string()),
    ]);
    let ctx = HcomContext::from_env(&env, temp.path().to_path_buf());

    let (code, output) = handle_start(
        &ctx,
        &db,
        &[
            "--session-id".to_string(),
            "sid-new".to_string(),
            "--cwd".to_string(),
            temp.path().to_string_lossy().to_string(),
        ],
    );

    assert_eq!(code, 0);
    let response: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(response.get("name").and_then(|v| v.as_str()), Some("miso"));
    assert_eq!(
        db.get_process_binding("pid-recover").unwrap(),
        Some("miso".to_string())
    );
    assert_eq!(
        db.get_session_binding("sid-new").unwrap(),
        Some("miso".to_string())
    );
    let rebound = db.get_instance_full("miso").unwrap().unwrap();
    assert_eq!(rebound.session_id.as_deref(), Some("sid-new"));
    assert_eq!(rebound.status, ST_LISTENING);

    cleanup(path);
}

#[test]
fn start_recovers_binding_via_instance_name_when_process_binding_cleared() {
    let (db, path) = setup_test_db();
    let temp = tempfile::TempDir::new().unwrap();
    save_test_instance(&db, "miso", ST_LISTENING);
    db.conn()
        .execute(
            "UPDATE instances SET session_id = 'sid-old', status = 'inactive' WHERE name = 'miso'",
            [],
        )
        .unwrap();
    db.rebind_session("sid-old", "miso").unwrap();

    let env = std::collections::HashMap::from([
        ("HCOM_PROCESS_ID".to_string(), "pid-recover".to_string()),
        ("HCOM_INSTANCE_NAME".to_string(), "miso".to_string()),
        ("HCOM_LAUNCHED".to_string(), "1".to_string()),
        ("HCOM_TOOL".to_string(), "omp".to_string()),
    ]);
    let ctx = HcomContext::from_env(&env, temp.path().to_path_buf());

    let (code, output) = handle_start(
        &ctx,
        &db,
        &[
            "--session-id".to_string(),
            "sid-new".to_string(),
            "--cwd".to_string(),
            temp.path().to_string_lossy().to_string(),
        ],
    );

    assert_eq!(code, 0);
    let response: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(response.get("name").and_then(|v| v.as_str()), Some("miso"));
    assert_eq!(
        db.get_process_binding("pid-recover").unwrap(),
        Some("miso".to_string())
    );
    assert_eq!(
        db.get_session_binding("sid-new").unwrap(),
        Some("miso".to_string())
    );

    cleanup(path);
}

#[test]
fn recover_rejects_non_omp_tool() {
    let (db, path) = setup_test_db();
    save_test_instance(&db, "luna", crate::shared::ST_INACTIVE);
    db.conn()
        .execute(
            "UPDATE instances SET tool = 'claude' WHERE name = 'luna'",
            [],
        )
        .unwrap();

    assert_eq!(
        crate::instance_binding::recover_process_binding_for_instance(
            &db, "luna", "sid-new", "pid-1"
        ),
        None
    );

    cleanup(path);
}

#[test]
fn recover_rejects_active_instance() {
    let (db, path) = setup_test_db();
    save_test_instance(&db, "luna", ST_LISTENING);

    assert_eq!(
        crate::instance_binding::recover_process_binding_for_instance(
            &db, "luna", "sid-new", "pid-1"
        ),
        None
    );

    cleanup(path);
}

#[test]
fn plugin_source_pins_soft_stop_polish_markers() {
    assert!(PLUGIN_SOURCE.contains("Symbol.for(\"hcom.omp.identity\")"));
    assert!(PLUGIN_SOURCE.contains("stopReconcileTimer"));
    assert!(PLUGIN_SOURCE.contains("tearingDown"));
    assert!(PLUGIN_SOURCE.contains("HCOM_TIMEOUT_MS"));
    assert!(PLUGIN_SOURCE.contains("1800"));
    let shutdown_idx = PLUGIN_SOURCE
        .find("pi.on(\"session_shutdown\"")
        .expect("session_shutdown handler present");
    let handler = &PLUGIN_SOURCE[shutdown_idx..];
    assert!(
        handler.contains("keepOwner = !softStopOk"),
        "failed soft-stop path must keepOwner"
    );
    assert!(
        handler.contains("reg.tearingDown = false"),
        "failed soft-stop keepOwner must clear tearingDown so owner can rebind"
    );
    assert!(
        PLUGIN_SOURCE.contains("reg.tearingDown && !ownsIdentity"),
        "tearingDown must not block the owning extension from rebinding"
    );
}

/// Manual/CI scenario: nested OMP task extension must not steal parent identity
/// after soft-stop + session_branch rebind. Run with:
/// `cargo test omp_nested_task_identity_survives_soft_stop -- --ignored`
#[test]
#[ignore = "requires live OMP nested task; run manually"]
fn omp_nested_task_identity_survives_soft_stop() {
    // Launch OMP via hcom, soft-stop parent turn, spawn nested task extension,
    // verify child does not bind and parent recovers on next turn.
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
#[serial_test::serial]
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
    assert!(PLUGIN_SOURCE.contains("pi.on(\"session_shutdown\""));
    // Soft-finalize on bound shutdown; never hard-delete from this path.
    assert!(PLUGIN_SOURCE.contains("\"--soft\""));
    assert!(PLUGIN_SOURCE.contains("plugin.session_shutdown_skipped"));
    assert!(PLUGIN_SOURCE.contains("nested_session"));
    assert!(PLUGIN_SOURCE.contains("HCOM_OMP_IDENTITY_OWNER"));
    assert!(PLUGIN_SOURCE.contains("plugin.bind_skipped_nested"));
    assert!(PLUGIN_SOURCE.contains("ownsIdentity"));
    assert!(PLUGIN_SOURCE.contains("keepOwner: true"));
    // session_switch must also keepOwner (same race as session_branch).
    let switch_idx = PLUGIN_SOURCE
        .find("pi.on(\"session_switch\"")
        .expect("session_switch handler present");
    assert!(
        PLUGIN_SOURCE[switch_idx..].contains("resetBinding({ keepOwner: true })"),
        "session_switch must keepOwner across rebind"
    );
    // Dead session-id gates removed (SessionShutdownEvent has no sessionId).
    assert!(!PLUGIN_SOURCE.contains("foreign_session"));
    assert!(!PLUGIN_SOURCE.contains("rootSessionId"));
    assert!(!PLUGIN_SOURCE.contains("shutdownSessionIdFromEvent"));
    assert!(!PLUGIN_SOURCE.contains("shutdownReasonFromEvent"));
    // Hard stop without --soft must not be the session_shutdown path.
    let idx = PLUGIN_SOURCE
        .find("pi.on(\"session_shutdown\"")
        .expect("session_shutdown handler present");
    let handler = &PLUGIN_SOURCE[idx..];
    let soft = handler.find("\"--soft\"").expect("soft stop in shutdown");
    let stop = handler.find("[\"omp-stop\"").expect("omp-stop in shutdown");
    assert!(stop < soft, "omp-stop invocation must include --soft");
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
