//! `hcom events` command — query events, manage subscriptions.
//!
//!
//! Modes:
//! - Query: `hcom events [--last N] [--all] [--full] [--wait SEC] [--sql EXPR] [filters...]`
//! - Subscribe: `hcom events sub [list | SQL | filters...] [--once] [--for name]`
//! - Unsubscribe: `hcom events unsub <id>`
//! - Launch status: `hcom events launch [batch_id] [--timeout N]`

use std::collections::HashMap;
use std::net::TcpListener;
use std::time::Duration;

use serde_json::{Value, json};

use crate::core::filters::{EventFilterArgs, build_sql_from_flags, resolve_filter_names};
use crate::core::launch_status::wait_for_launch;
use crate::db::HcomDb;
use crate::shared::CommandContext;

/// Parsed arguments for `hcom events`.
#[derive(clap::Parser, Debug)]
#[command(name = "events", about = "Query and subscribe to events")]
pub struct EventsArgs {
    /// Subcommand (sub, unsub, launch) or handled as query mode
    #[command(subcommand)]
    pub subcmd: Option<EventsSubcmd>,
    /// Limit count (default: 20)
    #[arg(long)]
    pub last: Option<usize>,
    /// Include archived sessions
    #[arg(long)]
    pub all: bool,
    /// Full output (not streamlined)
    #[arg(long)]
    pub full: bool,
    /// Block until match (default: 60s when flag present without value)
    #[arg(long, num_args(0..=1), default_missing_value = "60")]
    pub wait: Option<u64>,
    /// Raw SQL WHERE clause
    #[arg(long)]
    pub sql: Option<String>,
    /// Composable event filters
    #[command(flatten)]
    pub filters: EventFilterArgs,
}

#[derive(clap::Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum EventsSubcmd {
    /// Subscribe to events
    Sub(EventsSubArgs),
    /// Remove subscription
    Unsub(EventsUnsubArgs),
    /// Wait for launch to complete
    Launch(EventsLaunchArgs),
}

/// Args for `hcom events sub`.
#[derive(clap::Args, Debug)]
pub struct EventsSubArgs {
    /// Auto-remove after first match
    #[arg(long)]
    pub once: bool,
    /// Subscribe on behalf of another agent
    #[arg(long = "for")]
    pub for_agent: Option<String>,
    /// Composable event filters
    #[command(flatten)]
    pub filters: EventFilterArgs,
    /// SQL parts or "list" keyword
    pub rest: Vec<String>,
}

/// Args for `hcom events unsub`.
#[derive(clap::Args, Debug)]
pub struct EventsUnsubArgs {
    /// Subscription ID to remove
    pub id: String,
}

/// Args for `hcom events launch`.
#[derive(clap::Args, Debug)]
pub struct EventsLaunchArgs {
    /// Batch ID to wait for
    pub batch_id: Option<String>,
    /// Timeout in seconds (default: 30)
    #[arg(long, default_value = "30")]
    pub timeout: u64,
}

// ── Event Streamlining ──────────────────────────────────────────────────

/// Remove bloat fields from event for ~35% token reduction.
///
/// Preserves fields used in active filters.
pub fn streamline_event(event: &Value, filters: &HashMap<String, Vec<String>>) -> Value {
    let mut data = event.get("data").cloned().unwrap_or_else(|| json!({}));

    if let Some(obj) = data.as_object_mut() {
        // Drop universal bloat
        obj.remove("sender_kind");
        obj.remove("scope");
        obj.remove("delivered_to");
        if !filters.contains_key("mention") {
            obj.remove("mentions");
        }

        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match event_type {
            "message" => {
                obj.remove("reply_to");
            }
            "status" => {
                // Truncate detail unless --cmd or --file filter active
                if !filters.contains_key("cmd") && !filters.contains_key("file") {
                    if let Some(detail) = obj.get("detail").and_then(|v| v.as_str()) {
                        if detail.len() > 60 {
                            let end = (0..=60)
                                .rev()
                                .find(|&i| detail.is_char_boundary(i))
                                .unwrap_or(0);
                            let truncated = format!("{}...", &detail[..end]);
                            obj.insert("detail".into(), json!(truncated));
                        }
                    }
                }
                obj.remove("position");
            }
            "life" => {
                obj.remove("snapshot");
            }
            _ => {}
        }
    }

    // Truncate timestamp to 19 chars (remove microseconds)
    let ts = event.get("ts").and_then(|v| v.as_str()).unwrap_or("");
    let ts_truncated = if ts.len() > 19 { &ts[..19] } else { ts };

    json!({
        "id": event.get("id"),
        "ts": ts_truncated,
        "type": event.get("type"),
        "instance": event.get("instance"),
        "data": data,
    })
}

// ── Query events from DB ─────────────────────────────────────────────────

/// Query events from events_v view. Returns parsed event objects.
fn query_events(
    db: &HcomDb,
    filter_query: &str,
    last_n: usize,
    params: &[&dyn rusqlite::types::ToSql],
) -> Result<Vec<Value>, String> {
    let query =
        format!("SELECT * FROM events_v WHERE 1=1{filter_query} ORDER BY id DESC LIMIT {last_n}");

    let mut stmt = db
        .conn()
        .prepare(&query)
        .map_err(|e| format!("Error in SQL WHERE clause: {e}"))?;

    let rows = stmt
        .query_map(params, |row| {
            let id: i64 = row.get("id")?;
            let ts: String = row.get("timestamp")?;
            let etype: String = row.get("type")?;
            let instance: String = row.get("instance")?;
            let data_str: String = row.get("data")?;
            Ok((id, ts, etype, instance, data_str))
        })
        .map_err(|e| format!("Error in SQL WHERE clause: {e}"))?;

    let mut events = Vec::new();
    for row in rows {
        match row {
            Ok((id, ts, etype, instance, data_str)) => {
                let data: Value = serde_json::from_str(&data_str).unwrap_or(json!({}));
                events.push(json!({
                    "id": id,
                    "ts": ts,
                    "type": etype,
                    "instance": instance,
                    "data": data,
                }));
            }
            Err(e) => {
                eprintln!("Warning: Skipping corrupt event: {e}");
            }
        }
    }

    Ok(events)
}

// ── Subscription Management ──────────────────────────────────────────────

/// List all active event subscriptions.
fn events_sub_list(db: &HcomDb) -> i32 {
    let rows: Vec<(String, String)> = db
        .conn()
        .prepare("SELECT key, value FROM kv WHERE key LIKE 'events_sub:%'")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|r| r.ok())
            .collect()
        })
        .unwrap_or_default();

    if rows.is_empty() {
        println!("No active subscriptions");
        return 0;
    }

    let subs: Vec<Value> = rows
        .iter()
        .filter_map(|(_, v)| serde_json::from_str(v).ok())
        .collect();

    if subs.is_empty() {
        println!("No active subscriptions");
        return 0;
    }

    println!("{:<10} {:<12} {:<10} FILTER", "ID", "FOR", "MODE");
    for sub in &subs {
        let id = sub.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let caller = sub.get("caller").and_then(|v| v.as_str()).unwrap_or("");
        let mode = if sub.get("once").and_then(|v| v.as_bool()).unwrap_or(false) {
            "once"
        } else {
            "continuous"
        };

        let filter_display = if let Some(filters) = sub.get("filters") {
            let s = filters.to_string();
            if s.len() > 35 {
                {
                    let end = (0..=35).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
                    format!("{}...", &s[..end])
                }
            } else {
                s
            }
        } else {
            let sql = sub.get("sql").and_then(|v| v.as_str()).unwrap_or("");
            if sql.len() > 35 {
                {
                    let end = (0..=35)
                        .rev()
                        .find(|&i| sql.is_char_boundary(i))
                        .unwrap_or(0);
                    format!("{}...", &sql[..end])
                }
            } else {
                sql.to_string()
            }
        };

        println!("{id:<10} {caller:<12} {mode:<10} {filter_display}");
    }

    0
}

/// Show one-time tip for a command, tracked per-instance via kv.
/// Delegates to centralized core::tips module.
fn maybe_show_tip(db: &HcomDb, instance_name: &str, command: &str) {
    crate::core::tips::maybe_show_tip(db, instance_name, command, false);
}

/// Create a filter-based subscription.
fn events_sub_filter(
    db: &HcomDb,
    filters: &HashMap<String, Vec<String>>,
    sql_parts: &[String],
    caller: &str,
    once: bool,
) -> i32 {
    create_filter_subscription(db, filters, sql_parts, caller, once, false)
}

/// Core subscription creation logic. When `silent` is true, suppresses all stdout output.
/// Used by both the CLI `events sub` command and `auto_subscribe_defaults`.
pub(crate) fn create_filter_subscription(
    db: &HcomDb,
    filters: &HashMap<String, Vec<String>>,
    sql_parts: &[String],
    caller: &str,
    once: bool,
    silent: bool,
) -> i32 {
    // Build SQL from filters
    let mut sql = match build_sql_from_flags(filters) {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => {
            if !silent {
                eprintln!("Error: No valid filters provided");
            }
            return 1;
        }
        Err(e) => {
            if !silent {
                eprintln!("Error: Filter error: {e}");
            }
            return 1;
        }
    };

    // Validate and combine user-provided SQL parts
    if !sql_parts.is_empty() {
        let manual_sql = sql_parts.join(" ").replace("\\!", "!");
        if let Err(e) = db
            .conn()
            .execute(&format!("SELECT 1 FROM events_v WHERE ({manual_sql}) LIMIT 0"), [])
        {
            if !silent {
                eprintln!("Error: Invalid SQL: {e}");
            }
            return 1;
        }
        sql = format!("({sql}) AND ({manual_sql})");
    }

    // Collision self-relevance filtering
    if filters.contains_key("collision") {
        let caller_escaped = caller.replace('\'', "''");
        let self_relevance = format!(
            "(events_v.instance = '{caller_escaped}' OR EXISTS (SELECT 1 FROM events_v e2 WHERE e2.type = 'status' AND e2.status_context IN ('tool:Write', 'tool:Edit', 'tool:write_file', 'tool:edit_file') AND e2.status_detail = events_v.status_detail AND e2.instance = '{caller_escaped}' AND ABS(strftime('%s', events_v.timestamp) - strftime('%s', e2.timestamp)) < 20))"
        );
        sql = format!("({sql}) AND {self_relevance}");
    }

    // Generate subscription ID from SHA256 hash
    let id_source = format!(
        "{}:{}:{}:{}",
        caller,
        serde_json::to_string(filters).unwrap_or_default(),
        sql,
        once
    );
    let hash = sha256_hash(&id_source);
    let sub_id = format!("sub-{}", &hash[..8]);
    let sub_key = format!("events_sub:{sub_id}");

    // Check duplicate
    if db.kv_get(&sub_key).ok().flatten().is_some() {
        if !silent {
            println!("Subscription {sub_id} already exists");
        }
        return 0;
    }

    let now = crate::shared::time::now_epoch_f64();
    let last_id = db.get_last_event_id();

    let sub_data = json!({
        "id": sub_id,
        "caller": caller,
        "filters": filters,
        "sql": sql,
        "created": now,
        "last_id": last_id,
        "once": once,
    });

    let _ = db.kv_set(&sub_key, Some(&sub_data.to_string()));

    if !silent {
        println!("Subscription {sub_id} created");

        // Show historical match count
        if let Ok(count) = db.conn().query_row(
            &format!("SELECT COUNT(*) FROM events_v WHERE ({sql})"),
            [],
            |row| row.get::<_, i64>(0),
        ) {
            if count > 0 {
                println!("  historical matches: {count} events");
                println!("  You will be notified on the next matching event(s)");
            }
        }

        maybe_show_tip(db, caller, "sub:created");
    }

    0
}

/// Create a raw SQL subscription.
fn events_sub_sql(db: &HcomDb, sql_parts: &[String], caller: &str, once: bool) -> i32 {
    let sql = sql_parts.join(" ").replace("\\!", "!");

    // Validate SQL
    if let Err(e) = db
        .conn()
        .execute(&format!("SELECT 1 FROM events_v WHERE ({sql}) LIMIT 0"), [])
    {
        eprintln!("Invalid SQL: {e}");
        return 1;
    }

    // Generate ID (deterministic — deduplicates same SQL+caller+once mode)
    let hash = sha256_hash(&format!("{caller}{sql}{once}"));
    let sub_id = format!("sub-{}", &hash[..8]);
    let sub_key = format!("events_sub:{sub_id}");

    // Check duplicate
    if db.kv_get(&sub_key).ok().flatten().is_some() {
        println!("Subscription {sub_id} already exists");
        return 0;
    }

    let now = crate::shared::time::now_epoch_f64();
    let last_id = db.get_last_event_id();

    let sub_data = json!({
        "id": sub_id,
        "sql": sql,
        "caller": caller,
        "once": once,
        "last_id": last_id,
        "created": now,
    });

    let _ = db.kv_set(&sub_key, Some(&sub_data.to_string()));

    // Output
    println!("{sub_id}");
    println!("  for: {caller}");
    println!("  filter: {sql}");

    // Historical matches
    if let Ok(count) = db.conn().query_row(
        &format!("SELECT COUNT(*) FROM events_v WHERE ({sql})"),
        [],
        |row| row.get::<_, i64>(0),
    ) {
        if count > 0 {
            println!("  historical matches: {count} events");
            // Show latest match as example
            if let Ok(mut stmt) = db.conn().prepare(
                &format!("SELECT timestamp, type, instance FROM events_v WHERE ({sql}) ORDER BY id DESC LIMIT 1")
            ) {
                if let Ok(row) = stmt.query_row([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                }) {
                    let ts = if row.0.len() > 19 { &row.0[..19] } else { &row.0 };
                    println!("  latest match: [{}] {} @ {}", row.1, row.2, ts);
                }
            }
            println!("  You will be notified on the next matching event(s)");
        } else {
            println!("  historical matches: 0 (filter will apply to future events only)");
        }
    }

    maybe_show_tip(db, caller, "sub:created");

    0
}

/// Handle `hcom events sub` subcommand.
fn cmd_events_sub(db: &HcomDb, args: &EventsSubArgs, caller_name: Option<&str>) -> i32 {
    // Handle 'list' subcommand
    if args.rest.first().map(|s| s.as_str()) == Some("list") {
        return events_sub_list(db);
    }

    // Convert clap filter args to FilterMap
    let mut filters = args.filters.to_filter_map();
    resolve_filter_names(&mut filters, db);

    let once = args.once;
    let target_instance = args
        .for_agent
        .as_deref()
        .map(|name| crate::instances::resolve_display_name(db, name).unwrap_or_else(|| name.to_string()));
    let sql_parts: Vec<String> = args.rest.clone();

    // Resolve caller
    let caller = if let Some(target) = &target_instance {
        // Exact match first, then prefix fallback
        let exact: Option<String> = db
            .conn()
            .query_row(
                "SELECT name FROM instances WHERE name = ?",
                rusqlite::params![target],
                |row| row.get::<_, String>(0),
            )
            .ok();
        let resolved = exact.or_else(|| {
            db.conn()
                .query_row(
                    "SELECT name FROM instances WHERE name LIKE ? LIMIT 1",
                    rusqlite::params![format!("{target}%")],
                    |row| row.get::<_, String>(0),
                )
                .ok()
        });
        match resolved {
            Some(name) => name,
            None => {
                eprintln!("Not found: {target}");
                eprintln!("Use 'hcom list' to see available agents");
                return 1;
            }
        }
    } else if let Some(name) = caller_name {
        name.to_string()
    } else {
        match crate::identity::resolve_identity(db, None, None, None, None, None, None) {
            Ok(id) => id.name,
            Err(_) => {
                eprintln!("Error: Cannot create subscription without identity.");
                eprintln!("Run 'hcom start' first, or use --name.");
                return 1;
            }
        }
    };

    // Filter-based subscription
    if !filters.is_empty() {
        return events_sub_filter(db, &filters, &sql_parts, &caller, once);
    }

    // No filters and no SQL: show help
    if sql_parts.is_empty() {
        println!(
            "Event subscriptions: get notified via hcom message when a future event matches.\n\n\
             Usage:\n\
             \x20 events sub [filters] [--once]     Subscribe using filter flags\n\
             \x20 events sub \"SQL WHERE\" [--once]   Subscribe using raw SQL\n\
             \x20 events sub list                   List active subscriptions\n\
             \x20 events unsub <id>                 Remove a subscription\n\
             \x20   --once                          Auto-remove after first match\n\
             \x20   --for <name>                    Subscribe on behalf of another agent\n\n\
             Filters (same flag repeated = OR, different flags = AND):\n\
             \x20 --agent NAME                      Agent name\n\
             \x20 --type TYPE                       message | status | life\n\
             \x20 --status VAL                      listening | active | blocked\n\
             \x20 --context PATTERN                 tool:Bash | deliver:X (supports * wildcard)\n\
             \x20 --action VAL                      created | started | ready | stopped | batch_launched\n\
             \x20 --cmd PATTERN                     Shell command (contains, ^prefix, $suffix, =exact, *glob)\n\
             \x20 --file PATH                       File write (*.py for glob, file.py for contains)\n\
             \x20 --collision                        Two agents edit same file within 30s\n\
             \x20 --from NAME                       Sender\n\
             \x20 --mention NAME                    @mention target\n\
             \x20 --intent VAL                      request | inform | ack\n\
             \x20 --thread NAME                     Thread name\n\
             \x20 --after TIME                      After timestamp (ISO-8601)\n\
             \x20 --before TIME                     Before timestamp (ISO-8601)\n\
             \x20 Shortcuts: --idle NAME, --blocked NAME\n\n\
             Examples:\n\
             \x20 events sub --idle peso            Notified when peso goes idle\n\
             \x20 events sub --file '*.py' --once   One-shot: next .py file write\n\
             \x20 events sub --collision            File edit conflict detection"
        );
        return 0;
    }

    // SQL-based subscription
    events_sub_sql(db, &sql_parts, &caller, once)
}

/// Handle `hcom events unsub <id>`.
fn cmd_events_unsub(db: &HcomDb, args: &EventsUnsubArgs) -> i32 {
    let mut sub_id = args.id.clone();
    if !sub_id.starts_with("sub-") {
        sub_id = format!("sub-{sub_id}");
    }

    let key = format!("events_sub:{sub_id}");

    // Check exists
    if db.kv_get(&key).ok().flatten().is_none() {
        eprintln!("Not found: {sub_id}");
        eprintln!("Use 'hcom events sub list' to list active subscriptions.");
        return 1;
    }

    let _ = db.kv_set(&key, None);
    println!("Removed {sub_id}");
    0
}

/// Handle `hcom events launch [batch_id] [--timeout N]`.
fn cmd_events_launch(db: &HcomDb, args: &EventsLaunchArgs, instance_name: Option<&str>) -> i32 {
    let timeout = args.timeout;

    let batch_id = args.batch_id.as_deref();

    // Resolve launcher
    let launcher = instance_name.map(|s| s.to_string()).or_else(|| {
        if crate::shared::is_inside_ai_tool() {
            crate::identity::resolve_identity(db, None, None, None, None, None, None)
                .ok()
                .map(|id| id.name)
        } else {
            None
        }
    });

    let result = wait_for_launch(db, launcher.as_deref(), batch_id, timeout);
    let result_json = result.to_json();
    println!(
        "{}",
        serde_json::to_string(&result_json).unwrap_or_default()
    );

    if result_json.get("status").and_then(|v| v.as_str()) == Some("ready") {
        0
    } else {
        1
    }
}

// ── Wait Mode ────────────────────────────────────────────────────────────

/// Wait mode: block until matching event or timeout.
fn events_wait(
    db: &HcomDb,
    filter_query: &str,
    wait_timeout: u64,
    full_output: bool,
    filters: &HashMap<String, Vec<String>>,
    instance_name: Option<&str>,
) -> i32 {
    use std::time::Instant;

    // Quick lookback: check last 10s for already-matching events
    let now_ts = crate::shared::time::now_epoch_i64() - 10;
    let lookback_ts = chrono::DateTime::from_timestamp(now_ts, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default();

    let lookback_query = format!(
        "SELECT * FROM events_v WHERE timestamp > ?{filter_query} ORDER BY id DESC LIMIT 1"
    );
    if let Ok(mut stmt) = db.conn().prepare(&lookback_query) {
        if let Ok(mut rows) = stmt.query(rusqlite::params![lookback_ts]) {
            if let Ok(Some(row)) = rows.next() {
                if let Ok(event) = parse_event_row(row) {
                    let output = if full_output {
                        event.clone()
                    } else {
                        streamline_event(&event, filters)
                    };
                    println!("{}", serde_json::to_string(&output).unwrap_or_default());
                    return 0;
                }
            }
        }
    }

    // Setup TCP notify server for instant wake
    let notify_server = TcpListener::bind("127.0.0.1:0").ok();
    let notify_port = notify_server
        .as_ref()
        .and_then(|s| s.local_addr().ok())
        .map(|a| a.port());

    if let (Some(name), Some(port)) = (instance_name, notify_port) {
        if let Some(ref server) = notify_server {
            server.set_nonblocking(true).ok();
        }
        let _ = db.conn().execute(
            "INSERT OR REPLACE INTO notify_endpoints (instance_name, kind, port) VALUES (?1, 'events_wait', ?2)",
            rusqlite::params![name, port as i64],
        );
    }

    let start = Instant::now();
    let mut last_id = db.get_last_event_id();

    let result = loop {
        if start.elapsed() >= Duration::from_secs(wait_timeout) {
            println!("{}", json!({"timed_out": true}));
            break 1;
        }

        // Query for new matching events
        let query = format!("SELECT * FROM events_v WHERE id > ?{filter_query} ORDER BY id");
        let mut found = false;
        match db.conn().prepare(&query) {
            Ok(mut stmt) => {
                if let Ok(mut rows) = stmt.query(rusqlite::params![last_id]) {
                    while let Ok(Some(row)) = rows.next() {
                        // Always advance last_id regardless of parse success
                        if let Ok(id) = row.get::<_, i64>("id") {
                            last_id = id;
                        }
                        if let Ok(event) = parse_event_row(row) {
                            let output = if full_output {
                                event.clone()
                            } else {
                                streamline_event(&event, filters)
                            };
                            println!("{}", serde_json::to_string(&output).unwrap_or_default());
                            found = true;
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("Error in SQL WHERE clause: {e}");
                break 2;
            }
        }

        if found {
            break 0;
        }

        // Check for unread messages (interrupt wait) — use <hcom> XML tag format
        if let Some(name) = instance_name {
            let messages = db.get_unread_messages(name);
            if !messages.is_empty() {
                // Format as <hcom> XML tag
                let preview = build_message_preview(db, name);
                println!("{preview}");
                break 0;
            }
        }

        // Wait for TCP notification or timeout
        let remaining = wait_timeout.saturating_sub(start.elapsed().as_secs());
        if remaining == 0 {
            println!("{}", json!({"timed_out": true}));
            break 1;
        }

        // Short relay poll for remote events before TCP wait
        if crate::relay::is_relay_enabled(&crate::config::load_config_snapshot().core) {
            crate::relay::relay_wait((remaining as f64).min(2.0));
            continue; // Re-check for events after relay sync
        }

        if let Some(ref server) = notify_server {
            // Use poll-based wait (500ms intervals since TcpListener is non-blocking)
            let wait_time = std::cmp::min(remaining, 5);
            let poll_end = Instant::now() + Duration::from_secs(wait_time);
            while Instant::now() < poll_end {
                // Try accept (non-blocking)
                if let Ok((conn, _)) = server.accept() {
                    let _ = conn.shutdown(std::net::Shutdown::Both);
                    break; // Got notification, re-check events
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        } else {
            std::thread::sleep(Duration::from_millis(500));
        }
    };

    // Cleanup TCP notify endpoint
    if let (Some(name), Some(port)) = (instance_name, notify_port) {
        let _ = db.conn().execute(
            "DELETE FROM notify_endpoints WHERE instance_name = ?1 AND kind = 'events_wait' AND port = ?2",
            rusqlite::params![name, port as i64],
        );
    }

    result
}

/// Parse a row from events_v into a JSON value.
fn parse_event_row(row: &rusqlite::Row) -> Result<Value, rusqlite::Error> {
    let id: i64 = row.get("id")?;
    let ts: String = row.get("timestamp")?;
    let etype: String = row.get("type")?;
    let instance: String = row.get("instance")?;
    let data_str: String = row.get("data")?;
    let data: Value = serde_json::from_str(&data_str).unwrap_or(json!({}));
    Ok(json!({
        "id": id,
        "ts": ts,
        "type": etype,
        "instance": instance,
        "data": data,
    }))
}

/// SHA-256 hex hash
fn sha256_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

/// Build <hcom> XML message preview for unread notification.
fn build_message_preview(db: &HcomDb, instance_name: &str) -> String {
    let messages = db.get_unread_messages(instance_name);
    if messages.is_empty() {
        return "<hcom></hcom>".to_string();
    }

    // Build simple "sender → you" format
    let display_name = crate::instances::get_display_name(db, instance_name);
    let senders: Vec<String> = messages
        .iter()
        .map(|m| crate::instances::get_display_name(db, &m.from))
        .collect();

    // Deduplicate senders preserving order
    let mut seen = std::collections::HashSet::new();
    let unique_senders: Vec<&str> = senders
        .iter()
        .filter(|s| seen.insert(s.as_str()))
        .map(|s| s.as_str())
        .collect();

    let preview = if unique_senders.len() == 1 {
        format!("{} → {display_name}", unique_senders[0])
    } else {
        format!("{} → {display_name}", unique_senders.join(", "))
    };

    // Truncate if needed (max ~200 chars)
    let max_content = 200;
    if preview.len() > max_content {
        let end = (0..=(max_content - 3))
            .rev()
            .find(|&i| preview.is_char_boundary(i))
            .unwrap_or(0);
        format!("<hcom>{}...</hcom>", &preview[..end])
    } else {
        format!("<hcom>{preview}</hcom>")
    }
}

// ── Main Entry Point ─────────────────────────────────────────────────────

/// Main entry point for `hcom events` command.
pub fn cmd_events(db: &HcomDb, args: &EventsArgs, ctx: Option<&CommandContext>) -> i32 {
    // Resolve identity context
    let instance_name = ctx
        .and_then(|c| c.identity.as_ref())
        .filter(|id| matches!(id.kind, crate::shared::SenderKind::Instance))
        .map(|id| id.name.clone());
    let caller_name = instance_name.clone();

    // Handle subcommands
    if let Some(ref subcmd) = args.subcmd {
        match subcmd {
            EventsSubcmd::Launch(launch_args) => {
                return cmd_events_launch(db, launch_args, instance_name.as_deref());
            }
            EventsSubcmd::Sub(sub_args) => {
                return cmd_events_sub(db, sub_args, caller_name.as_deref());
            }
            EventsSubcmd::Unsub(unsub_args) => {
                return cmd_events_unsub(db, unsub_args);
            }
        }
    }

    // Query mode — use typed fields directly
    let search_all = args.all;
    let full_output = args.full;
    let last_n = args.last.unwrap_or(20);
    let sql_where = args.sql.as_ref().map(|s| s.replace("\\!", "!"));
    let wait_timeout = args.wait;

    // Convert clap filter args to FilterMap
    let mut filters = args.filters.to_filter_map();
    resolve_filter_names(&mut filters, db);

    // Build filter SQL
    let mut filter_query = String::new();

    if !filters.is_empty() {
        match build_sql_from_flags(&filters) {
            Ok(flag_sql) if !flag_sql.is_empty() => {
                filter_query.push_str(&format!(" AND ({flag_sql})"));
            }
            Err(e) => {
                eprintln!("Error: Filter error: {e}");
                return 1;
            }
            _ => {}
        }
    }

    // Add user SQL WHERE clause
    if let Some(ref sql) = sql_where {
        filter_query.push_str(&format!(" AND ({sql})"));
    }

    // Wait mode
    if let Some(timeout) = wait_timeout {
        return events_wait(
            db,
            &filter_query,
            timeout,
            full_output,
            &filters,
            instance_name.as_deref(),
        );
    }

    // Snapshot mode (default)
    let events = match query_events(db, &filter_query, last_n, &[]) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };

    // Optionally search archives
    let mut all_events = events;

    if search_all {
        // Mark current events
        for event in &mut all_events {
            if let Some(obj) = event.as_object_mut() {
                obj.insert("source".into(), json!("current"));
            }
        }

        // Search archives
        let archive_dir = crate::paths::hcom_dir().join("archive");
        if archive_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&archive_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    let db_path = path.join("hcom.db");
                    if !db_path.exists() {
                        continue;
                    }
                    let archive_name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("archive");

                    if let Ok(archive_db) = HcomDb::open_raw(&db_path) {
                        // Build archive query with same filters
                        let archive_filter = filter_query.clone();
                        let query = format!(
                            "SELECT * FROM events_v WHERE 1=1{archive_filter} ORDER BY id DESC LIMIT {last_n}"
                        );
                        if let Ok(mut stmt) = archive_db.conn().prepare(&query) {
                            if let Ok(rows) = stmt.query_map([], |row| {
                                let id: i64 = row.get("id")?;
                                let ts: String = row.get("timestamp")?;
                                let etype: String = row.get("type")?;
                                let instance: String = row.get("instance")?;
                                let data_str: String = row.get("data")?;
                                Ok((id, ts, etype, instance, data_str))
                            }) {
                                for row in rows.flatten() {
                                    let (id, ts, etype, instance, data_str) = row;
                                    let data: Value =
                                        serde_json::from_str(&data_str).unwrap_or(json!({}));
                                    all_events.push(json!({
                                        "id": id,
                                        "ts": ts,
                                        "type": etype,
                                        "instance": instance,
                                        "data": data,
                                        "source": archive_name,
                                    }));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Sort by timestamp and limit
    all_events.sort_by(|a, b| {
        let ts_a = a.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        let ts_b = b.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        ts_a.cmp(ts_b)
    });

    if all_events.len() > last_n {
        let start = all_events.len() - last_n;
        all_events = all_events[start..].to_vec();
    }

    // Output
    for event in &all_events {
        let output = if full_output {
            event.clone()
        } else {
            streamline_event(event, &filters)
        };
        println!("{}", serde_json::to_string(&output).unwrap_or_default());
    }

    0
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_streamline_event_message() {
        let event = json!({
            "id": 1,
            "ts": "2025-02-23T15:30:45.123456",
            "type": "message",
            "instance": "luna",
            "data": {
                "from": "nova",
                "text": "hello",
                "sender_kind": "instance",
                "scope": "mentions",
                "delivered_to": ["luna"],
                "mentions": ["luna"],
                "reply_to": "42",
                "reply_to_local": 42,
            }
        });

        let filters = HashMap::new();
        let result = streamline_event(&event, &filters);

        let data = result.get("data").unwrap();
        assert!(data.get("sender_kind").is_none());
        assert!(data.get("scope").is_none());
        assert!(data.get("delivered_to").is_none());
        assert!(data.get("mentions").is_none());
        assert!(data.get("reply_to").is_none());
        assert!(data.get("reply_to_local").is_some());
        assert_eq!(result.get("ts").unwrap().as_str().unwrap().len(), 19);
    }

    #[test]
    fn test_streamline_event_status() {
        let long_detail = "x".repeat(100);
        let event = json!({
            "id": 2,
            "ts": "2025-02-23T15:30:45",
            "type": "status",
            "instance": "luna",
            "data": {
                "detail": long_detail,
                "position": {"last_event_id": 42},
                "status": "active",
            }
        });

        let filters = HashMap::new();
        let result = streamline_event(&event, &filters);
        let data = result.get("data").unwrap();

        // Detail should be truncated
        let detail = data.get("detail").unwrap().as_str().unwrap();
        assert!(detail.len() <= 64); // 60 + "..."
        assert!(detail.ends_with("..."));

        // Position should be removed
        assert!(data.get("position").is_none());
    }

    #[test]
    fn test_streamline_event_status_with_cmd_filter() {
        let long_detail = "x".repeat(100);
        let event = json!({
            "id": 2,
            "ts": "2025-02-23T15:30:45",
            "type": "status",
            "instance": "luna",
            "data": {
                "detail": long_detail,
            }
        });

        let mut filters = HashMap::new();
        filters.insert("cmd".to_string(), vec!["git".to_string()]);
        let result = streamline_event(&event, &filters);
        let data = result.get("data").unwrap();

        // Detail should NOT be truncated when --cmd filter active
        let detail = data.get("detail").unwrap().as_str().unwrap();
        assert_eq!(detail.len(), 100);
    }

    #[test]
    fn test_streamline_event_life() {
        let event = json!({
            "id": 3,
            "ts": "2025-02-23T15:30:45",
            "type": "life",
            "instance": "luna",
            "data": {
                "action": "stopped",
                "snapshot": {"large": "nested", "object": true},
            }
        });

        let filters = HashMap::new();
        let result = streamline_event(&event, &filters);
        let data = result.get("data").unwrap();

        assert!(data.get("snapshot").is_none());
        assert!(data.get("action").is_some());
    }

    #[test]
    fn test_streamline_preserves_mentions_with_filter() {
        let event = json!({
            "id": 1,
            "ts": "2025-02-23T15:30:45",
            "type": "message",
            "instance": "luna",
            "data": {
                "mentions": ["luna", "nova"],
            }
        });

        let mut filters = HashMap::new();
        filters.insert("mention".to_string(), vec!["luna".to_string()]);
        let result = streamline_event(&event, &filters);
        let data = result.get("data").unwrap();

        assert!(data.get("mentions").is_some());
    }

    #[test]
    fn test_sha256_hash() {
        let h1 = sha256_hash("test input");
        let h2 = sha256_hash("test input");
        let h3 = sha256_hash("different input");
        assert_eq!(h1, h2); // deterministic
        assert_ne!(h1, h3); // different inputs
        assert_eq!(h1.len(), 64); // full SHA-256 hex
        // Verify known SHA-256 hash
        assert_eq!(&h1[..8], "9dfe6f15");
    }

    #[test]
    fn test_events_args_wait_with_value() {
        use clap::Parser;
        let args = EventsArgs::try_parse_from(["events", "--wait", "30", "--full"]).unwrap();
        assert_eq!(args.wait, Some(30));
        assert!(args.full);
    }

    #[test]
    fn test_events_args_wait_no_value() {
        use clap::Parser;
        let args = EventsArgs::try_parse_from(["events", "--wait", "--full"]).unwrap();
        assert_eq!(args.wait, Some(60)); // default_missing_value
        assert!(args.full);
    }

    #[test]
    fn test_events_args_no_wait() {
        use clap::Parser;
        let args = EventsArgs::try_parse_from(["events", "--full"]).unwrap();
        assert_eq!(args.wait, None);
        assert!(args.full);
    }

    #[test]
    fn test_events_args_last() {
        use clap::Parser;
        let args = EventsArgs::try_parse_from(["events", "--last", "50"]).unwrap();
        assert_eq!(args.last, Some(50));
    }

    #[test]
    fn test_events_args_with_filters() {
        use clap::Parser;
        let args =
            EventsArgs::try_parse_from(["events", "--agent", "peso", "--type", "message"]).unwrap();
        assert_eq!(args.filters.agent, vec!["peso"]);
        assert_eq!(args.filters.event_type, vec!["message"]);
        assert!(args.subcmd.is_none());
    }

    #[test]
    fn test_events_sub_args() {
        use clap::Parser;
        let args =
            EventsArgs::try_parse_from(["events", "sub", "--agent", "peso", "--once"]).unwrap();
        match args.subcmd {
            Some(EventsSubcmd::Sub(ref sub)) => {
                assert!(sub.once);
                assert_eq!(sub.filters.agent, vec!["peso"]);
            }
            _ => panic!("Expected Sub subcommand"),
        }
    }

    #[test]
    fn test_events_unsub_args() {
        use clap::Parser;
        let args = EventsArgs::try_parse_from(["events", "unsub", "sub-abc123"]).unwrap();
        match args.subcmd {
            Some(EventsSubcmd::Unsub(ref unsub)) => {
                assert_eq!(unsub.id, "sub-abc123");
            }
            _ => panic!("Expected Unsub subcommand"),
        }
    }

    #[test]
    fn test_events_launch_args() {
        use clap::Parser;
        let args =
            EventsArgs::try_parse_from(["events", "launch", "batch1", "--timeout", "60"]).unwrap();
        match args.subcmd {
            Some(EventsSubcmd::Launch(ref launch)) => {
                assert_eq!(launch.batch_id, Some("batch1".to_string()));
                assert_eq!(launch.timeout, 60);
            }
            _ => panic!("Expected Launch subcommand"),
        }
    }
}
