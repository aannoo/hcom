use super::{
    HOOK_TIMEOUT_SECS, KIMI_HOOK_COMMANDS, build_kimi_hook_command, get_handler,
    get_kimi_settings_path, handle_sessionend, handle_sessionstart, handle_stop,
    is_hcom_kimi_command, kimi_permission_patterns, merge_hcom_hooks, merge_hcom_permissions,
    remove_hcom_permissions,
};
use crate::db::HcomDb;
use crate::hooks::HookResult;
use crate::hooks::common::commit_delivery_ack;
use crate::hooks::test_helpers::isolated_test_env;
use crate::shared::{ST_ACTIVE, ST_LISTENING};
use serial_test::serial;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table};

#[test]
#[serial]
fn config_path_is_project_local_unless_explicitly_overridden() {
    let (_dir, hcom_dir, _home, _guard) = isolated_test_env();
    unsafe {
        std::env::remove_var("KIMI_CODE_HOME");
    }
    assert_eq!(
        get_kimi_settings_path(),
        hcom_dir
            .parent()
            .unwrap()
            .join(".kimi-code")
            .join("config.toml")
    );

    let explicit = hcom_dir.join("explicit-kimi");
    unsafe {
        std::env::set_var("KIMI_CODE_HOME", &explicit);
    }
    assert_eq!(get_kimi_settings_path(), explicit.join("config.toml"));
}

fn rules(doc: &DocumentMut) -> &ArrayOfTables {
    match doc.get("permission").and_then(|p| p.get("rules")) {
        Some(Item::ArrayOfTables(arr)) => arr,
        _ => panic!("expected [[permission.rules]]"),
    }
}

#[test]
fn is_hcom_kimi_command_matches_canonical_commands() {
    // Each installed hook command must be recognized as hcom-managed, else
    // re-setup duplicates them instead of replacing them.
    for (_, suffix) in KIMI_HOOK_COMMANDS {
        let cmd = build_kimi_hook_command(suffix);
        assert!(
            is_hcom_kimi_command(&cmd),
            "should recognize installed hcom hook command: {cmd}"
        );
        assert!(
            is_hcom_kimi_command(&format!("hcom {suffix}")),
            "should recognize bare hcom {suffix}"
        );
        assert!(
            is_hcom_kimi_command(&format!("uvx hcom {suffix}")),
            "should recognize uvx hcom {suffix}"
        );
    }
    assert!(!is_hcom_kimi_command("echo hello"));
    assert!(!is_hcom_kimi_command("hcom send @x -- hi"));
}

#[test]
fn merge_hooks_strips_stale_uvx_prefix_rows() {
    let mut doc = DocumentMut::new();
    // Simulate a prior uvx-based install left in config.toml.
    {
        let hooks = doc
            .entry("hooks")
            .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
        let Item::ArrayOfTables(arr) = hooks else {
            panic!("hooks");
        };
        for (event, suffix) in KIMI_HOOK_COMMANDS {
            let mut table = Table::new();
            table.insert("event", toml_edit::value(*event));
            table.insert("command", toml_edit::value(format!("uvx hcom {suffix}")));
            table.insert("timeout", toml_edit::value(HOOK_TIMEOUT_SECS));
            arr.push(table);
        }
    }
    merge_hcom_hooks(&mut doc);
    let Item::ArrayOfTables(arr) = doc.get("hooks").unwrap() else {
        panic!("expected [[hooks]]");
    };
    assert_eq!(arr.len(), KIMI_HOOK_COMMANDS.len());
    for i in 0..arr.len() {
        let cmd = arr
            .get(i)
            .unwrap()
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(
            !cmd.starts_with("uvx "),
            "stale uvx hook must be stripped: {cmd}"
        );
        assert!(
            is_hcom_kimi_command(cmd),
            "replacement must be current-prefix hcom command: {cmd}"
        );
    }
}

#[test]
fn permission_events_are_registered_and_dispatchable() {
    // Both observation-only permission events must be (a) installed into
    // config.toml via KIMI_HOOK_COMMANDS and (b) routable to a handler,
    // else the blocked-on-approval status transition never fires.
    for (event, suffix) in [
        ("PermissionRequest", "kimi-permissionrequest"),
        ("PermissionResult", "kimi-permissionresult"),
    ] {
        assert!(
            KIMI_HOOK_COMMANDS.contains(&(event, suffix)),
            "{event}/{suffix} must be in KIMI_HOOK_COMMANDS"
        );
        assert!(
            get_handler(suffix).is_some(),
            "{suffix} must resolve to a handler"
        );
    }
    // The spec's routing list (Tool::from_hook_name) must agree, or the
    // installed hook command would never reach dispatch_kimi_hook.
    let spec_names = crate::tool::Tool::Kimi.hooks();
    assert!(spec_names.contains(&"kimi-permissionrequest"));
    assert!(spec_names.contains(&"kimi-permissionresult"));
}

#[test]
fn merge_hooks_is_idempotent() {
    let mut doc = DocumentMut::new();
    merge_hcom_hooks(&mut doc);
    let first = match doc.get("hooks") {
        Some(Item::ArrayOfTables(arr)) => arr.len(),
        _ => panic!("expected [[hooks]]"),
    };
    merge_hcom_hooks(&mut doc);
    let second = match doc.get("hooks") {
        Some(Item::ArrayOfTables(arr)) => arr.len(),
        _ => panic!("expected [[hooks]]"),
    };
    assert_eq!(first, KIMI_HOOK_COMMANDS.len());
    assert_eq!(
        first, second,
        "re-merging hooks must not duplicate existing hcom hooks"
    );
}

#[test]
fn merge_prepends_allow_rules_for_all_safe_commands() {
    let mut doc = DocumentMut::new();
    merge_hcom_permissions(&mut doc);

    let arr = rules(&doc);
    let expected = kimi_permission_patterns();
    // Our allow-rules come first, one per safe command, all decision=allow.
    assert!(arr.len() >= expected.len());
    for (i, pat) in expected.iter().enumerate() {
        let table = arr.get(i).expect("rule present");
        assert_eq!(
            table.get("decision").and_then(|v| v.as_str()),
            Some("allow")
        );
        assert_eq!(
            table.get("pattern").and_then(|v| v.as_str()),
            Some(pat.as_str())
        );
    }
    assert!(verify_permissions_at_doc(&doc));
}

#[test]
fn merge_is_idempotent_and_keeps_user_rules_after_ours() {
    let mut doc: DocumentMut = r#"
[[permission.rules]]
decision = "ask"
pattern = "Bash"
"#
    .parse()
    .unwrap();

    merge_hcom_permissions(&mut doc);
    let after_first = rules(&doc).len();
    merge_hcom_permissions(&mut doc);
    let after_second = rules(&doc).len();
    assert_eq!(
        after_first, after_second,
        "re-merging must not duplicate managed rules"
    );

    // The user's broad `ask Bash` rule survives and sits AFTER our allows,
    // so first-match-wins still auto-approves hcom commands.
    let arr = rules(&doc);
    let last = arr.get(arr.len() - 1).unwrap();
    assert_eq!(last.get("decision").and_then(|v| v.as_str()), Some("ask"));
    assert_eq!(last.get("pattern").and_then(|v| v.as_str()), Some("Bash"));
    assert!(verify_permissions_at_doc(&doc));
}

#[test]
fn remove_strips_only_managed_rules() {
    let mut doc: DocumentMut = r#"
[[permission.rules]]
decision = "deny"
pattern = "Bash(rm -rf*)"
"#
    .parse()
    .unwrap();

    merge_hcom_permissions(&mut doc);
    remove_hcom_permissions(&mut doc);

    // The user's deny rule remains; managed allows are gone.
    let arr = rules(&doc);
    assert_eq!(arr.len(), 1);
    let only = arr.get(0).unwrap();
    assert_eq!(only.get("decision").and_then(|v| v.as_str()), Some("deny"));
    assert!(!verify_permissions_at_doc(&doc));
}

#[test]
fn remove_drops_empty_permission_table() {
    let mut doc = DocumentMut::new();
    merge_hcom_permissions(&mut doc);
    remove_hcom_permissions(&mut doc);
    assert!(
        doc.get("permission").is_none(),
        "permission table should be removed when no rules remain"
    );
}

// Mirror of verify_permissions_at but against an in-memory document so the
// tests never touch the real ~/.kimi-code/config.toml.
fn verify_permissions_at_doc(doc: &DocumentMut) -> bool {
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

fn make_test_db() -> (tempfile::TempDir, HcomDb) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = HcomDb::open_raw(&db_path).unwrap();
    db.init_db().unwrap();
    (dir, db)
}

fn ctx_with_process(process_id: &str) -> crate::shared::context::HcomContext {
    let mut env = std::collections::HashMap::new();
    env.insert("HCOM_PROCESS_ID".into(), process_id.into());
    env.insert("HCOM_TOOL".into(), "kimi".into());
    crate::shared::context::HcomContext::from_env(&env, std::env::current_dir().unwrap())
}

fn kimi_payload(session_id: &str, hook_name: &str) -> crate::hooks::HookPayload {
    crate::hooks::HookPayload {
        session_id: Some(session_id.into()),
        transcript_path: None,
        hook_name: hook_name.into(),
        tool: "kimi".into(),
        tool_name: String::new(),
        tool_input: serde_json::Value::Null,
        tool_result: String::new(),
        notification_type: None,
        raw: serde_json::Value::Null,
    }
}

fn seed_bound_kimi_instance(db: &HcomDb, name: &str, session_id: &str, process_id: &str) {
    let now = chrono::Utc::now().timestamp() as f64;
    db.conn()
        .execute(
            "INSERT INTO instances (name, status, status_context, created_at, tool, session_id, last_event_id)
                 VALUES (?1, 'active', 'docs/x.md', ?2, 'kimi', ?3, 0)",
            rusqlite::params![name, now, session_id],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO process_bindings (process_id, session_id, instance_name, updated_at)
                 VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![process_id, session_id, name, now],
        )
        .unwrap();
    db.rebind_session(session_id, name).unwrap();
}

fn insert_unread_message(db: &HcomDb, from: &str, text: &str) -> i64 {
    let data = serde_json::json!({
        "from": from,
        "text": text,
        "scope": "broadcast",
    })
    .to_string();
    db.conn()
        .execute(
            "INSERT INTO events (type, timestamp, instance, data) VALUES ('message', '2026-01-01T00:00:01Z', ?1, ?2)",
            rusqlite::params![from, data],
        )
        .unwrap();
    db.conn().last_insert_rowid()
}

fn instance_last_event_id(db: &HcomDb, name: &str) -> i64 {
    db.conn()
        .query_row(
            "SELECT last_event_id FROM instances WHERE name = ?1",
            [name],
            |row| row.get(0),
        )
        .unwrap()
}

fn stopped_event_count(db: &HcomDb, name: &str) -> i64 {
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM events
             WHERE type = 'life' AND instance = ?1
               AND json_extract(data, '$.action') = 'stopped'",
            [name],
            |row| row.get(0),
        )
        .unwrap()
}

fn assert_allow_no_delivery(result: HookResult) {
    match result {
        HookResult::Allow {
            additional_context,
            delivery_ack,
            ..
        } => {
            assert!(additional_context.is_none());
            assert!(delivery_ack.is_none());
        }
        _ => panic!("expected Allow with no delivery"),
    }
}

#[test]
fn posttooluse_with_pending_does_not_ack_or_advance_cursor() {
    let (_dir, db) = make_test_db();
    seed_bound_kimi_instance(&db, "kima", "sess-post", "pid-post");
    let message_id = insert_unread_message(&db, "homo", "secret body");

    let handler = get_handler("kimi-posttooluse").expect("kimi-posttooluse handler");
    let result = handler(
        &db,
        &ctx_with_process("pid-post"),
        &kimi_payload("sess-post", "kimi-posttooluse"),
    );
    assert_allow_no_delivery(result);

    assert_eq!(instance_last_event_id(&db, "kima"), 0);
    let unread = db.get_unread_messages("kima");
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].event_id, Some(message_id));
}

#[test]
fn notification_with_pending_does_not_ack_or_advance_cursor() {
    let (_dir, db) = make_test_db();
    seed_bound_kimi_instance(&db, "kima", "sess-notify", "pid-notify");
    let message_id = insert_unread_message(&db, "homo", "secret body");

    let handler = get_handler("kimi-notification").expect("kimi-notification handler");
    let result = handler(
        &db,
        &ctx_with_process("pid-notify"),
        &kimi_payload("sess-notify", "kimi-notification"),
    );
    assert_allow_no_delivery(result);

    assert_eq!(instance_last_event_id(&db, "kima"), 0);
    let unread = db.get_unread_messages("kima");
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].event_id, Some(message_id));
}

#[test]
fn posttooluse_repeated_while_pending_leaves_cursor_unchanged() {
    let (_dir, db) = make_test_db();
    seed_bound_kimi_instance(&db, "kima", "sess-repeat", "pid-repeat");
    insert_unread_message(&db, "homo", "secret body");

    let handler = get_handler("kimi-posttooluse").expect("kimi-posttooluse handler");
    let ctx = ctx_with_process("pid-repeat");
    let payload = kimi_payload("sess-repeat", "kimi-posttooluse");
    for _ in 0..3 {
        assert_allow_no_delivery(handler(&db, &ctx, &payload));
    }

    assert_eq!(instance_last_event_id(&db, "kima"), 0);
    assert_eq!(db.get_unread_messages("kima").len(), 1);
}

#[test]
fn userpromptsubmit_with_pending_delivers_and_acks() {
    let (_dir, db) = make_test_db();
    seed_bound_kimi_instance(&db, "kima", "sess-prompt", "pid-prompt");
    let message_id = insert_unread_message(&db, "homo", "deliver me");

    let handler = get_handler("kimi-userpromptsubmit").expect("kimi-userpromptsubmit handler");
    let result = handler(
        &db,
        &ctx_with_process("pid-prompt"),
        &kimi_payload("sess-prompt", "kimi-userpromptsubmit"),
    );

    let ack = match result {
        HookResult::Allow {
            additional_context,
            delivery_ack,
            ..
        } => {
            let ctx = additional_context.expect("expected formatted delivery context");
            assert!(ctx.contains("deliver me"));
            delivery_ack.expect("expected deferred delivery ack")
        }
        _ => panic!("expected Allow with delivery"),
    };

    assert_eq!(instance_last_event_id(&db, "kima"), 0);

    // Simulates dispatch writing stdout then committing the deferred ack.
    commit_delivery_ack(&db, &ack);

    assert_eq!(instance_last_event_id(&db, "kima"), message_id);
    let inst = db.get_instance_full("kima").unwrap().unwrap();
    assert_eq!(inst.status, ST_ACTIVE);
    assert!(inst.status_context.starts_with("deliver:"));
}

#[test]
fn stop_with_pending_defers_ack_stays_active() {
    let (_dir, db) = make_test_db();
    seed_bound_kimi_instance(&db, "gire", "sess-stop-pending", "pid-stop-pending");
    let message_id = insert_unread_message(&db, "homo", "stop deliver");

    let result = handle_stop(
        &db,
        &ctx_with_process("pid-stop-pending"),
        &kimi_payload("sess-stop-pending", "kimi-stop"),
    );

    let ack = match result {
        HookResult::Block {
            reason,
            delivery_ack,
        } => {
            assert!(reason.contains("stop deliver"));
            delivery_ack.expect("expected deferred delivery ack on pending stop")
        }
        _ => panic!("expected Block with deferred delivery"),
    };

    assert_eq!(instance_last_event_id(&db, "gire"), 0);
    let inst = db.get_instance_full("gire").unwrap().unwrap();
    assert_eq!(inst.status, ST_ACTIVE);
    assert_ne!(inst.status, ST_LISTENING);

    // Simulates dispatch writing block reason stdout then committing the ack.
    commit_delivery_ack(&db, &ack);

    assert_eq!(instance_last_event_id(&db, "gire"), message_id);
    let inst = db.get_instance_full("gire").unwrap().unwrap();
    assert_eq!(inst.status, ST_ACTIVE);
    assert_ne!(inst.status, ST_LISTENING);
}

#[test]
fn stop_without_pending_sets_listening() {
    let (_dir, db) = make_test_db();
    let now = chrono::Utc::now().timestamp() as f64;
    db.conn()
        .execute(
            "INSERT INTO instances (name, status, status_context, created_at, tool, session_id)
                 VALUES ('gire', 'active', 'docs/x.md', ?1, 'kimi', 'sess-stop')",
            rusqlite::params![now],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO process_bindings (process_id, session_id, instance_name, updated_at)
                 VALUES ('pid-stop', 'sess-stop', 'gire', ?1)",
            rusqlite::params![now],
        )
        .unwrap();
    db.rebind_session("sess-stop", "gire").unwrap();

    let result = handle_stop(
        &db,
        &ctx_with_process("pid-stop"),
        &kimi_payload("sess-stop", "kimi-stop"),
    );
    assert_eq!(result.exit_code(), 0);
    let inst = db.get_instance_full("gire").unwrap().unwrap();
    assert_eq!(inst.status, ST_LISTENING);
}

#[test]
fn sessionstart_rejects_foreign_process_owner_without_mutation() {
    let (_dir, db) = make_test_db();
    let now = chrono::Utc::now().timestamp() as f64;
    db.conn()
        .execute(
            "INSERT INTO instances (name, status, status_context, created_at, tool, session_id)
             VALUES ('movi', 'active', 'foreign-work', ?1, 'omp', 'sess-old')",
            rusqlite::params![now],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO process_bindings (process_id, session_id, instance_name, updated_at)
             VALUES ('pid-foreign', 'sess-old', 'movi', ?1)",
            rusqlite::params![now],
        )
        .unwrap();
    db.rebind_session("sess-old", "movi").unwrap();

    assert_allow_no_delivery(handle_sessionstart(
        &db,
        &ctx_with_process("pid-foreign"),
        &kimi_payload("sess-new", "kimi-sessionstart"),
    ));

    let owner = db.get_instance_full("movi").unwrap().unwrap();
    assert_eq!(owner.tool, "omp");
    assert_eq!(owner.session_id.as_deref(), Some("sess-old"));
    assert_eq!(owner.status, ST_ACTIVE);
    assert_eq!(owner.status_context, "foreign-work");
    assert_eq!(db.get_session_binding("sess-new").unwrap(), None);
    assert_eq!(
        db.get_process_binding("pid-foreign").unwrap().as_deref(),
        Some("movi")
    );
}

#[test]
fn sessionstart_rejects_foreign_canonical_owner_without_mutation() {
    let (_dir, db) = make_test_db();
    let now = chrono::Utc::now().timestamp() as f64;
    db.conn()
        .execute(
            "INSERT INTO instances (name, status, status_context, created_at, tool)
             VALUES ('kima', 'listening', 'ready_observed', ?1, 'kimi')",
            rusqlite::params![now],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO process_bindings (process_id, session_id, instance_name, updated_at)
             VALUES ('pid-kimi', '', 'kima', ?1)",
            rusqlite::params![now],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO instances (name, status, status_context, created_at, tool, session_id)
             VALUES ('movi', 'inactive', 'exit:closed', ?1, 'omp', 'sess-foreign')",
            rusqlite::params![now],
        )
        .unwrap();
    db.rebind_session("sess-foreign", "movi").unwrap();

    assert_allow_no_delivery(handle_sessionstart(
        &db,
        &ctx_with_process("pid-kimi"),
        &kimi_payload("sess-foreign", "kimi-sessionstart"),
    ));

    let placeholder = db.get_instance_full("kima").unwrap().unwrap();
    assert_eq!(placeholder.session_id, None);
    assert_eq!(placeholder.status, ST_LISTENING);
    let canonical = db.get_instance_full("movi").unwrap().unwrap();
    assert_eq!(canonical.tool, "omp");
    assert_eq!(canonical.session_id.as_deref(), Some("sess-foreign"));
    assert_eq!(
        db.get_session_binding("sess-foreign").unwrap().as_deref(),
        Some("movi")
    );
    assert_eq!(
        db.get_process_binding("pid-kimi").unwrap().as_deref(),
        Some("kima")
    );
}

#[test]
fn sessionstart_rejects_foreign_stopped_owner_without_mutation() {
    let (_dir, db) = make_test_db();
    let now = chrono::Utc::now().timestamp() as f64;
    db.conn()
        .execute(
            "INSERT INTO instances (name, status, status_context, created_at, tool)
             VALUES ('kima', 'listening', 'ready_observed', ?1, 'kimi')",
            rusqlite::params![now],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO process_bindings (process_id, session_id, instance_name, updated_at)
             VALUES ('pid-kimi', '', 'kima', ?1)",
            rusqlite::params![now],
        )
        .unwrap();
    let stopped = serde_json::json!({
        "action": "stopped",
        "by": "session",
        "reason": "exit:closed",
        "snapshot": {
            "name": "movi",
            "session_id": "sess-stopped-foreign",
            "tool": "omp"
        }
    });
    db.log_event("life", "movi", &stopped).unwrap();

    assert_allow_no_delivery(handle_sessionstart(
        &db,
        &ctx_with_process("pid-kimi"),
        &kimi_payload("sess-stopped-foreign", "kimi-sessionstart"),
    ));

    let placeholder = db.get_instance_full("kima").unwrap().unwrap();
    assert_eq!(placeholder.session_id, None);
    assert_eq!(placeholder.status, ST_LISTENING);
    assert_eq!(
        db.get_session_binding("sess-stopped-foreign").unwrap(),
        None
    );
    assert_eq!(
        db.get_process_binding("pid-kimi").unwrap().as_deref(),
        Some("kima")
    );
    assert!(db.get_instance_full("movi").unwrap().is_none());
}

#[test]
fn sessionstart_accepts_kimi_canonical_owner_and_migrates_placeholder() {
    let (_dir, db) = make_test_db();
    let now = chrono::Utc::now().timestamp() as f64;
    db.conn()
        .execute(
            "INSERT INTO instances (name, status, status_context, created_at, tool)
             VALUES ('temp', 'listening', 'ready_observed', ?1, 'kimi')",
            rusqlite::params![now],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO process_bindings (process_id, session_id, instance_name, updated_at)
             VALUES ('pid-kimi', '', 'temp', ?1)",
            rusqlite::params![now],
        )
        .unwrap();
    db.upsert_notify_endpoint("temp", "pty", 51_234).unwrap();
    seed_bound_kimi_instance(&db, "kima", "sess-kimi", "pid-old");

    assert_allow_no_delivery(handle_sessionstart(
        &db,
        &ctx_with_process("pid-kimi"),
        &kimi_payload("sess-kimi", "kimi-sessionstart"),
    ));

    assert!(db.get_instance_full("temp").unwrap().is_none());
    let canonical = db.get_instance_full("kima").unwrap().unwrap();
    assert_eq!(canonical.tool, "kimi");
    assert_eq!(canonical.session_id.as_deref(), Some("sess-kimi"));
    assert_eq!(canonical.status, ST_LISTENING);
    assert_eq!(
        db.get_process_binding("pid-kimi").unwrap().as_deref(),
        Some("kima")
    );
    assert!(db.has_notify_endpoint_kind("kima", "pty"));
}

#[test]
fn sessionend_ignores_non_kimi_tool() {
    let (_dir, db) = make_test_db();
    let now = chrono::Utc::now().timestamp() as f64;
    db.conn()
        .execute(
            "INSERT INTO instances (name, status, created_at, tool, session_id)
                 VALUES ('movi', 'listening', ?1, 'omp', 'sess-omp')",
            rusqlite::params![now],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO process_bindings (process_id, session_id, instance_name, updated_at)
                 VALUES ('pid-omp', 'sess-omp', 'movi', ?1)",
            rusqlite::params![now],
        )
        .unwrap();
    db.rebind_session("sess-omp", "movi").unwrap();

    let result = handle_sessionend(
        &db,
        &ctx_with_process("pid-omp"),
        &kimi_payload("sess-omp", "kimi-sessionend"),
    );
    assert_eq!(result.exit_code(), 0);
    let inst = db.get_instance_full("movi").unwrap().unwrap();
    assert_eq!(inst.status, "listening");
    assert_eq!(inst.tool, "omp");
    assert_eq!(
        db.get_process_binding("pid-omp").unwrap().as_deref(),
        Some("movi")
    );
}

#[test]
fn sessionend_finalizes_once_even_while_pid_is_alive() {
    let (_dir, db) = make_test_db();
    let now = chrono::Utc::now().timestamp() as f64;
    let pid = std::process::id() as i64;
    db.conn()
        .execute(
            "INSERT INTO instances (name, status, created_at, tool, session_id, pid)
                 VALUES ('kima', 'active', ?1, 'kimi', 'sess-soft', ?2)",
            rusqlite::params![now, pid],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO process_bindings (process_id, session_id, instance_name, updated_at)
                 VALUES ('pid-soft', 'sess-soft', 'kima', ?1)",
            rusqlite::params![now],
        )
        .unwrap();
    db.rebind_session("sess-soft", "kima").unwrap();

    let result = handle_sessionend(
        &db,
        &ctx_with_process("pid-soft"),
        &kimi_payload("sess-soft", "kimi-sessionend"),
    );
    assert_eq!(result.exit_code(), 0);
    assert!(db.get_instance_full("kima").unwrap().is_none());
    assert_eq!(db.get_process_binding("pid-soft").unwrap(), None);
    assert_eq!(db.get_session_binding("sess-soft").unwrap(), None);
    assert_eq!(stopped_event_count(&db, "kima"), 1);
}
