//! Shared hook functions — deliver, poll, bind, bootstrap, finalize.

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::params;
use serde_json::Value;

use crate::bootstrap;
use crate::db::{HcomDb, InstanceRow, Message};
use crate::instance_lifecycle as lifecycle;
use crate::instances;
use crate::log;
use crate::messages;
use crate::shared::constants::{BIND_MARKER_RE, MAX_MESSAGES_PER_DELIVERY};
use crate::shared::context::HcomContext;
use crate::shared::{ST_ACTIVE, ST_INACTIVE, ST_LISTENING};

/// Run a hook handler with panic safety.
///
/// Catches panics in the handler closure, logs them, and returns the fallback
/// value instead of crashing the host process. Used by all tool dispatchers.
pub(crate) fn dispatch_with_panic_guard<R>(
    tool: &str,
    hook_name: &str,
    fallback: R,
    f: impl FnOnce() -> R,
) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            log::log_error(
                "hooks",
                &format!("{tool}.dispatch.panic"),
                &format!("hook={hook_name}"),
            );
            fallback
        }
    }
}

/// Commands auto-approved in tool permission rules (Claude/Gemini/Codex settings).
///
/// Included: read-only queries, messaging, and session lifecycle commands that
/// agents need to run without user approval prompts.
/// Excluded: `stop`, `kill`, `run`, `reset` — these are destructive or
/// admin-level and require explicit user approval.
pub(crate) const SAFE_HCOM_COMMANDS: &[&str] = &[
    "send",
    "start",
    "help",
    "--help",
    "-h",
    "list",
    "events",
    "listen",
    "relay",
    "config",
    "transcript",
    "archive",
    "bundle",
    "status",
    "term",
    "hooks",
    "--version",
    "-v",
    "--new-terminal",
];

/// Pre-gate check: should hooks proceed?
///
///
/// - HCOM-launched (process_id or is_launched) → always proceed
/// - Otherwise: check if DB has any instances → if not, skip (exit 0, empty output)
///
/// This prevents outputting hints/errors when hcom is installed but not actively used.
pub fn hook_gate_check(ctx: &HcomContext, db: &HcomDb) -> bool {
    if ctx.process_id.is_some() || ctx.is_launched {
        return true;
    }
    // Check if any instances exist — distinguish "no rows" from DB error
    match db
        .conn()
        .query_row("SELECT 1 FROM instances LIMIT 1", [], |_| Ok(()))
    {
        Ok(()) => true,
        Err(rusqlite::Error::QueryReturnedNoRows) => false,
        Err(e) => {
            log::log_warn(
                "hooks",
                "gate.db_error",
                &format!("hook gate DB check failed: {e}, proceeding anyway"),
            );
            true // On DB error, proceed rather than silently disabling hooks
        }
    }
}

/// Convert a db::Message to a serde_json::Value object.
pub(crate) fn message_to_value(m: &Message) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("from".into(), Value::String(m.from.clone()));
    obj.insert("message".into(), Value::String(m.text.clone()));
    if let Some(ref intent) = m.intent {
        obj.insert("intent".into(), Value::String(intent.clone()));
    }
    if let Some(ref thread) = m.thread {
        obj.insert("thread".into(), Value::String(thread.clone()));
    }
    if let Some(id) = m.event_id {
        obj.insert("event_id".into(), serde_json::json!(id));
    }
    if let Some(ref ts) = m.timestamp {
        obj.insert("timestamp".into(), Value::String(ts.clone()));
    }
    if let Some(ref delivered_to) = m.delivered_to {
        obj.insert("delivered_to".into(), serde_json::json!(delivered_to));
    }
    if let Some(ref bundle_id) = m.bundle_id {
        obj.insert("bundle_id".into(), Value::String(bundle_id.clone()));
    }
    Value::Object(obj)
}

/// Load config hints string (from instance-level or global config).
/// Call once per hook invocation and pass to format functions.
pub(crate) fn load_config_hints() -> String {
    crate::config::HcomConfig::load(None)
        .map(|c| c.hints.clone())
        .unwrap_or_default()
}

/// Build instance-data lookup function for message formatting.
pub(crate) fn make_instance_lookup(db: &HcomDb) -> impl Fn(&str) -> Option<Value> + '_ {
    |name: &str| db.get_instance(name).ok().flatten()
}

/// Build a tip-tracking callback for hook message formatting.
pub(crate) fn make_tip_checker(db: &HcomDb) -> impl Fn(&str, &str) -> (bool, Box<dyn Fn()>) + '_ {
    move |instance_name: &str, tip_key: &str| {
        let seen = crate::core::tips::has_seen_tip(db, instance_name, tip_key);
        let db_path = db.path().to_path_buf();
        let instance_name = instance_name.to_string();
        let tip_key = tip_key.to_string();
        let mark = Box::new(move || {
            if let Ok(mark_db) = HcomDb::open_at(&db_path) {
                crate::core::tips::mark_tip_seen(&mark_db, &instance_name, &tip_key);
            }
        }) as Box<dyn Fn()>;
        (seen, mark)
    }
}

pub(crate) struct DeliveryBatch {
    messages: Vec<Value>,
}

impl DeliveryBatch {
    pub(crate) fn into_messages(self) -> Vec<Value> {
        self.messages
    }

    pub(crate) fn format_model_context(&self, db: &HcomDb, instance_name: &str) -> String {
        format_messages_json_for_instance(db, &self.messages, instance_name)
    }

    pub(crate) fn format_user_display(&self, db: &HcomDb, instance_name: &str) -> String {
        format_hook_messages_for_instance(db, &self.messages, instance_name)
    }
}

pub(crate) fn limit_delivery_messages(messages: &[Value]) -> Vec<Value> {
    if messages.len() > MAX_MESSAGES_PER_DELIVERY {
        messages[..MAX_MESSAGES_PER_DELIVERY].to_vec()
    } else {
        messages.to_vec()
    }
}

pub(crate) fn format_messages_json_for_instance(
    db: &HcomDb,
    messages: &[Value],
    instance_name: &str,
) -> String {
    let get_instance_data = make_instance_lookup(db);
    let hints = load_config_hints();
    let get_config_hints = || hints.clone();
    let tip_checker = make_tip_checker(db);
    messages::format_messages_json(
        messages,
        instance_name,
        &get_instance_data,
        &get_config_hints,
        Some(&tip_checker),
    )
}

pub(crate) fn format_hook_messages_for_instance(
    db: &HcomDb,
    messages: &[Value],
    instance_name: &str,
) -> String {
    let get_instance_data = make_instance_lookup(db);
    let hints = load_config_hints();
    let get_config_hints = || hints.clone();
    messages::format_hook_messages(
        messages,
        instance_name,
        &get_instance_data,
        &get_config_hints,
        None,
    )
}

pub(crate) fn prepare_delivery_batch(
    db: &HcomDb,
    instance_name: &str,
    raw_messages: Vec<Message>,
) -> Option<DeliveryBatch> {
    if raw_messages.is_empty() {
        return None;
    }

    let messages: Vec<Value> = raw_messages.iter().map(message_to_value).collect();
    let deliver = limit_delivery_messages(&messages);

    let last_id = deliver
        .last()
        .and_then(|m| m.get("event_id").and_then(|v| v.as_i64()))
        .unwrap_or(0);

    let mut updates = serde_json::Map::new();
    updates.insert("last_event_id".into(), serde_json::json!(last_id));
    instances::update_instance_position(db, instance_name, &updates);

    let sender = deliver
        .first()
        .and_then(|m| m.get("from").and_then(|v| v.as_str()))
        .unwrap_or("unknown");
    let sender_display = instances::get_display_name(db, sender);
    let msg_ts = deliver
        .last()
        .and_then(|m| m.get("timestamp").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();

    lifecycle::set_status(
        db,
        instance_name,
        ST_ACTIVE,
        &format!("deliver:{}", sender_display),
        lifecycle::StatusUpdate {
            msg_ts: &msg_ts,
            ..Default::default()
        },
    );

    Some(DeliveryBatch { messages: deliver })
}

/// Fetch unread messages, update cursor, set delivery status.
///
/// Returns (delivered_messages, formatted_json). Empty vec and None if no messages.
/// Callers that need additional formatting can use the returned messages vec.
///
pub fn deliver_pending_messages(db: &HcomDb, instance_name: &str) -> (Vec<Value>, Option<String>) {
    let raw_messages = db.get_unread_messages(instance_name);
    deliver_raw_messages(db, instance_name, raw_messages)
}

/// Process pre-fetched raw messages: convert, limit, advance cursor, format, update status.
///
/// Shared by `deliver_pending_messages` (fetches then calls this) and `poll_loop`
/// (fetches once for orphan check, then calls this to avoid double-fetch).
fn deliver_raw_messages(
    db: &HcomDb,
    instance_name: &str,
    raw_messages: Vec<Message>,
) -> (Vec<Value>, Option<String>) {
    let Some(batch) = prepare_delivery_batch(db, instance_name, raw_messages) else {
        return (vec![], None);
    };
    let formatted = batch.format_model_context(db, instance_name);
    (batch.into_messages(), Some(formatted))
}

/// Stop hook polling loop — NOT used by main PTY path.
///
/// Runs for: headless instances, vanilla tool instances, subagent polling.
/// Main PTY path bypasses this (HCOM_PTY_MODE=1, PTY wrapper handles injection).
///
/// Uses select() on a TCP socket for efficient wake-on-message delivery.
/// Senders call notify_instance() which connects to wake the select().
///
/// Returns (exit_code, hook_output_json, timed_out).
/// - exit_code: 0 for timeout/no-participant, 2 for message delivery
/// - hook_output: JSON value if messages delivered
/// - timed_out: true if polling timed out without messages
///
pub fn poll_messages(
    db: &HcomDb,
    instance_name: &str,
    timeout_secs: u64,
    is_background: bool,
) -> (i32, Option<Value>, bool) {
    match poll_messages_inner(db, instance_name, timeout_secs, is_background) {
        Ok(result) => result,
        Err(e) => {
            log::log_error(
                "hooks",
                "hook.error",
                &format!("hook=poll_messages err={}", e),
            );
            (0, None, false)
        }
    }
}

fn poll_messages_inner(
    db: &HcomDb,
    instance_name: &str,
    timeout_secs: u64,
    is_background: bool,
) -> Result<(i32, Option<Value>, bool)> {
    // Check instance exists
    let instance_data = db
        .get_instance_full(instance_name)
        .context("DB error checking instance")?;
    if instance_data.is_none() {
        return Ok((0, None, false));
    }

    // Setup TCP notification socket
    let (notify_server, tcp_mode) = setup_tcp_notification(instance_name);
    let notify_port = notify_server
        .as_ref()
        .and_then(|s| s.local_addr().ok())
        .map(|a| a.port());

    // Register TCP mode
    let mut updates = serde_json::Map::new();
    updates.insert("tcp_mode".into(), serde_json::json!(tcp_mode));
    instances::update_instance_position(db, instance_name, &updates);

    // Register hook notify endpoint
    if let Some(port) = notify_port {
        register_hook_notify_port(db, instance_name, port);
    }

    // Set listening status
    lifecycle::set_status(db, instance_name, ST_LISTENING, "", Default::default());

    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    let result = poll_loop(
        db,
        instance_name,
        timeout,
        start,
        is_background,
        notify_server.as_ref(),
    );

    // Cleanup: close socket, remove notify endpoint
    drop(notify_server);
    delete_hook_notify_endpoint(db, instance_name);

    result
}

fn poll_loop(
    db: &HcomDb,
    instance_name: &str,
    timeout: Duration,
    start: Instant,
    is_background: bool,
    notify_server: Option<&TcpListener>,
) -> Result<(i32, Option<Value>, bool)> {
    let mut waited = false;
    while start.elapsed() < timeout {
        // Check if instance still exists (stopped = row deleted)
        let instance_data = db.get_instance_full(instance_name)?;
        if instance_data.is_none() {
            return Ok((0, None, false));
        }

        // Sync remote events via relay (short poll, doesn't block long)
        if crate::relay::is_relay_enabled(&crate::config::load_config_snapshot().core) {
            let remaining = (timeout - start.elapsed()).as_secs_f64();
            if remaining > 0.0 {
                crate::relay::relay_wait(remaining.min(25.0));
            }
        }

        // Poll for messages BEFORE select to catch transition gap
        let raw_messages = db.get_unread_messages(instance_name);
        if !raw_messages.is_empty() {
            // Orphan detection: don't deliver if parent died.
            // Only check after we've waited at least once — on the first iteration stdin
            // may legitimately be closed (e.g. subprocess invocation via `input=...`).
            if waited && !is_background && check_stdin_closed() {
                return Ok((0, None, false));
            }

            let (_deliver, formatted) = deliver_raw_messages(db, instance_name, raw_messages);
            if let Some(formatted) = formatted {
                let output = serde_json::json!({
                    "decision": "block",
                    "reason": formatted,
                });
                return Ok((2, Some(output), false));
            }
        }

        // Calculate remaining time
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            break;
        }
        let remaining = timeout - elapsed;

        // TCP select for notifications (or fallback poll)
        // - With relay: relay_wait() did long-poll, short TCP check (1s)
        // - Local-only with TCP: select wakes on notification (30s)
        // - Local-only no TCP: must poll frequently (100ms)
        let relay_active =
            crate::relay::is_relay_enabled(&crate::config::load_config_snapshot().core);
        let wait_time = if relay_active {
            Duration::from_secs(remaining.as_secs().min(1))
        } else if notify_server.is_some() {
            Duration::from_secs(remaining.as_secs().min(30))
        } else {
            Duration::from_millis(remaining.as_millis().min(100) as u64)
        };

        if let Some(server) = notify_server {
            // Block on poll(2) instead of busy-looping with accept+sleep
            use std::os::fd::AsRawFd;
            let fd = server.as_raw_fd();
            let timeout_ms = wait_time.as_millis().min(i32::MAX as u128) as i32;
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: valid pollfd, nfds=1, bounded timeout
            let ret = unsafe { libc::poll(&mut pfd as *mut _, 1, timeout_ms) };
            if ret > 0 {
                // Drain all pending connections
                if let Err(e) = server.set_nonblocking(true) {
                    log::log_warn(
                        "hooks",
                        "poll.nonblocking_failed",
                        &format!("set_nonblocking failed: {e}, skipping drain"),
                    );
                } else {
                    while let Ok((conn, _)) = server.accept() {
                        drop(conn);
                    }
                }
            }
        } else {
            std::thread::sleep(wait_time);
        }

        waited = true;

        // Update heartbeat (also re-asserts tcp_mode=1 for self-healing)
        let _ = db.update_heartbeat(instance_name);
    }

    // Timeout reached
    Ok((0, None, true))
}

/// Check if stdin is closed (orphan detection heuristic).
///
/// Piped stdin (normal for hook subprocess invocation) always gets POLLHUP
/// after the payload is consumed — this is NOT an orphan signal. Only check
/// POLLERR (broken pipe) and POLLNVAL (fd was closed/invalidated).
///
fn check_stdin_closed() -> bool {
    let mut pfd = libc::pollfd {
        fd: 0, // stdin
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: valid pollfd, nfds=1, timeout=0
    let ret = unsafe { libc::poll(&mut pfd as *mut _, 1, 0) };
    if ret < 0 {
        return true; // poll error → assume closed
    }
    // Only POLLERR/POLLNVAL — NOT POLLHUP (normal pipe EOF)
    (pfd.revents & (libc::POLLERR | libc::POLLNVAL)) != 0
}

/// Create TCP server socket for instant message wake notifications.
fn setup_tcp_notification(instance_name: &str) -> (Option<TcpListener>, bool) {
    match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => {
            listener.set_nonblocking(true).unwrap_or(());
            (Some(listener), true)
        }
        Err(e) => {
            log::log_error(
                "hooks",
                "hook.error",
                &format!("hook=tcp_notification instance={} err={}", instance_name, e),
            );
            (None, false)
        }
    }
}

/// Register hook notify port in DB.
fn register_hook_notify_port(db: &HcomDb, instance_name: &str, port: u16) {
    if let Err(e) = db.upsert_notify_endpoint(instance_name, "hook", port) {
        log::log_warn(
            "native",
            "hooks.register_notify_fail",
            &format!(
                "Failed to register hook notify port for {}: {}",
                instance_name, e
            ),
        );
    }
}

/// Remove hook notify endpoint from DB.
fn delete_hook_notify_endpoint(db: &HcomDb, instance_name: &str) {
    let _ = db.conn().execute(
        "DELETE FROM notify_endpoints WHERE instance = ? AND kind = 'hook'",
        params![instance_name],
    );
}

/// Find last [hcom:xxx] marker in transcript.
///
/// Reads file backwards in 64MB chunks with 70-byte overlap to find marker.
pub fn find_last_bind_marker(transcript_path: &str) -> Option<String> {
    let path = Path::new(transcript_path);
    let metadata = std::fs::metadata(path).ok()?;
    let file_size = metadata.len() as usize;

    if file_size == 0 {
        return None;
    }

    let chunk_size: usize = 64 * 1024 * 1024; // 64MB
    let overlap: usize = 70; // max prefix len (12) + max instance name (50) + margin
    let marker_prefixes: &[&[u8]] = &[b"[hcom:"];

    let mut file = std::fs::File::open(path).ok()?;

    let mut pos = file_size;
    let mut carry: Vec<u8> = Vec::new();

    while pos > 0 {
        let read_size = chunk_size.min(pos);
        pos -= read_size;

        use std::io::{Read as _, Seek, SeekFrom};
        file.seek(SeekFrom::Start(pos as u64)).ok()?;

        let mut data = vec![0u8; read_size];
        file.read_exact(&mut data).ok()?;

        // Combine data + carry for overlap handling
        let mut buf = data.clone();
        buf.extend_from_slice(&carry);

        // Find the last occurrence of any marker prefix
        let mut best_idx: Option<usize> = None;
        for prefix in marker_prefixes {
            if let Some(idx) = rfind_bytes(&buf, prefix) {
                match best_idx {
                    Some(current) if idx > current => best_idx = Some(idx),
                    None => best_idx = Some(idx),
                    _ => {}
                }
            }
        }

        if let Some(idx) = best_idx {
            // Find closing bracket
            if let Some(end_offset) = buf[idx..].iter().position(|&b| b == b']') {
                let marker_bytes = &buf[idx..idx + end_offset + 1];
                if let Ok(marker_str) = std::str::from_utf8(marker_bytes) {
                    if let Some(caps) = BIND_MARKER_RE.captures(marker_str) {
                        return Some(caps[1].to_string());
                    }
                }
            }
        }

        // Keep overlap for next chunk
        carry = if overlap > 0 && data.len() >= overlap {
            data[..overlap].to_vec()
        } else {
            data
        };
    }

    None
}

/// Reverse search for byte pattern in buffer.
fn rfind_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    for i in (0..=haystack.len() - needle.len()).rev() {
        if haystack[i..i + needle.len()] == *needle {
            return Some(i);
        }
    }
    None
}

/// Inject bootstrap text if not already announced.
///
/// Idempotent — checks name_announced flag and only injects once
/// per instance lifecycle. Returns bootstrap text if injection needed,
/// None if already announced.
///
pub fn inject_bootstrap_once(
    db: &HcomDb,
    ctx: &HcomContext,
    instance_name: &str,
    instance_data: &InstanceRow,
    tool: &str,
) -> Option<String> {
    if instance_data.name_announced != 0 {
        return None;
    }

    let tag = instance_data.tag.as_deref().unwrap_or("");
    let hcom_config = crate::config::HcomConfig::load(None).unwrap_or_default();
    let relay_enabled = crate::relay::is_relay_enabled(&hcom_config);

    let bootstrap_text = bootstrap::get_bootstrap(
        db,
        &ctx.hcom_dir,
        instance_name,
        tool,
        ctx.is_background,
        ctx.is_launched,
        &ctx.notes,
        tag,
        relay_enabled,
        ctx.background_name.as_deref(),
    );

    // Mark as announced
    let mut updates = serde_json::Map::new();
    updates.insert("name_announced".into(), serde_json::json!(true));
    instances::update_instance_position(db, instance_name, &updates);

    Some(bootstrap_text)
}

/// Initialize instance context from hook data via binding lookup.
///
/// Primary gate for hook participation. Resolution order:
/// 1. HCOM_PROCESS_ID → process_bindings → instance_name
/// 2. session_id → session_bindings → instance_name
/// 3. Transcript marker fallback
/// 4. Not found → (None, empty, false)
///
/// Returns (instance_name, metadata_updates, is_matched_resume).
///
pub fn init_hook_context(
    db: &HcomDb,
    ctx: &HcomContext,
    session_id: &str,
    transcript_path: &str,
) -> (Option<String>, serde_json::Map<String, Value>, bool) {
    let start = Instant::now();
    let mut instance_name: Option<String> = None;

    // Path 1: Process binding (hcom-launched instances)
    let process_start = Instant::now();
    if let Some(ref process_id) = ctx.process_id {
        if let Ok(Some(name)) = db.get_process_binding(process_id) {
            instance_name = Some(name);
        }
    }
    let process_ms = process_start.elapsed().as_secs_f64() * 1000.0;

    // Path 2: Session binding
    let binding_start = Instant::now();
    if instance_name.is_none() && !session_id.is_empty() {
        if let Ok(Some(name)) = db.get_session_binding(session_id) {
            instance_name = Some(name);
        }
    }
    let binding_ms = binding_start.elapsed().as_secs_f64() * 1000.0;

    // Path 3: Transcript marker fallback
    let transcript_start = Instant::now();
    if instance_name.is_none() {
        instance_name = try_bind_from_transcript(db, session_id, transcript_path);
        if instance_name.is_none() {
            let transcript_ms = transcript_start.elapsed().as_secs_f64() * 1000.0;
            let total_ms = start.elapsed().as_secs_f64() * 1000.0;
            log::log_info(
                "hooks",
                "init_hook_context.timing",
                &format!(
                    "process_ms={:.2} binding_ms={:.2} transcript_ms={:.2} total_ms={:.2} result=no_instance",
                    process_ms, binding_ms, transcript_ms, total_ms
                ),
            );
            return (None, serde_json::Map::new(), false);
        }
    }
    let transcript_ms = transcript_start.elapsed().as_secs_f64() * 1000.0;

    let name = instance_name.unwrap();

    // Build metadata updates
    let instance_start = Instant::now();
    let mut updates = serde_json::Map::new();
    updates.insert(
        "directory".into(),
        Value::String(ctx.cwd.to_string_lossy().to_string()),
    );

    if !transcript_path.is_empty() {
        updates.insert(
            "transcript_path".into(),
            Value::String(transcript_path.to_string()),
        );
    }

    if ctx.is_background {
        if let Some(ref bg_name) = ctx.background_name {
            updates.insert("background".into(), serde_json::json!(true));
            let log_file = ctx.hcom_dir.join(".tmp").join("logs").join(bg_name);
            updates.insert(
                "background_log_file".into(),
                Value::String(log_file.to_string_lossy().to_string()),
            );
        }
    }

    // Check if session matches (resume detection)
    let is_matched_resume = if !session_id.is_empty() {
        db.get_instance_full(&name)
            .ok()
            .flatten()
            .map(|data| data.session_id.as_deref() == Some(session_id))
            .unwrap_or(false)
    } else {
        false
    };
    let instance_ms = instance_start.elapsed().as_secs_f64() * 1000.0;

    let total_ms = start.elapsed().as_secs_f64() * 1000.0;
    log::log_info(
        "hooks",
        "init_hook_context.timing",
        &format!(
            "instance={} process_ms={:.2} binding_ms={:.2} transcript_ms={:.2} instance_ms={:.2} total_ms={:.2}",
            name, process_ms, binding_ms, transcript_ms, instance_ms, total_ms
        ),
    );

    (Some(name), updates, is_matched_resume)
}

/// Transcript marker fallback binding.
///
/// Searches transcript for [hcom:name] marker and creates session binding
/// if instance is pending. Fast path: skips file I/O if no pending instances.
///
fn try_bind_from_transcript(
    db: &HcomDb,
    session_id: &str,
    transcript_path: &str,
) -> Option<String> {
    if transcript_path.is_empty() || session_id.is_empty() {
        return None;
    }

    // Fast path: skip file I/O if no pending instances
    let pending = get_pending_instances(db);
    if pending.is_empty() {
        return None;
    }

    let instance_name = find_last_bind_marker(transcript_path)?;

    // Only bind if instance is in pending list
    if !pending.contains(&instance_name) {
        log::log_info(
            "hooks",
            "transcript.bind.skip",
            &format!("instance={} not in pending={:?}", instance_name, pending),
        );
        return None;
    }

    // Verify instance exists
    let instance = db.get_instance_full(&instance_name).ok()??;
    let _ = instance; // just checking existence

    // Create binding
    if let Err(e) = db.rebind_instance_session(&instance_name, session_id) {
        log::log_error(
            "hooks",
            "transcript.bind.error",
            &format!("instance={} err={}", instance_name, e),
        );
        return None;
    }

    let mut updates = serde_json::Map::new();
    updates.insert("session_id".into(), Value::String(session_id.to_string()));
    instances::update_instance_position(db, &instance_name, &updates);

    log::log_info(
        "hooks",
        "transcript.bind.success",
        &format!("instance={}", instance_name),
    );

    Some(instance_name)
}

/// Get instances pending session binding (session_id IS NULL, non-adhoc).
///
/// "Pending" means the instance was created (e.g., by launcher) but hasn't
/// been bound to a tool session yet. Used as fast-path optimization before
/// doing expensive transcript marker search.
///
pub fn get_pending_instances(db: &HcomDb) -> Vec<String> {
    // Purge leaked launch placeholders before treating them as bindable.
    // Otherwise an old transcript marker can silently re-bind a stale row.
    lifecycle::cleanup_stale_placeholders(db);
    let mut stmt = match db.conn().prepare(
        "SELECT name FROM instances WHERE session_id IS NULL AND tool != 'adhoc' ORDER BY created_at DESC",
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    stmt.query_map([], |row| row.get::<_, String>(0))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

/// Wake an instance's hook poll loop via TCP connection.
///
/// Best-effort: opens DB, finds hook notify endpoint, sends brief TCP connect.
/// This is the hook-side counterpart to instances::notify_instance which
/// handles PTY-side notification.
///
pub fn notify_hook_instance(instance_name: &str) {
    if let Ok(db) = HcomDb::open() {
        notify_hook_instance_with_db(&db, instance_name);
    }
}

/// Wake hook poll loop with an existing DB handle.
pub fn notify_hook_instance_with_db(db: &HcomDb, instance_name: &str) {
    lifecycle::notify_instance_endpoints(db, instance_name, &["hook"]);
}

/// Stop instance: log snapshot, clean bindings, delete row.
///
/// Handles: snapshot capture, session/process/notify/subscription cleanup,
/// life event logging, and instance deletion.
pub fn stop_instance(db: &HcomDb, instance_name: &str, initiated_by: &str, reason: &str) {
    stop_instance_inner(db, instance_name, initiated_by, reason, 0);
}

/// Max recursion depth for subagent cleanup. Prevents stack overflow if DB
/// corruption creates a parent_session_id cycle.
const MAX_STOP_DEPTH: u32 = 10;

fn stop_instance_inner(
    db: &HcomDb,
    instance_name: &str,
    initiated_by: &str,
    reason: &str,
    depth: u32,
) {
    if depth >= MAX_STOP_DEPTH {
        log::log_warn(
            "core",
            "stop_instance.max_depth",
            &format!(
                "Recursion limit ({}) reached stopping {}; possible cycle",
                MAX_STOP_DEPTH, instance_name
            ),
        );
        return;
    }

    let instance_data = match db.get_instance_full(instance_name) {
        Ok(Some(data)) => data,
        _ => return,
    };

    // Kill headless processes (background=true)
    let pid = instance_data.pid;
    let is_headless = instance_data.background != 0;
    if let Some(pid_val) = pid {
        let pid_i32 = pid_val as i32;
        if is_headless {
            // SIGTERM → wait up to 2s → SIGKILL
            let term_ret = unsafe { libc::killpg(pid_i32, libc::SIGTERM) };
            if term_ret == 0 {
                let mut dead = false;
                for _ in 0..20 {
                    std::thread::sleep(Duration::from_millis(100));
                    let probe = unsafe { libc::kill(pid_i32, 0) };
                    if probe != 0 {
                        dead = true;
                        break;
                    }
                }
                if !dead {
                    unsafe { libc::killpg(pid_i32, libc::SIGKILL) };
                }
            }
            // ESRCH/EPERM from initial killpg is fine — process already gone or foreign
        } else {
            // Track surviving PTY processes in pidtrack
            let alive = {
                let probe = unsafe { libc::kill(pid_i32, 0) };
                if probe == 0 {
                    true
                } else {
                    // EPERM = exists but foreign user — still track
                    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
                }
            };
            if alive {
                let hcom_dir = crate::paths::hcom_dir();

                let ti = crate::terminal::resolve_terminal_info(
                    instance_data.terminal_preset_effective.as_deref(),
                    instance_data.launch_context.as_deref(),
                );
                let terminal_preset = ti.preset_name;
                let pane_id = ti.pane_id;
                let mut proc_id = ti.process_id;
                let terminal_id = ti.terminal_id;
                let kitty_listen_on = ti.kitty_listen_on;
                // Fallback: process_bindings table
                if proc_id.is_empty() {
                    if let Ok(mut stmt) = db
                        .conn()
                        .prepare("SELECT process_id FROM process_bindings WHERE instance_name = ?")
                    {
                        if let Ok(val) =
                            stmt.query_row(params![instance_name], |row| row.get::<_, String>(0))
                        {
                            proc_id = val;
                        }
                    }
                }
                // Grab notify/inject ports before DB cleanup deletes them
                let mut notify_port: u16 = 0;
                let mut inject_port: u16 = 0;
                if let Ok(mut stmt) = db
                    .conn()
                    .prepare("SELECT kind, port FROM notify_endpoints WHERE instance = ?")
                {
                    if let Ok(rows) = stmt.query_map(params![instance_name], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    }) {
                        for row in rows.flatten() {
                            match row.0.as_str() {
                                "pty" => notify_port = row.1 as u16,
                                "inject" => inject_port = row.1 as u16,
                                _ => {}
                            }
                        }
                    }
                }

                crate::pidtrack::record_pid(&crate::pidtrack::PidRecord {
                    hcom_dir: &hcom_dir,
                    pid: pid_val as u32,
                    tool: &instance_data.tool,
                    name: instance_name,
                    directory: &instance_data.directory,
                    process_id: &proc_id,
                    terminal_preset: &terminal_preset,
                    pane_id: &pane_id,
                    terminal_id: &terminal_id,
                    kitty_listen_on: &kitty_listen_on,
                    session_id: instance_data.session_id.as_deref().unwrap_or(""),
                    notify_port,
                    inject_port,
                    tag: instance_data.tag.as_deref().unwrap_or(""),
                });
                log::log_info(
                    "stop",
                    "pidtrack_recorded",
                    &format!(
                        "pid={} instance={} preset={} pane_id={}",
                        pid_val, instance_name, terminal_preset, pane_id
                    ),
                );
            }
        }
    }

    // Capture notify ports BEFORE cleanup deletes them
    let notify_ports: Vec<i64> = db
        .conn()
        .prepare("SELECT port FROM notify_endpoints WHERE instance = ?")
        .and_then(|mut stmt| {
            stmt.query_map(params![instance_name], |row| row.get::<_, i64>(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    // Prepare snapshot before delete (preserves data for transcript access)
    // Use Option values directly so None serializes as JSON null
    let snapshot = serde_json::json!({
        "name": instance_name,
        "transcript_path": instance_data.transcript_path,
        "session_id": instance_data.session_id,
        "tool": instance_data.tool,
        "directory": instance_data.directory,
        "parent_name": instance_data.parent_name,
        "tag": instance_data.tag,
        "wait_timeout": instance_data.wait_timeout,
        "subagent_timeout": instance_data.subagent_timeout,
        "hints": instance_data.hints,
        "pid": instance_data.pid,
        "created_at": instance_data.created_at,
        "background": instance_data.background,
        "agent_id": instance_data.agent_id,
        "launch_args": instance_data.launch_args,
        "origin_device_id": instance_data.origin_device_id,
        "background_log_file": instance_data.background_log_file,
        "last_event_id": instance_data.last_event_id,
    });

    // Clean session bindings + process bindings + stop subagents for this session
    if let Some(ref session_id) = instance_data.session_id {
        let _ = db.conn().execute(
            "DELETE FROM session_bindings WHERE session_id = ?",
            params![session_id],
        );
        let _ = db.conn().execute(
            "DELETE FROM process_bindings WHERE session_id = ?",
            params![session_id],
        );

        // Recursively stop subagents whose parent_session_id matches this session
        let subagents: Vec<String> = db
            .conn()
            .prepare("SELECT name FROM instances WHERE parent_session_id = ?")
            .and_then(|mut stmt| {
                stmt.query_map(params![session_id], |row| row.get::<_, String>(0))
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();
        for sub_name in subagents {
            stop_instance_inner(db, &sub_name, initiated_by, "parent_stopped", depth + 1);
        }
    }

    // Clean notify endpoints and process bindings for this instance
    let _ = db.delete_notify_endpoints(instance_name);
    let _ = db.conn().execute(
        "DELETE FROM process_bindings WHERE instance_name = ?",
        params![instance_name],
    );

    // Clean event subscriptions
    let _ = db.cleanup_subscriptions(instance_name);

    // Log life event with snapshot BEFORE delete
    if let Err(e) = db.log_life_event(
        instance_name,
        "stopped",
        initiated_by,
        reason,
        Some(snapshot),
    ) {
        log::log_warn(
            "hooks",
            "finalize.life_event_failed",
            &format!("log_life_event failed for {instance_name}: {e}"),
        );
    }

    // Delete instance row (CASCADE cleans remaining FK references)
    if let Err(e) = db.delete_instance(instance_name) {
        log::log_warn(
            "hooks",
            "finalize.delete_failed",
            &format!("delete_instance failed for {instance_name}: {e}"),
        );
    }

    // Notify remaining listeners AFTER delete (so they see the row is gone)
    for port in notify_ports {
        if port > 0 && port <= 65535 {
            let addr = format!("127.0.0.1:{}", port);
            if let Ok(addr) = addr.parse() {
                let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(100));
            }
        }
    }

    // Trigger relay push (best-effort)
    let prefix = crate::runtime_env::get_hcom_prefix();
    if let Some((cmd, prefix_args)) = prefix.split_first() {
        let _ = std::process::Command::new(cmd)
            .args(prefix_args)
            .args(["relay", "push"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// Set inactive status, persist updates, and stop instance.
///
/// Common to Claude and Gemini SessionEnd handlers. Catches all errors
/// internally — callers don't need error handling.
///
pub fn finalize_session(
    db: &HcomDb,
    instance_name: &str,
    reason: &str,
    updates: Option<&serde_json::Map<String, Value>>,
) {
    log::log_info(
        "hooks",
        "sessionend",
        &format!("instance={} reason={}", instance_name, reason),
    );

    // Set inactive status
    lifecycle::set_status(
        db,
        instance_name,
        ST_INACTIVE,
        &format!("exit:{}", reason),
        Default::default(),
    );

    // Persist metadata updates
    if let Some(updates) = updates {
        instances::update_instance_position(db, instance_name, updates);
    }

    // Full stop_instance chain: snapshot, cleanup bindings, log, delete
    stop_instance(db, instance_name, "session", &format!("exit:{}", reason));
}

/// Update instance status for tool execution.
///
/// Calls extract_tool_detail for tool-specific detail formatting,
/// then sets status to active with tool context.
///
pub fn update_tool_status(
    db: &HcomDb,
    instance_name: &str,
    tool: &str,
    tool_name: &str,
    tool_input: &Value,
) {
    let detail = super::family::extract_tool_detail(tool, tool_name, tool_input);
    lifecycle::set_status(
        db,
        instance_name,
        ST_ACTIVE,
        &format!("tool:{}", tool_name),
        lifecycle::StatusUpdate {
            detail: &detail,
            ..Default::default()
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_find_last_bind_marker_basic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "some log data").unwrap();
        writeln!(f, "more data [hcom:luna] more stuff").unwrap();
        writeln!(f, "trailing data").unwrap();

        let result = find_last_bind_marker(path.to_str().unwrap());
        assert_eq!(result, Some("luna".to_string()));
    }

    #[test]
    fn test_find_last_bind_marker_returns_last() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "[hcom:first]").unwrap();
        writeln!(f, "[hcom:second]").unwrap();
        writeln!(f, "[hcom:third]").unwrap();

        let result = find_last_bind_marker(path.to_str().unwrap());
        assert_eq!(result, Some("third".to_string()));
    }

    #[test]
    fn test_find_last_bind_marker_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "no markers here").unwrap();

        let result = find_last_bind_marker(path.to_str().unwrap());
        assert!(result.is_none());
    }

    #[test]
    fn test_find_last_bind_marker_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        std::fs::File::create(&path).unwrap();

        let result = find_last_bind_marker(path.to_str().unwrap());
        assert!(result.is_none());
    }

    #[test]
    fn test_find_last_bind_marker_missing_file() {
        let result = find_last_bind_marker("/nonexistent/path.jsonl");
        assert!(result.is_none());
    }

    #[test]
    fn test_find_last_bind_marker_large_file_marker_at_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // Write ~1MB of padding + marker at end
        let padding = "x".repeat(1024);
        for _ in 0..1024 {
            writeln!(f, "{}", padding).unwrap();
        }
        writeln!(f, "[hcom:bigtarget]").unwrap();

        let result = find_last_bind_marker(path.to_str().unwrap());
        assert_eq!(result, Some("bigtarget".to_string()));
    }

    #[test]
    fn test_rfind_bytes_basic() {
        let haystack = b"hello [hcom:test] world [hcom:second] end";
        assert_eq!(rfind_bytes(haystack, b"[hcom:"), Some(24));
    }

    #[test]
    fn test_rfind_bytes_not_found() {
        assert_eq!(rfind_bytes(b"hello world", b"[hcom:"), None);
    }

    #[test]
    fn test_rfind_bytes_empty() {
        assert_eq!(rfind_bytes(b"", b"[hcom:"), None);
        assert_eq!(rfind_bytes(b"hello", b""), None);
    }

    #[test]
    fn test_check_stdin_closed_does_not_panic() {
        // Verify the function runs without panicking regardless of stdin state.
        // In test context stdin is typically a pipe — check_stdin_closed should
        // return false because POLLHUP (normal pipe EOF) is NOT treated as closed.
        let result = check_stdin_closed();
        // Don't assert specific value — stdin state varies across test runners.
        let _ = result;
    }

    #[test]
    fn test_setup_tcp_notification() {
        let (server, tcp_mode) = setup_tcp_notification("test_instance");
        assert!(tcp_mode);
        assert!(server.is_some());

        let addr = server.as_ref().unwrap().local_addr().unwrap();
        assert!(addr.port() > 0);
    }

    #[test]
    fn test_notify_hook_instance_no_db() {
        // Best-effort function should not panic even with no DB
        // (HcomDb::open() will fail in test env without ~/.hcom)
        notify_hook_instance("nonexistent");
    }

    fn make_test_db() -> (tempfile::TempDir, crate::db::HcomDb) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = crate::db::HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        (dir, db)
    }

    fn insert_test_instance(db: &crate::db::HcomDb, name: &str) {
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES (?1, 'claude', 'listening', 'start', 0, 0, 0)",
                [name],
            )
            .unwrap();
    }

    fn insert_test_message(
        db: &crate::db::HcomDb,
        instance: &str,
        from: &str,
        text: &str,
        timestamp: &str,
    ) {
        let data = serde_json::json!({
            "from": from,
            "text": text,
            "scope": "broadcast",
        })
        .to_string();
        db.conn()
            .execute(
                "INSERT INTO events (type, timestamp, instance, data) VALUES ('message', ?1, ?2, ?3)",
                rusqlite::params![timestamp, instance, data],
            )
            .unwrap();
    }

    #[test]
    fn test_prepare_delivery_batch_empty() {
        let (_dir, db) = make_test_db();
        insert_test_instance(&db, "nova");

        let batch = prepare_delivery_batch(&db, "nova", vec![]);
        assert!(batch.is_none());
    }

    #[test]
    fn test_prepare_delivery_batch_caps_cursor_and_status() {
        let (_dir, db) = make_test_db();
        insert_test_instance(&db, "nova");
        for i in 0..(MAX_MESSAGES_PER_DELIVERY + 2) {
            insert_test_message(
                &db,
                "luna",
                "luna",
                &format!("msg {i}"),
                &format!("2026-01-01T00:00:{i:02}Z"),
            );
        }

        let raw_messages = db.get_unread_messages("nova");
        let batch = prepare_delivery_batch(&db, "nova", raw_messages).unwrap();

        assert_eq!(batch.messages.len(), MAX_MESSAGES_PER_DELIVERY);

        let cursor: i64 = db
            .conn()
            .query_row(
                "SELECT last_event_id FROM instances WHERE name = 'nova'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let expected_last_id = batch
            .messages
            .last()
            .and_then(|m| m.get("event_id"))
            .and_then(|v| v.as_i64())
            .unwrap();
        assert_eq!(cursor, expected_last_id);

        let instance = db.get_instance_full("nova").unwrap().unwrap();
        let last_msg_ts = batch
            .messages
            .last()
            .and_then(|m| m.get("timestamp"))
            .and_then(|v| v.as_str())
            .unwrap();
        let delivered_name = crate::instances::get_display_name(&db, "luna");
        let mut deliver_events = db
            .conn()
            .prepare(
                "SELECT data FROM events WHERE type = 'status' AND instance = 'nova' ORDER BY id DESC LIMIT 1",
            )
            .unwrap();
        let event_data: String = deliver_events.query_row([], |row| row.get(0)).unwrap();
        let event_json: serde_json::Value = serde_json::from_str(&event_data).unwrap();

        assert_eq!(instance.status, ST_ACTIVE);
        assert_eq!(instance.status_context, format!("deliver:{delivered_name}"));
        assert_eq!(event_json["msg_ts"], last_msg_ts);
        assert_eq!(event_json["context"], format!("deliver:{delivered_name}"));
    }

    #[test]
    fn test_stop_instance_basic_cleanup() {
        crate::config::Config::init();
        let (_dir, db) = make_test_db();

        // Create parent instance
        let _ = db.conn().execute(
            "INSERT INTO instances (name, tool, session_id, status, status_context, status_time, created_at)
             VALUES ('parent', 'claude', 'sess-1', 'active', 'new', 0, 0)",
            [],
        );
        // Add notify endpoint
        let _ = db.conn().execute(
            "INSERT INTO notify_endpoints (instance, kind, port, updated_at) VALUES ('parent', 'pty', 9999, 0)",
            [],
        );
        // Add process binding
        let _ = db.conn().execute(
            "INSERT INTO process_bindings (process_id, session_id, instance_name, updated_at) VALUES ('proc-1', 'sess-1', 'parent', 0)",
            [],
        );

        stop_instance(&db, "parent", "test", "test_cleanup");

        // Instance should be deleted
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM instances WHERE name = 'parent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "instance should be deleted");

        // Notify endpoints should be deleted
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM notify_endpoints WHERE instance = 'parent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "notify endpoints should be deleted");

        // Process bindings should be deleted
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM process_bindings WHERE instance_name = 'parent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "process bindings should be deleted");

        // Life event should be logged
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'life' AND instance = 'parent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "life event should be logged");
    }

    #[test]
    fn test_stop_instance_recursive_subagent_cleanup() {
        crate::config::Config::init();
        let (_dir, db) = make_test_db();

        // Create parent instance with session_id
        let _ = db.conn().execute(
            "INSERT INTO instances (name, tool, session_id, status, status_context, status_time, created_at)
             VALUES ('parent', 'claude', 'sess-parent', 'active', 'new', 0, 0)",
            [],
        );
        // Create subagent linked to parent via parent_session_id
        let _ = db.conn().execute(
            "INSERT INTO instances (name, tool, session_id, parent_session_id, parent_name, status, status_context, status_time, created_at)
             VALUES ('sub1', 'claude', 'sess-sub1', 'sess-parent', 'parent', 'active', 'new', 0, 0)",
            [],
        );
        // Create second subagent
        let _ = db.conn().execute(
            "INSERT INTO instances (name, tool, session_id, parent_session_id, parent_name, status, status_context, status_time, created_at)
             VALUES ('sub2', 'claude', 'sess-sub2', 'sess-parent', 'parent', 'active', 'new', 0, 0)",
            [],
        );

        // Stop parent — should recursively stop subagents
        stop_instance(&db, "parent", "test", "test_recursive");

        // All three instances should be deleted
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM instances", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "all instances (parent + subagents) should be deleted"
        );

        // Life events should be logged for all three
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM events WHERE type = 'life'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 3, "life events for parent + 2 subagents");
    }

    #[test]
    fn test_stop_instance_recursive_depth_2() {
        crate::config::Config::init();
        let (_dir, db) = make_test_db();

        // parent → sub1 → subsub1 (depth-2 chain proves real recursion)
        let _ = db.conn().execute(
            "INSERT INTO instances (name, tool, session_id, status, status_context, status_time, created_at)
             VALUES ('parent', 'claude', 'sess-p', 'active', 'running', 0, 0)",
            [],
        );
        let _ = db.conn().execute(
            "INSERT INTO instances (name, tool, session_id, parent_session_id, parent_name, status, status_context, status_time, created_at)
             VALUES ('sub1', 'claude', 'sess-s1', 'sess-p', 'parent', 'active', 'running', 0, 0)",
            [],
        );
        let _ = db.conn().execute(
            "INSERT INTO instances (name, tool, session_id, parent_session_id, parent_name, status, status_context, status_time, created_at)
             VALUES ('subsub1', 'claude', 'sess-ss1', 'sess-s1', 'sub1', 'active', 'running', 0, 0)",
            [],
        );

        stop_instance(&db, "parent", "test", "test_depth2");

        // All three levels should be cleaned up
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM instances", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "all 3 levels should be deleted");

        // Verify life events logged for each level
        let stopped: Vec<String> = db.conn()
            .prepare("SELECT instance FROM events WHERE type = 'life' AND data LIKE '%stopped%' ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(stopped.len(), 3, "life events for all 3 levels");
        // subsub1 stopped first (deepest), then sub1, then parent
        assert_eq!(stopped[0], "subsub1");
        assert_eq!(stopped[1], "sub1");
        assert_eq!(stopped[2], "parent");
    }

    #[test]
    fn test_stop_instance_depth_limit() {
        crate::config::Config::init();
        let (_dir, db) = make_test_db();

        // Create a chain deeper than MAX_STOP_DEPTH to verify the limit kicks in
        // We'll create 12 levels (limit is 10)
        for i in 0..12u32 {
            let name = format!("inst{}", i);
            let session_id = format!("sess-{}", i);
            let parent_sid = if i == 0 {
                String::new()
            } else {
                format!("sess-{}", i - 1)
            };

            if i == 0 {
                let _ = db.conn().execute(
                    "INSERT INTO instances (name, tool, session_id, status, status_context, status_time, created_at)
                     VALUES (?1, 'claude', ?2, 'active', 'running', 0, 0)",
                    rusqlite::params![name, session_id],
                );
            } else {
                let parent_name = format!("inst{}", i - 1);
                let _ = db.conn().execute(
                    "INSERT INTO instances (name, tool, session_id, parent_session_id, parent_name, status, status_context, status_time, created_at)
                     VALUES (?1, 'claude', ?2, ?3, ?4, 'active', 'running', 0, 0)",
                    rusqlite::params![name, session_id, parent_sid, parent_name],
                );
            }
        }

        // Stop root — should stop up to depth 10 but leave the deepest 2
        stop_instance(&db, "inst0", "test", "test_depth_limit");

        // inst10 and inst11 should survive (depth 10 and 11, beyond limit)
        let remaining: Vec<String> = db
            .conn()
            .prepare("SELECT name FROM instances ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(
            remaining,
            vec!["inst10", "inst11"],
            "instances beyond depth limit should survive"
        );
    }

    #[test]
    fn test_stop_instance_idempotent() {
        crate::config::Config::init();
        let (_dir, db) = make_test_db();

        // Create instance
        let _ = db.conn().execute(
            "INSERT INTO instances (name, tool, status, status_context, status_time, created_at)
             VALUES ('inst', 'claude', 'active', 'new', 0, 0)",
            [],
        );

        // Stop twice — second call should be a no-op
        stop_instance(&db, "inst", "test", "first");
        stop_instance(&db, "inst", "test", "second");

        // Only one life event
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'life' AND instance = 'inst'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "only one life event for idempotent stop");
    }

    #[test]
    fn test_stop_instance_nonexistent() {
        crate::config::Config::init();
        let (_dir, db) = make_test_db();

        // Should be a no-op, not panic
        stop_instance(&db, "nonexistent", "test", "test");
    }

    #[test]
    fn test_finalize_session_calls_stop() {
        crate::config::Config::init();
        let (_dir, db) = make_test_db();

        // Use status_context != "new" to avoid triggering the "ready" life event
        let _ = db.conn().execute(
            "INSERT INTO instances (name, tool, session_id, status, status_context, status_time, created_at)
             VALUES ('inst', 'claude', 'sess-1', 'active', 'running', 0, 0)",
            [],
        );

        finalize_session(&db, "inst", "user_quit", None);

        // Instance should be deleted
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM instances WHERE name = 'inst'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "finalize_session should delete instance");

        // "stopped" life event logged
        let count: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM events WHERE type = 'life' AND instance = 'inst' AND data LIKE '%stopped%'",
            [], |r| r.get(0)
        ).unwrap();
        assert_eq!(count, 1, "stopped life event should be logged");
    }

    #[test]
    fn test_stale_placeholder_marker_does_not_rebind() {
        crate::config::Config::init();
        let (dir, db) = make_test_db();

        // Simulate a leaked launch placeholder older than the stale threshold.
        let old_time = crate::shared::time::now_epoch_f64()
            - (lifecycle::CLEANUP_PLACEHOLDER_THRESHOLD as f64 + 80.0);
        let _ = db.conn().execute(
            "INSERT INTO instances (name, tool, status, status_context, created_at)
             VALUES ('luna', 'claude', 'pending', 'new', ?1)",
            rusqlite::params![old_time],
        );

        let transcript = dir.path().join("transcript.jsonl");
        std::fs::write(&transcript, "assistant output [hcom:luna]\n").unwrap();

        let ctx = crate::shared::context::HcomContext::from_env(
            &std::collections::HashMap::new(),
            dir.path().to_path_buf(),
        );
        let (instance_name, _updates, _matched_resume) =
            init_hook_context(&db, &ctx, "sess-fresh", transcript.to_str().unwrap());

        assert!(
            instance_name.is_none(),
            "stale placeholder should be cleaned before transcript binding"
        );

        assert!(
            db.get_instance_full("luna").unwrap().is_none(),
            "stale placeholder row should be deleted"
        );

        assert_eq!(
            db.get_session_binding("sess-fresh").unwrap(),
            None,
            "fresh session must not get bound via stale marker"
        );
    }
}
