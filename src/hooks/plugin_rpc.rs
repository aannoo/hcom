//! RPC handlers shared by hcom's TypeScript plugins (pi, omp, opencode/kilo).
//!
//! Every reply exits 0 so the agent never treats the hook itself as failed;
//! success carries a payload and failure is `{"error": ...}`. The plugins must
//! check for the payload, not the exit code.

use serde_json::{Value, json};

use crate::db::HcomDb;
use crate::instance_lifecycle as lifecycle;
use crate::instances;
use crate::log::log_error;
use crate::shared::{ST_ACTIVE, ST_LISTENING};

use super::common;

pub(crate) fn parse_flag(argv: &[String], flag: &str) -> Option<String> {
    argv.iter()
        .position(|a| a == flag)
        .and_then(|i| argv.get(i + 1))
        .cloned()
}

pub(crate) fn has_flag(argv: &[String], flag: &str) -> bool {
    argv.iter().any(|a| a == flag)
}

/// Resolve the process binding key for `plugin start`.
///
/// `--process-id` lets a session opened WITHOUT the hcom launcher mint its own
/// binding key; `HCOM_PROCESS_ID` stays the launcher path. The flag wins so a
/// caller can override an inherited environment.
pub(crate) fn resolve_process_id(argv: &[String], env_process_id: Option<&str>) -> Option<String> {
    parse_flag(argv, "--process-id").or_else(|| env_process_id.map(str::to_string))
}

/// `--pid <n>`: numeric pid of the client process.
///
/// Persisting it is what makes stale cleanup spare a live session: without a
/// numeric pid the collector drops an instance whose process is very much alive
/// (the failure mode `hcom start` had before this flag).
pub(crate) fn parse_pid_flag(argv: &[String]) -> Option<u32> {
    // 0 is not a real client pid (it is the swapper on Linux and rejected by the
    // DB layer), so treat it as "no pid given" instead of attempting to persist it.
    parse_flag(argv, "--pid")
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|pid| *pid > 0)
}

pub(crate) fn persist_pid(db: &HcomDb, name: &str, pid: Option<u32>) {
    let Some(pid) = pid else { return };
    if let Err(e) = db.update_instance_pid(name, pid) {
        log_error(
            "hooks",
            "hook.error",
            &format!("hook=plugin-start op=update_instance_pid err={e}"),
        );
    }
}

pub(crate) fn error(message: impl std::fmt::Display) -> (i32, String) {
    (0, json!({ "error": message.to_string() }).to_string())
}

fn ok(payload: Value) -> (i32, String) {
    (0, payload.to_string())
}

/// Resolve `--name` to an existing instance, so writes to a stopped or
/// unknown instance fail loudly instead of updating zero rows.
fn require_instance(db: &HcomDb, argv: &[String]) -> Result<String, (i32, String)> {
    let name = parse_flag(argv, "--name").ok_or_else(|| error("Missing --name"))?;
    match db.get_instance_status(&name) {
        Ok(Some(_)) => Ok(name),
        Ok(None) => Err(error(format!("Unknown instance: {name}"))),
        Err(e) => Err(error(format!("Instance lookup failed: {e}"))),
    }
}

/// Plugin RPCs act for `--name` only while this launch is attached to it, so a
/// stopped plugin process cannot read, ack, or stop a resumed participant.
pub(crate) fn require_attached(
    ctx: &crate::shared::context::HcomContext,
    db: &HcomDb,
    argv: &[String],
) -> Result<(), (i32, String)> {
    let name = parse_flag(argv, "--name").ok_or_else(|| error("Missing --name"))?;
    // A launchless session mints its key with `--process-id`, so read/ack/stop
    // must resolve the same way `plugin start` did: flag first, then env.
    let process_id = resolve_process_id(argv, ctx.process_id.as_deref());
    let owner = process_id
        .as_deref()
        .and_then(|token| db.get_process_binding(token).ok().flatten());
    if owner.as_deref() != Some(name.as_str()) {
        return Err(error(format!("{name} is not attached to this process")));
    }
    Ok(())
}

/// New instances start reading from their launch event (or the current tail),
/// never from 0, which would replay the whole broadcast backlog.
pub(crate) fn initialize_last_event_id(db: &HcomDb, instance_name: &str) {
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
        updates.insert("last_event_id".into(), json!(new_id));
        instances::update_instance_position(db, instance_name, &updates);
    }
}

/// `<tool>-status --name N --status S [--context C] [--detail D]`
pub(crate) fn handle_status(db: &HcomDb, argv: &[String]) -> (i32, String) {
    let name = match require_instance(db, argv) {
        Ok(name) => name,
        Err(reply) => return reply,
    };
    let Some(status) = parse_flag(argv, "--status") else {
        return error("Missing --status");
    };
    let context = parse_flag(argv, "--context").unwrap_or_default();
    let detail = parse_flag(argv, "--detail").unwrap_or_default();
    let was_listening = db
        .get_instance_full(&name)
        .ok()
        .flatten()
        .is_some_and(|inst| inst.status == ST_LISTENING);

    if let Err(e) = lifecycle::try_set_status(
        db,
        &name,
        &status,
        &context,
        lifecycle::StatusUpdate {
            detail: &detail,
            ..Default::default()
        },
    ) {
        return error(format!("Status update failed: {e}"));
    }
    if status == ST_LISTENING && !was_listening {
        crate::notify::wake(db, &name, &[]);
    }
    ok(json!({ "ok": true }))
}

/// `<tool>-read --name N` returns `{"messages": [...], "has_more": bool}`: the
/// oldest unread messages, at most one delivery's worth, in event order.
///
/// `<tool>-read --name N --check` prints `true`/`false` (diagnostics, tests).
///
/// `<tool>-read --name N --ack --up-to ID` advances the cursor to ID, never
/// backwards, and returns `{"acked_to": ID}`.
pub(crate) fn handle_read(db: &HcomDb, argv: &[String]) -> (i32, String) {
    let name = match require_instance(db, argv) {
        Ok(name) => name,
        Err(reply) => return reply,
    };

    if has_flag(argv, "--ack") {
        let Some(up_to) = parse_flag(argv, "--up-to") else {
            return error("Missing --up-to");
        };
        let ack_id = match up_to.parse::<i64>() {
            Ok(id) if id > 0 => id,
            _ => return error(format!("Invalid --up-to: {up_to}")),
        };
        return match db.advance_instance_cursor(&name, ack_id) {
            Ok(()) => ok(json!({ "acked_to": ack_id })),
            Err(e) => error(format!("Acknowledgement failed: {e}")),
        };
    }

    let unread = match db.try_get_unread_messages(&name) {
        Ok(messages) => messages,
        Err(e) => return error(format!("Read failed: {e}")),
    };
    if has_flag(argv, "--check") {
        return ok(json!(!unread.is_empty()));
    }
    let messages: Vec<Value> = unread.iter().map(common::message_to_value).collect();
    let batch = common::limit_delivery_messages(&messages);
    ok(json!({
        "messages": batch,
        "has_more": messages.len() > batch.len(),
    }))
}

/// `<tool>-beforetool --name N --tool T [--input-json J]` (OpenCode/Kilo:
/// `opencode-status` with `--tool`): record the tool as the instance's
/// activity. Observation only; it never blocks the tool.
pub(crate) fn handle_tool(db: &HcomDb, argv: &[String], tool: &str) -> (i32, String) {
    let name = match require_instance(db, argv) {
        Ok(name) => name,
        Err(reply) => return reply,
    };
    let Some(tool_name) = parse_flag(argv, "--tool").filter(|t| !t.is_empty()) else {
        return error("Missing --tool");
    };
    let input = parse_flag(argv, "--input-json")
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or_else(|| json!({}));
    let detail = super::family::extract_tool_detail(tool, &tool_name, &input);
    match lifecycle::try_set_status(
        db,
        &name,
        ST_ACTIVE,
        &format!("tool:{tool_name}"),
        lifecycle::StatusUpdate {
            detail: &detail,
            ..Default::default()
        },
    ) {
        Ok(()) => ok(json!({ "ok": true })),
        Err(e) => error(format!("Status update failed: {e}")),
    }
}

/// `<tool>-stop --name N [--reason R] [--soft]`. `--soft` keeps the instance
/// row (and process binding) so the same process can rebind it.
pub(crate) fn handle_stop(db: &HcomDb, argv: &[String]) -> (i32, String) {
    let Some(name) = parse_flag(argv, "--name") else {
        return error("Missing --name");
    };
    let reason = parse_flag(argv, "--reason").unwrap_or_else(|| "unknown".to_string());
    if has_flag(argv, "--soft") {
        match common::soft_finalize_session(
            db,
            &name,
            &reason,
            None,
            true,
            common::SoftStop::ProcessContinues,
        ) {
            Ok(()) => ok(json!({ "ok": true, "soft": true })),
            Err(e) => error(format!("Stop failed: {e}")),
        }
    } else {
        match common::finalize_session(db, &name, &reason, None) {
            common::StopOutcome::RetryableError(e) => error(format!("Stop failed: {e}")),
            _ => ok(json!({ "ok": true })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn test_db() -> (tempfile::TempDir, HcomDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_at(&dir.path().join("test.db")).unwrap();
        (dir, db)
    }

    fn save_instance(db: &HcomDb, name: &str, last_event_id: i64) {
        let mut row = serde_json::Map::new();
        row.insert("name".into(), json!(name));
        row.insert("tool".into(), json!("pi"));
        row.insert("status".into(), json!(ST_LISTENING));
        row.insert("status_context".into(), json!(""));
        row.insert("status_detail".into(), json!(""));
        row.insert("last_event_id".into(), json!(last_event_id));
        row.insert("created_at".into(), json!(1.0));
        db.save_instance_named(name, &row).unwrap();
    }

    #[test]
    fn rpc_accepts_the_self_minted_process_id_flag() {
        // A session opened WITHOUT the launcher mints its key with `--process-id`
        // and repeats it on read/ack. The owner check must resolve the same way
        // `plugin start` did, or binding would not survive into delivery.
        let (_dir, db) = test_db();
        save_instance(&db, "luna", 0);
        db.set_process_binding("pi:4242:sid-1", "", "luna").unwrap();
        let argv = sv(&["--name", "luna", "--process-id", "pi:4242:sid-1"]);
        let ctx = |token: Option<&str>| {
            let mut env = std::collections::HashMap::new();
            if let Some(t) = token {
                env.insert("HCOM_PROCESS_ID".to_string(), t.to_string());
            }
            crate::shared::context::HcomContext::from_env(&env, std::env::temp_dir())
        };
        // No env at all: the flag alone authorizes the launchless session.
        assert!(require_attached(&ctx(None), &db, &argv).is_ok());
        // Another launch's key is still rejected.
        let outro = sv(&["--name", "luna", "--process-id", "pi:9:sid-9"]);
        assert!(require_attached(&ctx(None), &db, &outro).is_err());
        // The flag wins over an inherited env, mirroring resolve_process_id.
        assert!(require_attached(&ctx(Some("pi:9:sid-9")), &db, &argv).is_ok());
    }

    #[test]
    fn rpc_requires_this_launch_to_hold_the_name() {
        let (_dir, db) = test_db();
        save_instance(&db, "luna", 0);
        db.set_process_binding("current", "", "luna").unwrap();
        let argv = sv(&["--name", "luna"]);
        let ctx = |token: &str| {
            let env = std::collections::HashMap::from([("HCOM_PROCESS_ID".into(), token.into())]);
            crate::shared::context::HcomContext::from_env(&env, std::env::temp_dir())
        };
        assert!(require_attached(&ctx("current"), &db, &argv).is_ok());
        // A stopped launch whose name was resumed elsewhere.
        assert!(require_attached(&ctx("stopped"), &db, &argv).is_err());
        assert!(require_attached(&ctx("current"), &db, &sv(&["--name", "nova"])).is_err());
    }

    fn send(db: &HcomDb, to: &str, text: &str) -> i64 {
        db.log_event(
            "message",
            "bigboss",
            &json!({"from": "bigboss", "scope": "mentions", "mentions": [to], "text": text}),
        )
        .unwrap()
    }

    fn cursor(db: &HcomDb, name: &str) -> i64 {
        db.get_instance_full(name).unwrap().unwrap().last_event_id
    }

    fn reply(out: &(i32, String)) -> Value {
        assert_eq!(out.0, 0);
        serde_json::from_str(&out.1).unwrap()
    }

    #[test]
    fn ack_never_moves_cursor_backwards() {
        let (_dir, db) = test_db();
        save_instance(&db, "luna", 40);

        let out = reply(&handle_read(
            &db,
            &sv(&["--name", "luna", "--ack", "--up-to", "42"]),
        ));
        assert_eq!(out["acked_to"], 42);
        assert_eq!(cursor(&db, "luna"), 42);

        // A stale acknowledgement from an older delivery must not rewind.
        let out = reply(&handle_read(
            &db,
            &sv(&["--name", "luna", "--ack", "--up-to", "7"]),
        ));
        assert_eq!(out["acked_to"], 7);
        assert_eq!(cursor(&db, "luna"), 42);
    }

    #[test]
    fn ack_rejects_bad_input_and_unknown_instances() {
        let (_dir, db) = test_db();
        save_instance(&db, "luna", 0);
        for argv in [
            sv(&["--name", "luna", "--ack"]),
            sv(&["--name", "luna", "--ack", "--up-to", "abc"]),
            sv(&["--name", "luna", "--ack", "--up-to", "0"]),
            sv(&["--name", "gone", "--ack", "--up-to", "5"]),
            sv(&["--ack", "--up-to", "5"]),
        ] {
            let out = reply(&handle_read(&db, &argv));
            assert!(out.get("error").is_some(), "{argv:?} -> {out}");
        }
        assert_eq!(cursor(&db, "luna"), 0);
    }

    #[test]
    fn read_returns_ordered_bounded_prefix() {
        let (_dir, db) = test_db();
        save_instance(&db, "luna", 0);
        let total = crate::shared::MAX_MESSAGES_PER_DELIVERY + 3;
        let ids: Vec<i64> = (0..total)
            .map(|i| send(&db, "luna", &format!("m{i}")))
            .collect();

        let out = reply(&handle_read(&db, &sv(&["--name", "luna"])));
        let batch = out["messages"].as_array().unwrap();
        assert_eq!(batch.len(), crate::shared::MAX_MESSAGES_PER_DELIVERY);
        assert_eq!(out["has_more"], true);
        let got: Vec<i64> = batch
            .iter()
            .map(|m| m["event_id"].as_i64().unwrap())
            .collect();
        assert_eq!(got, ids[..batch.len()]);

        // Acking the prefix leaves exactly the remainder unread.
        let last = got.last().unwrap().to_string();
        reply(&handle_read(
            &db,
            &sv(&["--name", "luna", "--ack", "--up-to", &last]),
        ));
        let out = reply(&handle_read(&db, &sv(&["--name", "luna"])));
        assert_eq!(out["messages"].as_array().unwrap().len(), 3);
        assert_eq!(out["has_more"], false);
    }

    #[test]
    fn read_distinguishes_empty_from_failure() {
        let (_dir, db) = test_db();
        save_instance(&db, "luna", db.get_last_event_id());
        let out = reply(&handle_read(&db, &sv(&["--name", "luna"])));
        assert_eq!(out["messages"], json!([]));
        assert_eq!(out["has_more"], false);
        assert_eq!(
            reply(&handle_read(&db, &sv(&["--name", "luna", "--check"]))),
            json!(false)
        );
        send(&db, "luna", "hi");
        assert_eq!(
            reply(&handle_read(&db, &sv(&["--name", "luna", "--check"]))),
            json!(true)
        );

        let out = reply(&handle_read(&db, &sv(&["--name", "gone"])));
        assert!(out.get("error").is_some());
    }

    #[test]
    fn status_and_tool_report_unknown_instances() {
        let (_dir, db) = test_db();
        let status = reply(&handle_status(
            &db,
            &sv(&["--name", "gone", "--status", "active"]),
        ));
        assert!(status.get("error").is_some());
        let tool = reply(&handle_tool(
            &db,
            &sv(&["--name", "gone", "--tool", "bash"]),
            "pi",
        ));
        assert!(tool.get("error").is_some());
    }

    /// A stop whose DB teardown fails must not report success: OMP releases its
    /// identity latch only on a confirmed stop.
    #[test]
    fn stop_reports_failed_teardown() {
        let (_dir, db) = test_db();
        save_instance(&db, "luna", 0);
        db.conn()
            .execute_batch(
                "CREATE TRIGGER no_inactive BEFORE UPDATE OF status ON instances
                 WHEN NEW.status = 'inactive' BEGIN SELECT RAISE(ABORT, 'blocked'); END;
                 CREATE TRIGGER no_delete BEFORE DELETE ON instances
                 BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
            )
            .unwrap();
        let soft = reply(&handle_stop(&db, &sv(&["--name", "luna", "--soft"])));
        assert!(soft.get("error").is_some(), "{soft}");
        db.conn().execute_batch("DROP TRIGGER no_inactive").unwrap();
        let hard = reply(&handle_stop(&db, &sv(&["--name", "luna"])));
        assert!(hard.get("error").is_some(), "{hard}");
        assert!(db.get_instance_full("luna").unwrap().is_some());

        db.conn().execute_batch("DROP TRIGGER no_delete").unwrap();
        assert_eq!(
            reply(&handle_stop(&db, &sv(&["--name", "luna"]))),
            json!({ "ok": true })
        );
        assert!(db.get_instance_full("luna").unwrap().is_none());
    }

    #[test]
    fn tool_records_activity_without_a_decision() {
        let (_dir, db) = test_db();
        save_instance(&db, "luna", 0);
        let out = reply(&handle_tool(
            &db,
            &sv(&[
                "--name",
                "luna",
                "--tool",
                "bash",
                "--input-json",
                r#"{"command":"ls -la"}"#,
            ]),
            "pi",
        ));
        assert_eq!(out, json!({ "ok": true }));
        let inst = db.get_instance_full("luna").unwrap().unwrap();
        assert_eq!(inst.status, ST_ACTIVE);
        assert_eq!(inst.status_context, "tool:bash");
        assert_eq!(inst.status_detail, "ls -la");
    }
}
