//! Shared CLI infrastructure for hcom commands.
//!
//! - `CommandContext` builder (`_build_ctx_for_command`)
//! - Identity gating (`REQUIRE_IDENTITY`)
//! - `set_hookless_command_status` — status for non-hook CLI commands
//! - `maybe_deliver_pending_messages` — append unread for codex/adhoc
//! - `format_messages_human` — human-readable message formatting

use crate::db::HcomDb;
use crate::identity;
use crate::instance_lifecycle as lifecycle;
use crate::instances;
#[cfg(test)]
use crate::shared::SenderIdentity;
use crate::shared::ansi::{BOLD, DIM, FG_CYAN, RESET};
use crate::shared::{
    CommandContext, HcomError, ST_ACTIVE, ST_INACTIVE, SenderKind, status_fg, status_icon,
};

/// Commands that should NOT trigger hookless status update.
/// Handled internally or are lifecycle commands.
const STATUS_SKIP_COMMANDS: &[&str] = &["listen", "start", "stop", "kill", "reset", "status"];

/// Build a CommandContext for a CLI invocation (best-effort identity resolution).
///
///
/// `start` is special: it may be invoked with `--name <agent_id>` before the
/// instance exists (subagent registration), so the CLI must not resolve it.
///
/// Returns `Err` when an explicit `--name` fails to resolve — the error
/// propagates to the caller (printed + exit 1).
/// Without explicit name, resolution errors are swallowed (best-effort).
pub fn build_ctx_for_command(
    db: &HcomDb,
    cmd: Option<&str>,
    explicit_name: Option<&str>,
    go: bool,
    process_id: Option<&str>,
    codex_thread_id: Option<&str>,
) -> Result<CommandContext, HcomError> {
    let identity = if let Some(name) = explicit_name {
        if cmd != Some("start") {
            // Explicit --name: propagate typed error so router can pattern-match.
            Some(identity::resolve_identity(
                db,
                Some(name),
                None,
                None,
                process_id,
                codex_thread_id,
                None,
            )?)
        } else {
            None
        }
    } else {
        // No explicit name: best-effort, swallow errors
        identity::resolve_identity(db, None, None, None, process_id, codex_thread_id, None).ok()
    };

    Ok(CommandContext {
        explicit_name: explicit_name.map(|s| s.to_string()),
        identity,
        go,
    })
}

/// Check identity gating for a CLI command.
///
/// Returns `Ok(())` if the command can proceed, or `Err(message)` if identity
/// is required but not available.
///
pub fn check_identity_gate(
    cmd: &str,
    ctx: &CommandContext,
    has_from_flag: bool,
    is_inside_ai_tool: bool,
) -> Result<(), String> {
    if !identity::requires_identity(cmd) {
        return Ok(());
    }

    // --name provided or --from/-b bypass
    if ctx.explicit_name.is_some() {
        return Ok(());
    }
    if cmd == "send" && has_from_flag {
        return Ok(());
    }

    // Check if resolved identity is a registered instance
    let is_participant = ctx
        .identity
        .as_ref()
        .is_some_and(|id| matches!(id.kind, SenderKind::Instance) && id.instance_data.is_some());

    if !is_participant {
        let hcom_cmd = crate::runtime_env::build_hcom_command();
        let mut msg = format!(
            "hcom identity not found, you need to run '{hcom_cmd} start' first, then use '{hcom_cmd} {cmd}'"
        );
        if is_inside_ai_tool {
            msg.push_str(&format!(
                "\nUsage:\n  {hcom_cmd} start              # New hcom identity (assigns new name)\n  {hcom_cmd} start --as <name>  # Rebind to existing identity\n  Then use the command: {hcom_cmd} {cmd} --name <name>"
            ));
        } else {
            msg.push_str(&format!("\nUsage: {hcom_cmd} start"));
        }
        return Err(msg);
    }

    Ok(())
}

/// Set status for instances without PreToolUse hooks before command runs.
///
/// Claude/Gemini main instances have PreToolUse hooks that set active:tool:*.
/// These instance types need explicit status updates here:
/// - Subagent: has Claude hooks but not for hcom Bash commands
/// - Codex: has notify hook (turn-end) but no pre-tool hook
/// - Adhoc: no hooks at all
///
/// Status model:
/// - Adhoc: inactive:tool:* (no hooks to reset, just records "this happened")
/// - Others: active:tool:* (hooks will reset to idle when turn ends)
pub fn set_hookless_command_status(db: &HcomDb, cmd_name: &str, ctx: &CommandContext) {
    if STATUS_SKIP_COMMANDS.contains(&cmd_name) {
        return;
    }

    let identity = match &ctx.identity {
        Some(id) => id,
        None => return,
    };

    if !matches!(identity.kind, SenderKind::Instance) {
        return;
    }

    let instance_data = match &identity.instance_data {
        Some(d) => d,
        None => return,
    };

    let tool = instance_data
        .get("tool")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let has_parent = instance_data
        .get("parent_name")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty());

    // Only set status for hookless instances:
    // - subagent (has parent_name)
    // - codex
    // - adhoc
    let is_hookless = has_parent || tool == "codex" || tool == "adhoc";
    if !is_hookless {
        return;
    }

    let context = format!("tool:{cmd_name}");
    let status = if tool == "adhoc" {
        ST_INACTIVE
    } else {
        ST_ACTIVE
    };
    lifecycle::set_status(db, &identity.name, status, &context, Default::default());
}

/// For hookless instances (codex/adhoc): append unread messages after command output.
///
/// Codex and adhoc instances have no delivery hooks, so messages are delivered
/// via CLI command output. Skips for --json output to preserve machine-readable format.
///
/// Not display-only: also advances the instance cursor and updates delivery status.
/// This is the hookless counterpart to hook-based delivery.
///
/// Returns formatted output string if messages were delivered, None otherwise.
pub fn maybe_deliver_pending_messages(
    db: &HcomDb,
    ctx: &CommandContext,
    has_json_flag: bool,
) -> Option<String> {
    if has_json_flag {
        return None;
    }

    let identity = ctx.identity.as_ref()?;
    if !matches!(identity.kind, SenderKind::Instance) {
        return None;
    }

    let instance_data = identity.instance_data.as_ref()?;
    let tool = instance_data
        .get("tool")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if tool != "codex" && tool != "adhoc" {
        return None;
    }

    // Get unread messages
    let messages = db.get_unread_messages(&identity.name);
    if messages.is_empty() {
        return None;
    }

    // Advance cursor — update last_event_id on the instance
    if let Some(last) = messages.last() {
        if let Some(id) = last.event_id {
            let mut updates = serde_json::Map::new();
            updates.insert("last_event_id".into(), serde_json::json!(id));
            instances::update_instance_position(db, &identity.name, &updates);
        }
    }

    // Format with divider
    let formatted = format_hook_messages_simple_from_msgs(db, &messages, &identity.name);
    let output = format!(
        "\n{}\n[hcom]\n{}\n{}",
        "─".repeat(40),
        "─".repeat(40),
        formatted,
    );

    // Update status after delivery
    let msg_ts = messages
        .last()
        .and_then(|m| m.timestamp.as_deref())
        .unwrap_or("");
    let sender_display = instances::get_display_name(db, &messages[0].from);
    let context = format!("deliver:{sender_display}");

    let status = if tool == "codex" {
        ST_ACTIVE
    } else {
        ST_INACTIVE
    };
    lifecycle::set_status(
        db,
        &identity.name,
        status,
        &context,
        lifecycle::StatusUpdate {
            msg_ts,
            ..Default::default()
        },
    );

    Some(output)
}

/// Format messages for human terminal display.
///
/// Shared by list (display), send (recipient feedback), listen (human output).
/// Adds ANSI color, timestamps, and layout suitable for terminal viewing.
///
/// Format: `[intent:thread #id] sender → recipient: text`
/// With colors: status icon colored, sender bold, metadata dim.
#[allow(dead_code)]
pub fn format_messages_human(
    db: &HcomDb,
    messages: &[serde_json::Value],
    instance_name: &str,
) -> String {
    if messages.is_empty() {
        return String::new();
    }

    let recipient_display = instances::get_display_name(db, instance_name);
    let mut parts = Vec::new();

    for msg in messages {
        let from = msg
            .get("from")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let text = msg.get("message").and_then(|v| v.as_str()).unwrap_or("");
        let intent = msg.get("intent").and_then(|v| v.as_str());
        let thread = msg.get("thread").and_then(|v| v.as_str());
        let event_id = msg.get("event_id").and_then(|v| v.as_i64());

        // Build prefix
        let prefix = build_message_prefix(intent, thread, event_id, msg);

        // Sender display name with status
        let sender_display = instances::get_display_name(db, from);
        let sender_status = db
            .get_instance_full(from)
            .ok()
            .flatten()
            .map(|d| d.status.clone())
            .unwrap_or_else(|| "inactive".to_string());
        let icon = status_icon(&sender_status);
        let fg = status_fg(&sender_status);

        // Others count
        let others = msg
            .get("delivered_to")
            .and_then(|v| v.as_array())
            .map(|a| a.len().saturating_sub(1))
            .unwrap_or(0);

        let recipient = if others > 0 {
            let plural = if others > 1 { "s" } else { "" };
            format!("{recipient_display} (+{others} other{plural})")
        } else {
            recipient_display.clone()
        };

        parts.push(format!(
            "{DIM}{prefix}{RESET} {fg}{icon}{RESET} {BOLD}{sender_display}{RESET} → {FG_CYAN}{recipient}{RESET}: {text}"
        ));
    }

    if parts.len() == 1 {
        parts[0].clone()
    } else {
        format!(
            "{DIM}[{} messages]{RESET}\n{}",
            parts.len(),
            parts.join("\n")
        )
    }
}

/// Build message prefix from envelope fields.
///
/// Format: `[intent:thread #id]` or `[intent #id]` or `[thread:name #id]` or `[new message #id]`
fn build_message_prefix(
    intent: Option<&str>,
    thread: Option<&str>,
    event_id: Option<i64>,
    msg: &serde_json::Value,
) -> String {
    // Build ID reference (local or remote)
    let relay = msg.get("_relay");
    let id_ref = if let Some(relay) = relay {
        let short = relay.get("short").and_then(|v| v.as_str()).unwrap_or("");
        let rid = relay.get("id").and_then(|v| v.as_i64());
        if !short.is_empty() {
            if let Some(id) = rid {
                format!("#{id}:{short}")
            } else {
                String::new()
            }
        } else if let Some(id) = event_id {
            format!("#{id}")
        } else {
            String::new()
        }
    } else if let Some(id) = event_id {
        format!("#{id}")
    } else {
        String::new()
    };

    // Build prefix based on envelope fields
    let prefix = match (intent, thread) {
        (Some(i), Some(t)) => format!("{i}:{t}"),
        (Some(i), None) => i.to_string(),
        (None, Some(t)) => format!("thread:{t}"),
        (None, None) => "new message".to_string(),
    };

    if id_ref.is_empty() {
        format!("[{prefix}]")
    } else {
        format!("[{prefix} {id_ref}]")
    }
}

/// Simple format for hook-style messages (no ANSI colors).
///
#[allow(dead_code)]
fn format_hook_messages_simple(
    db: &HcomDb,
    messages: &[serde_json::Value],
    instance_name: &str,
) -> String {
    if messages.is_empty() {
        return String::new();
    }

    let recipient_display = instances::get_display_name(db, instance_name);

    if messages.len() == 1 {
        let msg = &messages[0];
        let from = msg
            .get("from")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let text = msg.get("message").and_then(|v| v.as_str()).unwrap_or("");
        let intent = msg.get("intent").and_then(|v| v.as_str());
        let thread = msg.get("thread").and_then(|v| v.as_str());
        let event_id = msg.get("event_id").and_then(|v| v.as_i64());

        let prefix = build_message_prefix(intent, thread, event_id, msg);
        let sender_display = instances::get_display_name(db, from);

        let others = msg
            .get("delivered_to")
            .and_then(|v| v.as_array())
            .map(|a| a.len().saturating_sub(1))
            .unwrap_or(0);
        let recipient = if others > 0 {
            let plural = if others > 1 { "s" } else { "" };
            format!("{recipient_display} (+{others} other{plural})")
        } else {
            recipient_display
        };

        format!("{prefix} {sender_display} → {recipient}: {text}")
    } else {
        let parts: Vec<String> = messages
            .iter()
            .map(|msg| {
                let from = msg
                    .get("from")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let text = msg.get("message").and_then(|v| v.as_str()).unwrap_or("");
                let intent = msg.get("intent").and_then(|v| v.as_str());
                let thread = msg.get("thread").and_then(|v| v.as_str());
                let event_id = msg.get("event_id").and_then(|v| v.as_i64());

                let prefix = build_message_prefix(intent, thread, event_id, msg);
                let sender_display = instances::get_display_name(db, from);

                let others = msg
                    .get("delivered_to")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len().saturating_sub(1))
                    .unwrap_or(0);
                let recipient = if others > 0 {
                    format!("{recipient_display} (+{others})")
                } else {
                    recipient_display.clone()
                };

                format!("{prefix} {sender_display} → {recipient}: {text}")
            })
            .collect();

        format!("[{} new messages] | {}", parts.len(), parts.join(" | "))
    }
}

/// Simple format for hook-style messages from `Message` structs (no ANSI colors).
///
/// Used by `maybe_deliver_pending_messages` which works with `db::Message` directly.
fn format_hook_messages_simple_from_msgs(
    db: &HcomDb,
    messages: &[crate::db::Message],
    instance_name: &str,
) -> String {
    if messages.is_empty() {
        return String::new();
    }

    let recipient_display = instances::get_display_name(db, instance_name);

    if messages.len() == 1 {
        let msg = &messages[0];
        let prefix = build_message_prefix(
            msg.intent.as_deref(),
            msg.thread.as_deref(),
            msg.event_id,
            &serde_json::json!({}),
        );
        let sender_display = instances::get_display_name(db, &msg.from);

        let others = msg
            .delivered_to
            .as_ref()
            .map(|a| a.len().saturating_sub(1))
            .unwrap_or(0);
        let recipient = if others > 0 {
            let plural = if others > 1 { "s" } else { "" };
            format!("{recipient_display} (+{others} other{plural})")
        } else {
            recipient_display
        };

        format!("{prefix} {sender_display} → {recipient}: {}", msg.text)
    } else {
        let parts: Vec<String> = messages
            .iter()
            .map(|msg| {
                let prefix = build_message_prefix(
                    msg.intent.as_deref(),
                    msg.thread.as_deref(),
                    msg.event_id,
                    &serde_json::json!({}),
                );
                let sender_display = instances::get_display_name(db, &msg.from);

                let others = msg
                    .delivered_to
                    .as_ref()
                    .map(|a| a.len().saturating_sub(1))
                    .unwrap_or(0);
                let recipient = if others > 0 {
                    format!("{recipient_display} (+{others})")
                } else {
                    recipient_display.clone()
                };

                format!("{prefix} {sender_display} → {recipient}: {}", msg.text)
            })
            .collect();

        format!("[{} new messages] | {}", parts.len(), parts.join(" | "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_db() -> (HcomDb, tempfile::TempDir) {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        (db, dir)
    }

    fn insert_instance(db: &HcomDb, name: &str, tool: &str) {
        let now = chrono::Utc::now().timestamp() as f64;
        db.conn()
            .execute(
                "INSERT INTO instances (name, status, created_at, tool) VALUES (?1, 'active', ?2, ?3)",
                rusqlite::params![name, now, tool],
            )
            .unwrap();
    }

    fn insert_process_binding(db: &HcomDb, process_id: &str, instance_name: &str) {
        let now = chrono::Utc::now().timestamp() as f64;
        db.conn()
            .execute(
                "INSERT INTO process_bindings (process_id, instance_name, updated_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![process_id, instance_name, now],
            )
            .unwrap();
    }

    // ── build_ctx_for_command tests ──

    #[test]
    fn test_build_ctx_no_identity() {
        let (db, _dir) = make_test_db();
        let ctx = build_ctx_for_command(&db, Some("list"), None, false, None, None).unwrap();
        assert!(ctx.identity.is_none());
        assert!(ctx.explicit_name.is_none());
        assert!(!ctx.go);
    }

    #[test]
    fn test_build_ctx_with_name() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", "claude");
        let ctx =
            build_ctx_for_command(&db, Some("send"), Some("luna"), false, None, None).unwrap();
        assert!(ctx.identity.is_some());
        assert_eq!(ctx.identity.as_ref().unwrap().name, "luna");
        assert_eq!(ctx.explicit_name.as_deref(), Some("luna"));
    }

    #[test]
    fn test_build_ctx_start_skips_name_resolution() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", "claude");
        let ctx =
            build_ctx_for_command(&db, Some("start"), Some("luna"), false, None, None).unwrap();
        // start skips name resolution
        assert!(ctx.identity.is_none());
        assert_eq!(ctx.explicit_name.as_deref(), Some("luna"));
    }

    #[test]
    fn test_build_ctx_with_process_id() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", "claude");
        insert_process_binding(&db, "pid-1", "luna");
        let ctx =
            build_ctx_for_command(&db, Some("send"), None, false, Some("pid-1"), None).unwrap();
        assert!(ctx.identity.is_some());
        assert_eq!(ctx.identity.as_ref().unwrap().name, "luna");
    }

    #[test]
    fn test_build_ctx_go_flag() {
        let (db, _dir) = make_test_db();
        let ctx = build_ctx_for_command(&db, Some("stop"), None, true, None, None).unwrap();
        assert!(ctx.go);
    }

    #[test]
    fn test_build_ctx_invalid_name_returns_error() {
        let (db, _dir) = make_test_db();
        // "garbage" doesn't exist — explicit --name must propagate error
        let result = build_ctx_for_command(&db, Some("send"), Some("garbage"), false, None, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_build_ctx_invalid_name_not_swallowed_by_gate() {
        let (db, _dir) = make_test_db();
        // Explicit --name that fails resolution → error before gate is reached
        let result = build_ctx_for_command(&db, Some("send"), Some("garbage"), false, None, None);
        assert!(result.is_err());
        // Gate should never see this case because build_ctx_for_command fails first
    }

    // ── check_identity_gate tests ──

    #[test]
    fn test_gate_non_gated_command() {
        let ctx = CommandContext {
            explicit_name: None,
            identity: None,
            go: false,
        };
        assert!(check_identity_gate("list", &ctx, false, false).is_ok());
    }

    #[test]
    fn test_gate_with_name() {
        let ctx = CommandContext {
            explicit_name: Some("luna".to_string()),
            identity: None,
            go: false,
        };
        assert!(check_identity_gate("send", &ctx, false, false).is_ok());
    }

    #[test]
    fn test_gate_send_with_from() {
        let ctx = CommandContext {
            explicit_name: None,
            identity: None,
            go: false,
        };
        assert!(check_identity_gate("send", &ctx, true, false).is_ok());
    }

    #[test]
    fn test_gate_send_no_identity() {
        let ctx = CommandContext {
            explicit_name: None,
            identity: None,
            go: false,
        };
        let err = check_identity_gate("send", &ctx, false, false).unwrap_err();
        assert!(err.contains("identity not found"));
    }

    #[test]
    fn test_gate_with_participant_identity() {
        let ctx = CommandContext {
            explicit_name: None,
            identity: Some(SenderIdentity {
                kind: SenderKind::Instance,
                name: "luna".into(),
                instance_data: Some(serde_json::json!({"tool": "claude"})),
                session_id: None,
                project: None,
            }),
            go: false,
        };
        assert!(check_identity_gate("send", &ctx, false, false).is_ok());
    }

    #[test]
    fn test_gate_listen_no_identity_inside_ai_tool() {
        let ctx = CommandContext {
            explicit_name: None,
            identity: None,
            go: false,
        };
        let err = check_identity_gate("listen", &ctx, false, true).unwrap_err();
        assert!(err.contains("start --as"));
    }

    // ── set_hookless_command_status tests ──

    #[test]
    fn test_hookless_status_skip_commands() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", "codex");
        let ctx = CommandContext {
            explicit_name: None,
            identity: Some(SenderIdentity {
                kind: SenderKind::Instance,
                name: "luna".into(),
                instance_data: Some(serde_json::json!({"tool": "codex"})),
                session_id: None,
                project: None,
            }),
            go: false,
        };
        // listen is in skip list — should not change status
        set_hookless_command_status(&db, "listen", &ctx);
        let data = db.get_instance_full("luna").unwrap().unwrap();
        // Status should remain the original (active from INSERT)
        assert_eq!(data.status, "active");
    }

    #[test]
    fn test_hookless_status_codex() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", "codex");
        let ctx = CommandContext {
            explicit_name: None,
            identity: Some(SenderIdentity {
                kind: SenderKind::Instance,
                name: "luna".into(),
                instance_data: Some(serde_json::json!({"tool": "codex"})),
                session_id: None,
                project: None,
            }),
            go: false,
        };
        // listen is in skip list — should not change status
        set_hookless_command_status(&db, "listen", &ctx);
        let data = db.get_instance_full("luna").unwrap().unwrap();
        // Status should remain the original (active from INSERT)
        assert_eq!(data.status, "active");
    }

    #[test]
    fn test_hookless_status_adhoc() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", "adhoc");
        let ctx = CommandContext {
            explicit_name: None,
            identity: Some(SenderIdentity {
                kind: SenderKind::Instance,
                name: "luna".into(),
                instance_data: Some(serde_json::json!({"tool": "adhoc"})),
                session_id: None,
                project: None,
            }),
            go: false,
        };
        set_hookless_command_status(&db, "events", &ctx);
        let data = db.get_instance_full("luna").unwrap().unwrap();
        assert_eq!(data.status, ST_INACTIVE);
        assert_eq!(data.status_context, "tool:events");
    }

    #[test]
    fn test_hookless_status_claude_main_skipped() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", "claude");
        let ctx = CommandContext {
            explicit_name: None,
            identity: Some(SenderIdentity {
                kind: SenderKind::Instance,
                name: "luna".into(),
                instance_data: Some(serde_json::json!({"tool": "claude"})),
                session_id: None,
                project: None,
            }),
            go: false,
        };
        set_hookless_command_status(&db, "send", &ctx);
        let data = db.get_instance_full("luna").unwrap().unwrap();
        // Claude main has hooks — should NOT be changed
        assert_eq!(data.status, "active"); // unchanged from INSERT
    }

    #[test]
    fn test_hookless_status_subagent() {
        let (db, _dir) = make_test_db();
        let now = chrono::Utc::now().timestamp() as f64;
        db.conn()
            .execute(
                "INSERT INTO instances (name, status, created_at, tool, parent_name) VALUES ('sub1', 'active', ?1, 'claude', 'luna')",
                rusqlite::params![now],
            )
            .unwrap();
        let ctx = CommandContext {
            explicit_name: None,
            identity: Some(SenderIdentity {
                kind: SenderKind::Instance,
                name: "sub1".into(),
                instance_data: Some(serde_json::json!({"tool": "claude", "parent_name": "luna"})),
                session_id: None,
                project: None,
            }),
            go: false,
        };
        set_hookless_command_status(&db, "send", &ctx);
        let data = db.get_instance_full("sub1").unwrap().unwrap();
        assert_eq!(data.status, ST_ACTIVE);
        assert_eq!(data.status_context, "tool:send");
    }

    // ── build_message_prefix tests ──

    #[test]
    fn test_prefix_intent_and_thread() {
        let prefix = build_message_prefix(
            Some("request"),
            Some("pr-42"),
            Some(42),
            &serde_json::json!({}),
        );
        assert_eq!(prefix, "[request:pr-42 #42]");
    }

    #[test]
    fn test_prefix_intent_only() {
        let prefix = build_message_prefix(Some("inform"), None, Some(10), &serde_json::json!({}));
        assert_eq!(prefix, "[inform #10]");
    }

    #[test]
    fn test_prefix_thread_only() {
        let prefix = build_message_prefix(None, Some("bugfix"), Some(5), &serde_json::json!({}));
        assert_eq!(prefix, "[thread:bugfix #5]");
    }

    #[test]
    fn test_prefix_no_envelope() {
        let prefix = build_message_prefix(None, None, Some(1), &serde_json::json!({}));
        assert_eq!(prefix, "[new message #1]");
    }

    #[test]
    fn test_prefix_no_id() {
        let prefix = build_message_prefix(Some("ack"), None, None, &serde_json::json!({}));
        assert_eq!(prefix, "[ack]");
    }

    #[test]
    fn test_prefix_remote_relay() {
        let msg = serde_json::json!({
            "_relay": {"id": 99, "short": "BOXE"}
        });
        let prefix = build_message_prefix(Some("request"), None, Some(42), &msg);
        assert_eq!(prefix, "[request #99:BOXE]");
    }

    // ── format_messages_human tests ──

    #[test]
    fn test_format_human_single_message() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", "claude");
        insert_instance(&db, "nova", "claude");
        let messages = vec![serde_json::json!({
            "from": "luna",
            "message": "hello world",
            "event_id": 42,
        })];
        let result = format_messages_human(&db, &messages, "nova");
        assert!(result.contains("luna"));
        assert!(result.contains("nova"));
        assert!(result.contains("hello world"));
        assert!(result.contains("#42"));
    }

    #[test]
    fn test_format_human_multiple_messages() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", "claude");
        insert_instance(&db, "nova", "claude");
        let messages = vec![
            serde_json::json!({
                "from": "luna",
                "message": "msg1",
                "event_id": 1,
            }),
            serde_json::json!({
                "from": "nova",
                "message": "msg2",
                "event_id": 2,
            }),
        ];
        let result = format_messages_human(&db, &messages, "luna");
        assert!(result.contains("2 messages"));
        assert!(result.contains("msg1"));
        assert!(result.contains("msg2"));
    }

    // ── format_hook_messages_simple tests ──

    #[test]
    fn test_format_simple_single() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", "claude");
        let messages = vec![serde_json::json!({
            "from": "luna",
            "message": "test",
            "intent": "request",
            "event_id": 1,
        })];
        let result = format_hook_messages_simple(&db, &messages, "luna");
        assert!(result.contains("[request #1]"));
        assert!(result.contains("test"));
    }

    #[test]
    fn test_format_simple_multiple() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", "claude");
        let messages = vec![
            serde_json::json!({"from": "luna", "message": "a", "event_id": 1}),
            serde_json::json!({"from": "luna", "message": "b", "event_id": 2}),
        ];
        let result = format_hook_messages_simple(&db, &messages, "luna");
        assert!(result.starts_with("[2 new messages]"));
        assert!(result.contains("a"));
        assert!(result.contains("b"));
    }

    // ── maybe_deliver_pending_messages tests ──

    #[test]
    fn test_deliver_skips_json_flag() {
        let (db, _dir) = make_test_db();
        let ctx = CommandContext {
            explicit_name: None,
            identity: Some(SenderIdentity {
                kind: SenderKind::Instance,
                name: "luna".into(),
                instance_data: Some(serde_json::json!({"tool": "codex"})),
                session_id: None,
                project: None,
            }),
            go: false,
        };
        assert!(maybe_deliver_pending_messages(&db, &ctx, true).is_none());
    }

    #[test]
    fn test_deliver_skips_non_codex_adhoc() {
        let (db, _dir) = make_test_db();
        let ctx = CommandContext {
            explicit_name: None,
            identity: Some(SenderIdentity {
                kind: SenderKind::Instance,
                name: "luna".into(),
                instance_data: Some(serde_json::json!({"tool": "claude"})),
                session_id: None,
                project: None,
            }),
            go: false,
        };
        assert!(maybe_deliver_pending_messages(&db, &ctx, false).is_none());
    }

    #[test]
    fn test_deliver_skips_no_identity() {
        let (db, _dir) = make_test_db();
        let ctx = CommandContext {
            explicit_name: None,
            identity: None,
            go: false,
        };
        assert!(maybe_deliver_pending_messages(&db, &ctx, false).is_none());
    }
}
