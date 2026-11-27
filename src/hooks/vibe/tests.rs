//! Vibe hook tests: `hooks.toml` install and the handlers.

use serde_json::{Value, json};
use serial_test::serial;
use toml_edit::DocumentMut;

use super::config::{
    VIBE_HOOK_COMMANDS, build_vibe_hook_command, merge_hcom_hooks, remove_hcom_hooks,
    verify_hooks_at,
};
use super::{
    handle_notification, handle_posttool, handle_preagent, handle_sessionend, is_hcom_main_agent,
};
use crate::db::HcomDb;
use crate::hooks::HookPayload;
use crate::hooks::common::commit_delivery_ack;
use crate::shared::context::HcomContext;
use crate::shared::{ST_ACTIVE, ST_BLOCKED, ST_LISTENING};

const USER_ROWS: &str = r#"
[[hooks]]
name = "my-lint"
type = "post_tool"
command = "ruff check ."

[[hooks]]
name = "hcom-pre-tool"
type = "pre_tool"
command = "/opt/dev/hcom/target/debug/hcom vibe-pretool"
"#;

fn hook_names(doc: &DocumentMut) -> Vec<String> {
    doc.get("hooks")
        .and_then(|item| item.as_array_of_tables())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.get("name").and_then(|v| v.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
#[serial]
fn merge_keeps_user_rows_replaces_hcom_rows_and_is_idempotent() {
    let mut doc: DocumentMut = USER_ROWS.parse().unwrap();
    merge_hcom_hooks(&mut doc);
    let once = doc.to_string();
    merge_hcom_hooks(&mut doc);
    assert_eq!(doc.to_string(), once, "merge must be idempotent");

    let names = hook_names(&doc);
    // The stale dev-build row is replaced, the user row stays first.
    assert_eq!(names[0], "my-lint");
    assert_eq!(names.len(), 1 + VIBE_HOOK_COMMANDS.len());
    for (name, _, _) in VIBE_HOOK_COMMANDS {
        assert_eq!(names.iter().filter(|n| n == name).count(), 1, "{name}");
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hooks.toml");
    std::fs::write(&path, doc.to_string()).unwrap();
    assert!(verify_hooks_at(&path));

    remove_hcom_hooks(&mut doc);
    assert_eq!(hook_names(&doc), vec!["my-lint".to_string()]);
    std::fs::write(&path, doc.to_string()).unwrap();
    assert!(!verify_hooks_at(&path));
}

#[test]
#[serial]
fn inline_hook_arrays_are_merged_verified_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hooks.toml");
    for (source, user_rows) in [
        ("hooks = []\n", vec![]),
        (
            "hooks = [{ name = \"my-lint\", type = \"post_tool\", command = \"ruff check .\" }]\n",
            vec!["my-lint".to_string()],
        ),
    ] {
        let mut doc: DocumentMut = source.parse().unwrap();
        merge_hcom_hooks(&mut doc);
        assert!(doc.to_string().contains("[[hooks]]\n"), "{doc}");
        let names = hook_names(&doc);
        assert_eq!(names[..user_rows.len()], user_rows[..], "{source}");
        assert_eq!(names.len(), user_rows.len() + VIBE_HOOK_COMMANDS.len());
        std::fs::write(&path, doc.to_string()).unwrap();
        assert!(verify_hooks_at(&path), "{source}");

        remove_hcom_hooks(&mut doc);
        assert_eq!(hook_names(&doc), user_rows, "{source}");
    }

    // An inline list that already holds hcom's rows verifies as installed.
    let mut doc = DocumentMut::new();
    merge_hcom_hooks(&mut doc);
    let inline: Vec<String> = doc["hooks"]
        .as_array_of_tables()
        .unwrap()
        .iter()
        .map(|t| t.clone().into_inline_table().to_string())
        .collect();
    std::fs::write(&path, format!("hooks = [{}]\n", inline.join(", "))).unwrap();
    assert!(verify_hooks_at(&path));
}

#[test]
fn launch_hooks_path_follows_the_launch_env() {
    use crate::hooks::runtime::LaunchCtx;
    let launch = |env: &[(&str, &str)]| LaunchCtx {
        tool: crate::tool::Tool::Vibe,
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        cwd: std::path::PathBuf::from("/work/proj"),
        args: Vec::new(),
        auto_approve: false,
    };
    let path = |env: &[(&str, &str)]| super::config::launch_hooks_path(&launch(env));
    assert_eq!(
        path(&[("HOME", "/home/u")]),
        std::path::Path::new("/home/u/.vibe/hooks.toml")
    );
    assert_eq!(
        path(&[("HOME", "/home/u"), ("VIBE_HOME", "/opt/vibe")]),
        std::path::Path::new("/opt/vibe/hooks.toml")
    );
    // Relative to the launch dir, where Vibe resolves it.
    assert_eq!(
        path(&[("HOME", "/home/u"), ("VIBE_HOME", "vh")]),
        std::path::Path::new("/work/proj/vh/hooks.toml")
    );
}

#[test]
#[serial]
fn vibe_home_follows_the_launch_precedence_and_is_absolute() {
    use super::config::{configured_vibe_home, vibe_config_dir};
    use crate::hooks::test_helpers::{EnvVarGuard, isolated_test_env};
    let (_dir, hcom_dir, home, _guard) = isolated_test_env();
    let _vibe_home = EnvVarGuard::unset("VIBE_HOME");
    assert_eq!(configured_vibe_home(), None);
    assert_eq!(vibe_config_dir(), home.join(".vibe"));

    let process_home = home.join("process-vh");
    let _vibe_home = EnvVarGuard::set("VIBE_HOME", &process_home);
    assert_eq!(vibe_config_dir(), process_home);

    // `~/.hcom/env` wins, as in the launcher; an empty value there does not.
    let env_file_home = home.join("env-file-vh");
    let env_file = hcom_dir.join("env");
    std::fs::write(&env_file, "VIBE_HOME=\n").unwrap();
    assert_eq!(vibe_config_dir(), process_home);
    std::fs::write(
        &env_file,
        format!("VIBE_HOME={}\n", env_file_home.display()),
    )
    .unwrap();
    assert_eq!(vibe_config_dir(), env_file_home);
    // Transcript search reads the same store as resume.
    assert_eq!(
        crate::transcript::disk_search_roots(crate::tool::Tool::Vibe),
        vec![env_file_home.join("logs").join("session").join("unified")]
    );

    // Relative: absolute against the current dir, where a launch from here
    // resolves it.
    std::fs::write(&env_file, "VIBE_HOME=rel/vh\n").unwrap();
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(vibe_config_dir(), cwd.join("rel").join("vh"));
    assert!(vibe_config_dir().is_absolute());
}

#[test]
fn remove_drops_empty_hooks_array() {
    let mut doc = DocumentMut::new();
    merge_hcom_hooks(&mut doc);
    remove_hcom_hooks(&mut doc);
    assert!(doc.get("hooks").is_none());
}

#[test]
fn hook_rows_use_hcom_prefix_and_vibe_types() {
    let mut doc = DocumentMut::new();
    merge_hcom_hooks(&mut doc);
    let arr = doc["hooks"].as_array_of_tables().unwrap();
    for ((name, hook_type, command), row) in VIBE_HOOK_COMMANDS.iter().zip(arr.iter()) {
        assert_eq!(row["name"].as_str(), Some(*name));
        assert_eq!(row["type"].as_str(), Some(*hook_type));
        assert_eq!(
            row["command"].as_str(),
            Some(build_vibe_hook_command(command).as_str())
        );
        // Vibe rejects `strict` on several hook types; hcom never sets it.
        assert!(row.get("strict").is_none());
    }
    // Every hook name routes back to Vibe.
    for (_, _, command) in VIBE_HOOK_COMMANDS {
        assert_eq!(
            crate::tool::Tool::from_hook_name(command),
            Some(crate::tool::Tool::Vibe)
        );
    }
}

// ── Handlers ────────────────────────────────────────────────────────────

fn make_test_db() -> (tempfile::TempDir, HcomDb) {
    let dir = tempfile::tempdir().unwrap();
    let db = HcomDb::open_raw(&dir.path().join("test.db")).unwrap();
    db.init_db().unwrap();
    (dir, db)
}

fn ctx(process_id: &str, tool: &str) -> HcomContext {
    let env = std::collections::HashMap::from([
        ("HCOM_PROCESS_ID".to_string(), process_id.to_string()),
        ("HCOM_TOOL".to_string(), tool.to_string()),
    ]);
    HcomContext::from_env(&env, std::env::current_dir().unwrap())
}

fn payload(hook_type: &str, raw: Value) -> HookPayload {
    HookPayload::from_vibe(hook_type, raw)
}

fn seed_instance(db: &HcomDb, name: &str, session_id: &str, process_id: &str, status: &str) {
    let now = chrono::Utc::now().timestamp() as f64;
    db.conn()
        .execute(
            "INSERT INTO instances (name, status, status_context, created_at, tool, session_id, last_event_id, name_announced)
                 VALUES (?1, ?2, '', ?3, 'vibe', ?4, 0, 1)",
            rusqlite::params![name, status, now, session_id],
        )
        .unwrap();
    db.set_process_binding(process_id, session_id, name)
        .unwrap();
    db.rebind_session(session_id, name).unwrap();
}

fn insert_message(db: &HcomDb, from: &str, text: &str) -> i64 {
    let data = json!({"from": from, "text": text, "scope": "broadcast"}).to_string();
    db.conn()
        .execute(
            "INSERT INTO events (type, timestamp, instance, data) VALUES ('message', '2026-01-01T00:00:01Z', ?1, ?2)",
            rusqlite::params![from, data],
        )
        .unwrap();
    db.conn().last_insert_rowid()
}

fn last_event_id(db: &HcomDb, name: &str) -> i64 {
    db.get_instance_full(name).unwrap().unwrap().last_event_id
}

fn additional_context(output: &Value) -> Option<&str> {
    output
        .pointer("/hook_specific_output/additional_context")
        .and_then(Value::as_str)
}

#[test]
#[serial]
fn pre_agent_attaches_pending_messages_and_acks_after_write() {
    let (_dir, db) = make_test_db();
    seed_instance(&db, "vela", "ses-1", "pid-1", ST_LISTENING);
    let id = insert_message(&db, "luna", "ship it");

    let (output, ack) = handle_preagent(
        &db,
        &ctx("pid-1", "vibe"),
        &payload(
            "pre_agent",
            json!({"session_id": "ses-1", "prompt": "<hcom>"}),
        ),
    );
    assert!(additional_context(&output).unwrap().contains("ship it"));
    let row = db.get_instance_full("vela").unwrap().unwrap();
    assert_eq!(row.status, ST_ACTIVE);
    assert_eq!(row.status_context, "trigger");
    // The cursor moves only once dispatch has written stdout.
    assert_eq!(last_event_id(&db, "vela"), 0);
    commit_delivery_ack(&db, &ack.expect("deferred ack"));
    assert_eq!(last_event_id(&db, "vela"), id);
}

#[test]
#[serial]
fn pre_agent_without_mail_marks_prompt_active_and_writes_nothing() {
    let (_dir, db) = make_test_db();
    seed_instance(&db, "vela", "ses-1", "pid-1", ST_LISTENING);
    let (output, ack) = handle_preagent(
        &db,
        &ctx("pid-1", "vibe"),
        &payload(
            "pre_agent",
            json!({"session_id": "ses-1", "prompt": "fix the bug"}),
        ),
    );
    assert_eq!(output, json!({}));
    assert!(ack.is_none());
    let row = db.get_instance_full("vela").unwrap().unwrap();
    assert_eq!(
        (row.status.as_str(), row.status_context.as_str()),
        (ST_ACTIVE, "prompt")
    );
}

#[test]
#[serial]
fn post_tool_delivers_mid_turn_and_leaves_approval_block() {
    let (_dir, db) = make_test_db();
    seed_instance(&db, "vela", "ses-1", "pid-1", ST_ACTIVE);
    let context = ctx("pid-1", "vibe");

    handle_notification(
        &db,
        &context,
        &payload(
            "notification",
            json!({"session_id": "ses-1", "notification_type": "approval_requested", "tool_name": "file_system.bash"}),
        ),
    );
    let row = db.get_instance_full("vela").unwrap().unwrap();
    assert_eq!(
        (row.status.as_str(), row.status_context.as_str()),
        (ST_BLOCKED, "approval")
    );

    let id = insert_message(&db, "luna", "mid-turn note");
    let (output, ack) = handle_posttool(
        &db,
        &context,
        &payload(
            "post_tool",
            json!({"session_id": "ses-1", "tool_name": "file_system.bash", "tool_input": {"command": "ls"}}),
        ),
    );
    assert!(
        additional_context(&output)
            .unwrap()
            .contains("mid-turn note")
    );
    let row = db.get_instance_full("vela").unwrap().unwrap();
    assert_eq!(row.status, ST_ACTIVE);
    commit_delivery_ack(&db, &ack.unwrap());
    assert_eq!(last_event_id(&db, "vela"), id);
}

#[test]
#[serial]
fn post_tool_failure_leaves_approval_block_but_holds_messages() {
    let (_dir, db) = make_test_db();
    seed_instance(&db, "vela", "ses-1", "pid-1", ST_BLOCKED);
    db.conn()
        .execute(
            "UPDATE instances SET status_context = 'approval' WHERE name = 'vela'",
            [],
        )
        .unwrap();
    let context = ctx("pid-1", "vibe");
    let before = last_event_id(&db, "vela");
    insert_message(&db, "luna", "held note");

    // A denied approval: Vibe would show the model only the appended context
    // instead of the denial error.
    let (output, ack) = handle_posttool(
        &db,
        &context,
        &payload(
            "post_tool",
            json!({
                "session_id": "ses-1",
                "tool_name": "file_system.bash",
                "tool_input": {"command": "rm x"},
                "tool_status": "failure",
                "tool_error": "Tool execution denied by approval callback",
            }),
        ),
    );
    assert_eq!(output, json!({}));
    assert!(ack.is_none());
    let row = db.get_instance_full("vela").unwrap().unwrap();
    assert_eq!(
        (row.status.as_str(), row.status_context.as_str()),
        (ST_ACTIVE, "approved:file_system.bash")
    );
    assert_eq!(last_event_id(&db, "vela"), before);

    // The next successful call delivers it.
    let (output, ack) = handle_posttool(
        &db,
        &context,
        &payload(
            "post_tool",
            json!({"session_id": "ses-1", "tool_name": "file_system.bash", "tool_status": "success"}),
        ),
    );
    assert!(additional_context(&output).unwrap().contains("held note"));
    assert!(ack.is_some());
}

#[test]
#[serial]
fn turn_completed_notification_sets_listening() {
    let (_dir, db) = make_test_db();
    seed_instance(&db, "vela", "ses-1", "pid-1", ST_ACTIVE);
    handle_notification(
        &db,
        &ctx("pid-1", "vibe"),
        &payload(
            "notification",
            json!({"session_id": "ses-1", "notification_type": "turn_completed"}),
        ),
    );
    assert_eq!(
        db.get_instance_full("vela").unwrap().unwrap().status,
        ST_LISTENING
    );
}

#[test]
fn only_hcom_launched_vibe_main_agents_pass_the_gate() {
    let main = json!({"session_id": "ses-1", "parent_session_id": null});
    let sub = json!({"session_id": "ses-2", "parent_session_id": "ses-1"});
    assert!(is_hcom_main_agent(&ctx("pid-1", "vibe"), &main));
    assert!(!is_hcom_main_agent(&ctx("pid-1", "vibe"), &sub));
    assert!(!is_hcom_main_agent(&ctx("pid-1", "kimi"), &main));
    assert!(!is_hcom_main_agent(&ctx("", "vibe"), &main));
}

#[test]
#[serial]
fn session_end_stops_only_on_own_session_and_not_on_clear() {
    let (_dir, db) = make_test_db();
    seed_instance(&db, "vela", "ses-1", "pid-1", ST_LISTENING);
    let context = ctx("pid-1", "vibe");

    handle_sessionend(
        &db,
        &context,
        &payload(
            "session_end",
            json!({"session_id": "ses-1", "reason": "clear"}),
        ),
    );
    assert_eq!(
        db.get_instance_full("vela").unwrap().unwrap().status,
        ST_LISTENING
    );

    handle_sessionend(
        &db,
        &context,
        &payload(
            "session_end",
            json!({"session_id": "ses-1", "reason": "exit"}),
        ),
    );
    assert!(
        db.get_instance_full("vela")
            .unwrap()
            .is_none_or(|row| row.status != ST_LISTENING),
        "exit must stop the instance"
    );
}

#[test]
#[serial]
fn stopped_instance_does_not_rejoin_from_a_later_hook() {
    let (_dir, db) = make_test_db();
    let context = ctx("pid-1", "vibe");
    seed_instance(&db, "luna", "ses-1", "pid-1", ST_LISTENING);
    crate::hooks::common::stop_instance(&db, "luna", "cli", "stop");
    insert_message(&db, "bob", "hello after stop");

    for hook_type in ["pre_agent", "post_tool"] {
        let p = payload(
            hook_type,
            json!({"session_id": "ses-1", "prompt": "<hcom>", "tool_name": "bash", "tool_status": "success"}),
        );
        let (output, ack) = if hook_type == "pre_agent" {
            handle_preagent(&db, &context, &p)
        } else {
            handle_posttool(&db, &context, &p)
        };
        assert_eq!(output, json!({}), "{hook_type}");
        assert!(ack.is_none(), "{hook_type}");
    }
    let rows: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM instances", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0, "no identity may be recreated after hcom stop");
}

#[test]
#[serial]
fn stopped_process_does_not_attach_to_the_replacement_of_its_session() {
    // `hcom r luna` starts a new process. Vibe 3.1.4 `--resume` gives it a
    // new session id; the same id must be safe too.
    for new_session in ["ses-2", "ses-1"] {
        let (_dir, db) = make_test_db();
        let old_process = ctx("pid-1", "vibe");
        seed_instance(&db, "luna", "ses-1", "pid-1", ST_LISTENING);
        crate::hooks::common::stop_instance(&db, "luna", "cli", "stop");
        seed_instance(&db, "luna", new_session, "pid-2", ST_LISTENING);
        let cursor = last_event_id(&db, "luna");
        insert_message(&db, "bob", "for the resumed luna");

        for hook_type in ["pre_agent", "pre_tool", "post_tool"] {
            let p = payload(
                hook_type,
                json!({"session_id": "ses-1", "prompt": "<hcom>", "tool_name": "bash", "tool_status": "success"}),
            );
            let (output, ack) = match hook_type {
                "pre_agent" => handle_preagent(&db, &old_process, &p),
                "pre_tool" => (super::handle_pretool(&db, &old_process, &p), None),
                _ => handle_posttool(&db, &old_process, &p),
            };
            assert_eq!(output, json!({}), "{new_session} {hook_type}");
            assert!(ack.is_none(), "{new_session} {hook_type}");
        }
        assert_eq!(
            db.get_process_binding("pid-1").unwrap(),
            None,
            "{new_session}"
        );
        assert_eq!(
            db.get_process_binding("pid-2").unwrap().as_deref(),
            Some("luna"),
            "{new_session}"
        );
        let luna = db.get_instance_full("luna").unwrap().unwrap();
        assert_eq!(luna.session_id.as_deref(), Some(new_session));
        assert_eq!(
            luna.last_event_id, cursor,
            "{new_session}: message must stay queued"
        );
    }
}

#[test]
fn payload_points_transcript_at_session_meta() {
    let p = payload(
        "pre_tool",
        json!({
            "session_id": "ses-1",
            "transcript_path": "/home/u/.vibe/logs/session/unified/ses-1",
            "tool_name": "file_system.bash",
            "tool_input": {"command": "ls"},
        }),
    );
    // Platform separator: Windows joins with `\`.
    let expected =
        std::path::Path::new("/home/u/.vibe/logs/session/unified/ses-1").join("meta.json");
    assert_eq!(p.transcript_path.as_deref(), expected.to_str());
    assert_eq!(p.tool_name, "file_system.bash");
    assert_eq!(
        crate::transcript::agent_name_from_path(p.transcript_path.as_deref().unwrap()),
        "vibe"
    );
}
