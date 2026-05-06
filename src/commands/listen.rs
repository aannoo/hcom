//! `hcom listen` command — block and receive messages.
//!
//!
//! Supports: message-wait mode, --timeout, --json, --sql filter mode.
//! Uses TCP notify socket for instant wake on local messages.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::core::filters::{EventFilterArgs, build_sql_from_flags, resolve_filter_names};
use crate::db::HcomDb;
use crate::identity;
use crate::instance_lifecycle::{StatusUpdate, set_status};
use crate::instances::{self, get_display_name};
use crate::notify::NotifyServer;
use crate::shared::{CommandContext, ST_ACTIVE, ST_INACTIVE, ST_LISTENING};

/// Parsed arguments for `hcom listen`.
#[derive(clap::Parser, Debug)]
#[command(name = "listen", about = "Wait for events matching filters")]
pub struct ListenArgs {
    /// Timeout in seconds (positional shorthand)
    pub timeout_positional: Option<u64>,
    /// Timeout in seconds (default: 86400 = 24h)
    #[arg(long)]
    pub timeout: Option<u64>,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// SQL WHERE filter
    #[arg(long)]
    pub sql: Option<String>,
    /// Composable event filters
    #[command(flatten)]
    pub filters: EventFilterArgs,
}

// Filter parsing, SQL generation, and expansion are imported from crate::core::filters

/// Initialize heartbeat for the listening instance.
/// Writes last_stop + wait_timeout to instances table
fn init_heartbeat(db: &HcomDb, instance_name: &str, timeout: f64) {
    let now = crate::shared::time::now_epoch_i64();

    let mut updates = serde_json::Map::new();
    updates.insert("last_stop".into(), serde_json::json!(now));
    updates.insert("wait_timeout".into(), serde_json::json!(timeout as i64));
    instances::update_instance_position(db, instance_name, &updates);
}

/// Update heartbeat timestamp.
/// Writes last_stop to instances table so stale-cleanup sees the agent as alive.
fn update_heartbeat(db: &HcomDb, instance_name: &str) {
    let now = crate::shared::time::now_epoch_i64();

    let mut updates = serde_json::Map::new();
    updates.insert("last_stop".into(), serde_json::json!(now));
    instances::update_instance_position(db, instance_name, &updates);
}

/// Format messages as JSON for model consumption.
fn format_messages_json(
    db: &HcomDb,
    messages: &[crate::db::Message],
    instance_name: &str,
) -> String {
    let recipient_display = get_display_name(db, instance_name);

    if messages.len() == 1 {
        let msg = &messages[0];
        let sender_display = get_display_name(db, &msg.from);
        let prefix = build_prefix(msg.intent.as_deref(), msg.thread.as_deref(), msg.event_id);
        format!(
            "{prefix} {sender_display} -> {recipient_display}: {}",
            msg.text
        )
    } else {
        let parts: Vec<String> = messages
            .iter()
            .map(|msg| {
                let sender_display = get_display_name(db, &msg.from);
                let prefix =
                    build_prefix(msg.intent.as_deref(), msg.thread.as_deref(), msg.event_id);
                format!(
                    "{prefix} {sender_display} -> {recipient_display}: {}",
                    msg.text
                )
            })
            .collect();
        format!("[{} new messages] | {}", parts.len(), parts.join(" | "))
    }
}

fn build_prefix(intent: Option<&str>, thread: Option<&str>, event_id: Option<i64>) -> String {
    let id_ref = event_id.map(|id| format!("#{id}")).unwrap_or_default();
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

/// Main entry point for `hcom listen` command.
///
/// Returns exit code (0 = success, 1 = error, 130 = interrupted).
pub fn cmd_listen(db: &HcomDb, args: &ListenArgs, ctx: Option<&CommandContext>) -> i32 {
    let explicit_name = ctx.and_then(|c| c.explicit_name.as_deref());

    // Resolve identity
    let resolve_result = if let Some(c) = ctx {
        if let Some(ref id) = c.identity {
            Ok((id.clone(), id.name.clone()))
        } else {
            let name = explicit_name.or(c.explicit_name.as_deref());
            match identity::resolve_identity(db, name, None, None, None, None, None) {
                Ok(id) => {
                    let n = id.name.clone();
                    Ok((id, n))
                }
                Err(e) => Err(e),
            }
        }
    } else {
        match identity::resolve_identity(db, explicit_name, None, None, None, None, None) {
            Ok(id) => {
                let n = id.name.clone();
                Ok((id, n))
            }
            Err(e) => Err(e),
        }
    };
    let (identity, instance_name) = match resolve_result {
        Ok(r) => r,
        Err(e) => {
            if explicit_name.is_some() {
                eprintln!("Error: {e}");
            } else {
                eprintln!("Error: --name required (no identity context)");
                eprintln!("Usage: hcom listen --name <name> [--timeout N]");
            }
            return 1;
        }
    };

    // Resolve timeout: --timeout flag > positional > default (24h)
    let mut timeout: f64 = if let Some(t) = args.timeout {
        t as f64
    } else if let Some(t) = args.timeout_positional {
        t as f64
    } else {
        86400.0
    };

    // Quick check mode
    if timeout <= 1.0 {
        timeout = 0.1;
    }

    let json_output = args.json;

    // Convert clap filter args to FilterMap
    let mut filters = args.filters.to_filter_map();
    resolve_filter_names(&mut filters, db);

    // Combine filters and --sql (both work together, ANDed)
    let combined_sql = {
        let mut sql_parts = Vec::new();

        if !filters.is_empty() {
            match build_sql_from_flags(&filters) {
                Ok(flag_sql) if !flag_sql.is_empty() => {
                    sql_parts.push(format!("({flag_sql})"));
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    return 1;
                }
                _ => {}
            }
        }

        if let Some(ref sql) = args.sql {
            sql_parts.push(format!("({sql})"));
        }

        if sql_parts.is_empty() {
            None
        } else {
            Some(sql_parts.join(" AND "))
        }
    };

    let instance_data = identity.instance_data.as_ref();
    if instance_data.is_none() {
        eprintln!("Error: hcom not started for '{instance_name}'.");
        return 1;
    }

    // Branch: SQL filter mode (combined from flags + --sql)
    if let Some(ref filter) = combined_sql {
        // Setup SIGTERM handler for filter mode
        let shutdown = Arc::new(AtomicBool::new(false));
        {
            let shutdown_flag = Arc::clone(&shutdown);
            let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, shutdown_flag);
        }
        return listen_with_filter(
            db,
            filter,
            &instance_name,
            timeout,
            json_output,
            instance_data.unwrap(),
            &shutdown,
        );
    }

    // Standard message-wait mode
    // Mark as listening
    set_status(
        db,
        &instance_name,
        ST_LISTENING,
        "ready",
        StatusUpdate {
            detail: "cmd:listen",
            ..Default::default()
        },
    );

    let start_time = std::time::Instant::now();

    // Setup TCP notify server
    let notify_server = NotifyServer::new().ok();
    let notify_port = notify_server.as_ref().map(|s| s.port());

    // Register notify endpoint
    if let Some(port) = notify_port {
        let _ = db.upsert_notify_endpoint(&instance_name, "listen", port);
    }

    init_heartbeat(db, &instance_name, timeout);

    // Setup SIGTERM handler for clean shutdown
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown_flag = Arc::clone(&shutdown);
        let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, shutdown_flag);
    }

    // Check if already disconnected
    if db
        .get_instance_full(&instance_name)
        .ok()
        .flatten()
        .is_none()
    {
        eprintln!("[You have been disconnected from HCOM]");
        return 0;
    }

    if !json_output {
        let display = get_display_name(db, &instance_name);
        eprintln!("[Listening for messages to {display}. Timeout: {timeout}s]");
    }

    let result = listen_loop(
        db,
        &instance_name,
        timeout,
        json_output,
        instance_data.unwrap(),
        start_time,
        notify_server.as_ref(),
        &shutdown,
    );

    // Cleanup: clear cmd:listen detail if still set
    if let Ok(Some(current)) = db.get_instance_full(&instance_name) {
        if current.status_detail == "cmd:listen" {
            set_status(
                db,
                &instance_name,
                ST_LISTENING,
                "ready",
                Default::default(),
            );
        }
    }

    // Cleanup notify endpoint
    let _ = db.delete_notify_endpoint(&instance_name, "listen");

    result
}

#[allow(clippy::too_many_arguments)]
fn listen_loop(
    db: &HcomDb,
    instance_name: &str,
    timeout: f64,
    json_output: bool,
    instance_data: &serde_json::Value,
    start_time: std::time::Instant,
    notify_server: Option<&NotifyServer>,
    shutdown: &AtomicBool,
) -> i32 {
    loop {
        // Check for SIGTERM
        if shutdown.load(Ordering::Relaxed) {
            if !json_output {
                eprintln!("\n[SIGTERM received, shutting down]");
            }
            return 130;
        }

        let elapsed = start_time.elapsed().as_secs_f64();
        if elapsed >= timeout {
            // Timeout
            if instance_data.get("tool").and_then(|v| v.as_str()) == Some("adhoc") {
                set_status(
                    db,
                    instance_name,
                    ST_INACTIVE,
                    "exit:timeout",
                    Default::default(),
                );
            }
            if !json_output {
                eprintln!("\n[Timeout: no messages after {timeout}s]");
            }
            return 0;
        }

        // Check if instance was stopped externally
        if db.get_instance_full(instance_name).ok().flatten().is_none() {
            if !json_output {
                eprintln!(
                    "\n[Disconnected: HCOM stopped for {instance_name}. Unless told otherwise, stop work and end your turn now]"
                );
            }
            return 0;
        }

        // Check for unread messages
        let messages = db.get_unread_messages(instance_name);
        if !messages.is_empty() {
            // Advance cursor
            if let Some(last) = messages.last() {
                if let Some(id) = last.event_id {
                    let mut updates = serde_json::Map::new();
                    updates.insert("last_event_id".into(), serde_json::json!(id));
                    instances::update_instance_position(db, instance_name, &updates);
                }
            }

            // Set status based on tool type
            let tool = instance_data
                .get("tool")
                .and_then(|v| v.as_str())
                .unwrap_or("claude");
            if tool == "adhoc" {
                set_status(
                    db,
                    instance_name,
                    ST_INACTIVE,
                    "message received",
                    Default::default(),
                );
            } else {
                set_status(
                    db,
                    instance_name,
                    ST_ACTIVE,
                    "finished listening",
                    Default::default(),
                );
            }

            if json_output {
                for msg in &messages {
                    let j = serde_json::json!({
                        "from": msg.from,
                        "text": msg.text,
                    });
                    println!("{}", serde_json::to_string(&j).unwrap_or_default());
                }
            } else {
                let formatted = format_messages_json(db, &messages, instance_name);
                println!("\n{formatted}");
            }
            return 0;
        }

        // Update heartbeat
        update_heartbeat(db, instance_name);

        // Wait for notification or short poll
        let remaining = timeout - elapsed;
        if remaining <= 0.0 {
            continue;
        }

        // TCP select for local notifications. Relay imports (pull.rs) call
        // `crate::notify::wake_all` after every batch, so the TCP wake fires
        // as soon as remote events land — no separate relay polling needed.
        let wait_time = if notify_server.is_some() {
            remaining.min(30.0)
        } else {
            remaining.min(0.1)
        };

        if let Some(server) = notify_server {
            server.wait(Duration::from_secs_f64(wait_time));
        } else {
            std::thread::sleep(Duration::from_secs_f64(wait_time));
        }
    }
}

/// Listen with SQL filter — uses temp subscription.
fn listen_with_filter(
    db: &HcomDb,
    sql_filter: &str,
    instance_name: &str,
    timeout: f64,
    json_output: bool,
    instance_data: &serde_json::Value,
    shutdown: &AtomicBool,
) -> i32 {
    // Validate SQL syntax (use events_v view for computed columns)
    let test_query = format!("SELECT 1 FROM events_v WHERE ({sql_filter}) LIMIT 0");
    if let Err(e) = db.conn().execute_batch(&test_query) {
        eprintln!("Invalid SQL filter: {e}");
        return 1;
    }

    // Check for recent match (10s lookback)
    let now_ts = crate::shared::time::now_epoch_f64();
    let lookback_ts = chrono::DateTime::from_timestamp((now_ts - 10.0) as i64, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
        .unwrap_or_default();

    let recent_query = format!(
        "SELECT id, type, instance, data FROM events_v WHERE timestamp > ? AND ({sql_filter}) ORDER BY id DESC LIMIT 1"
    );
    if let Ok(mut stmt) = db.conn().prepare(&recent_query) {
        if let Ok(row) = stmt.query_row(rusqlite::params![lookback_ts], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        }) {
            if json_output {
                let data: serde_json::Value = serde_json::from_str(&row.3).unwrap_or_default();
                let j = serde_json::json!({
                    "event_id": row.0,
                    "type": row.1,
                    "instance": row.2,
                    "data": data,
                });
                println!("{}", serde_json::to_string(&j).unwrap_or_default());
            } else {
                println!("[Match found] #{} {}:{}", row.0, row.1, row.2);
            }
            return 0;
        }
    }

    // Create temp subscription — SHA256 over instance+filter+time to avoid collisions
    let sub_id = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(format!("{instance_name}{sql_filter}{now_ts}").as_bytes());
        let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        format!("listen-{}", &hex[..6])
    };
    let sub_key = format!("events_sub:{sub_id}");

    // Mark as listening BEFORE capturing last_id
    set_status(
        db,
        instance_name,
        ST_LISTENING,
        &format!("filter:{sub_id}"),
        Default::default(),
    );

    let sub_data = serde_json::json!({
        "id": sub_id,
        "sql": sql_filter,
        "caller": instance_name,
        "once": true,
        "last_id": db.get_last_event_id(),
        "created": now_ts,
    });
    let _ = db.kv_set(&sub_key, Some(&sub_data.to_string()));

    // Setup notify
    let notify_server = NotifyServer::new().ok();
    if let Some(ref server) = notify_server {
        let _ = db.upsert_notify_endpoint(instance_name, "listen_filter", server.port());
    }

    init_heartbeat(db, instance_name, timeout);

    let start_time = std::time::Instant::now();

    if !json_output {
        eprintln!("[Listening for events matching filter. Timeout: {timeout}s]");
    }

    let result = filter_listen_loop(
        db,
        instance_name,
        &sub_id,
        timeout,
        json_output,
        instance_data,
        start_time,
        notify_server.as_ref(),
        shutdown,
    );

    // Cleanup
    let _ = db.kv_set(&sub_key, None);
    let _ = db.delete_notify_endpoint(instance_name, "listen_filter");

    result
}

#[allow(clippy::too_many_arguments)]
fn filter_listen_loop(
    db: &HcomDb,
    instance_name: &str,
    sub_id: &str,
    timeout: f64,
    json_output: bool,
    instance_data: &serde_json::Value,
    start_time: std::time::Instant,
    notify_server: Option<&NotifyServer>,
    shutdown: &AtomicBool,
) -> i32 {
    loop {
        // Check for SIGTERM
        if shutdown.load(Ordering::Relaxed) {
            if !json_output {
                eprintln!("\n[SIGTERM received, shutting down]");
            }
            return 130;
        }

        let elapsed = start_time.elapsed().as_secs_f64();
        if elapsed >= timeout {
            if !json_output {
                eprintln!("\n[Timeout: no match after {timeout}s]");
            }
            if instance_data.get("tool").and_then(|v| v.as_str()) == Some("adhoc") {
                set_status(
                    db,
                    instance_name,
                    ST_INACTIVE,
                    "exit:timeout",
                    Default::default(),
                );
            }
            return 0;
        }

        // Check if stopped
        if db.get_instance_full(instance_name).ok().flatten().is_none() {
            if !json_output {
                eprintln!("\n[Disconnected: HCOM stopped for {instance_name}]");
            }
            return 0;
        }

        // Check for messages (subscription notification or regular)
        let messages = db.get_unread_messages(instance_name);
        if !messages.is_empty() {
            // Advance cursor
            if let Some(last) = messages.last() {
                if let Some(id) = last.event_id {
                    let mut updates = serde_json::Map::new();
                    updates.insert("last_event_id".into(), serde_json::json!(id));
                    instances::update_instance_position(db, instance_name, &updates);
                }
            }

            // Check for subscription notification
            for msg in &messages {
                if msg.from == "[hcom-events]" && msg.text.contains(&format!("[sub:{sub_id}]")) {
                    if json_output {
                        let j = serde_json::json!({
                            "matched": true,
                            "notification": msg.text,
                        });
                        println!("{}", serde_json::to_string(&j).unwrap_or_default());
                    } else {
                        println!("\n{}", msg.text);
                    }
                    set_status(
                        db,
                        instance_name,
                        ST_ACTIVE,
                        "filter matched",
                        Default::default(),
                    );
                    return 0;
                }
            }

            // Other non-system messages
            let real_messages: Vec<&crate::db::Message> = messages
                .iter()
                .filter(|m| !m.from.starts_with('['))
                .collect();
            if !real_messages.is_empty() {
                if json_output {
                    for msg in &real_messages {
                        let j = serde_json::json!({
                            "from": msg.from,
                            "text": msg.text,
                        });
                        println!("{}", serde_json::to_string(&j).unwrap_or_default());
                    }
                } else {
                    let owned: Vec<crate::db::Message> =
                        real_messages.iter().map(|m| (*m).clone()).collect();
                    let formatted = format_messages_json(db, &owned, instance_name);
                    println!("\n{formatted}");
                }
                set_status(
                    db,
                    instance_name,
                    ST_ACTIVE,
                    "message received",
                    Default::default(),
                );
                return 0;
            }
        }

        update_heartbeat(db, instance_name);

        let remaining = timeout - elapsed;
        if remaining <= 0.0 {
            continue;
        }

        // TCP select for local notifications. Relay imports (pull.rs) call
        // `crate::notify::wake_all` after every batch, so the TCP wake fires
        // as soon as remote events land — no separate relay polling needed.
        let wait_time = if notify_server.is_some() {
            remaining.min(30.0)
        } else {
            remaining.min(0.1)
        };

        if let Some(server) = notify_server {
            server.wait(Duration::from_secs_f64(wait_time));
        } else {
            std::thread::sleep(Duration::from_secs_f64(wait_time));
        }
    }
}
