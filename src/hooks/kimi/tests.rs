use super::{
    HOOK_TIMEOUT_SECS, KIMI_HOOK_COMMANDS, build_kimi_hook_command, get_handler,
    get_kimi_settings_path, handle_sessionend, handle_stop, is_hcom_kimi_command,
    kimi_permission_patterns, merge_hcom_hooks, merge_hcom_permissions, remove_hcom_permissions,
};
use crate::db::HcomDb;
use crate::hooks::test_helpers::isolated_test_env;
use crate::shared::ST_LISTENING;
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
fn sessionend_soft_keeps_row_when_pid_alive() {
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
    let inst = db.get_instance_full("kima").unwrap().unwrap();
    assert_eq!(inst.status, crate::shared::ST_INACTIVE);
    assert_eq!(
        db.get_process_binding("pid-soft").unwrap().as_deref(),
        Some("kima"),
        "soft finalize must keep process binding"
    );
}
