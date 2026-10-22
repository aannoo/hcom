//! Push loop — build state snapshot and events, publish via MQTT.
//!
//! Batches up to 100 events per publish with a 10s drain budget.
//! Tracks progress via KV cursor `relay_last_push_id`.

use rumqttc::{Client, PublishOptions};
use serde_json::{Value, json};
use std::time::Instant;

use crate::db::HcomDb;
use crate::log;

use super::crypto;
use super::{device_short_id_for_db, safe_kv_get, safe_kv_set, set_relay_status, state_topic};

const RETAINED_EVENT_TAIL: i64 = 50;
/// Bytes of retained tail resent with every push.
const RETAINED_TAIL_MAX_BYTES: usize = 96 * 1024;

// Room kept below a packet limit for the topic and MQTT framing, so a QoS1 publish never goes
// over the limit and permanently occupies the in-flight window. A 128 KiB peer leaves 112 KiB.
const PACKET_OVERHEAD_BYTES: usize = 16 * 1024;

/// Largest sealed payload every current receiver accepts: the smallest of this
/// device's own limit, the broker's and each live peer's. A broker drops a
/// packet over a subscriber's limit without telling the sender, so one older
/// peer keeps everyone at its size. A peer counts while its state is fresh;
/// when it leaves, the budget grows back once it goes stale.
pub(crate) fn sealed_payload_budget(db: &HcomDb) -> usize {
    let mut limit = super::MQTT_MAX_PACKET_BYTES;
    if let Some(broker) = safe_kv_get(db, super::BROKER_MAX_PACKET_KEY).and_then(|v| v.parse().ok())
    {
        limit = limit.min(broker);
    }
    let now = crate::shared::time::now_epoch_f64();
    for (key, synced) in db.kv_prefix("relay_sync_time_").unwrap_or_default() {
        let fresh = synced
            .parse::<f64>()
            .is_ok_and(|t| now - t < super::DEVICE_STALE_SECS);
        if !fresh {
            continue;
        }
        let device_id = key.trim_start_matches("relay_sync_time_");
        let peer = safe_kv_get(db, &format!("{}{device_id}", super::PEER_MAX_PACKET_PREFIX))
            .and_then(|v| v.parse().ok())
            .unwrap_or(super::LEGACY_MAX_PACKET_BYTES);
        limit = limit.min(peer);
    }
    limit.saturating_sub(PACKET_OVERHEAD_BYTES)
}

#[allow(clippy::too_many_arguments)]
fn seal_bounded_payload(
    state: &Value,
    events: &mut Vec<Value>,
    last_push_id: i64,
    psk: &[u8; 32],
    relay_id: &str,
    topic: &str,
    now_secs: u64,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    // The retained tail goes out again on every push, idle or not. Cap it on
    // its own, near what a 128 KiB publish carried, so a bigger budget speeds
    // up new events instead of resending hundreds of KiB every few seconds.
    // The state is not counted here: a large state must not strip the tail
    // that peers use to catch up and to detect gaps. The loop below still
    // enforces the combined limit.
    let event_bytes = |event: &Value| serde_json::to_vec(event).map_or(0, |v| v.len());
    let is_tail = |event: &Value| event["id"].as_i64().is_some_and(|id| id <= last_push_id);
    let mut tail_bytes: usize = events.iter().filter(|e| is_tail(e)).map(event_bytes).sum();
    while tail_bytes > RETAINED_TAIL_MAX_BYTES
        && let Some(index) = events.iter().position(is_tail)
    {
        tail_bytes -= event_bytes(&events[index]);
        events.remove(index);
    }

    loop {
        let payload = json!({
            "state": state,
            "events": events,
        });
        let payload_bytes = serde_json::to_vec(&payload).map_err(|e| format!("json: {e}"))?;
        // Sealing adds a fixed header and tag, so size the plaintext and seal
        // once it fits instead of encrypting every candidate batch.
        if crypto::HEADER_LEN + payload_bytes.len() + crypto::TAG_LEN <= max_bytes {
            return crypto::seal(psk, relay_id, topic, &payload_bytes, now_secs)
                .map_err(|e| format!("seal: {e}"));
        }

        // Retained tail events have already advanced the local cursor. Drop the oldest of those
        // first so a large tail cannot starve new events forever. If only new events remain, trim
        // from the end and publish the earliest prefix; the cursor advances only through that
        // prefix and the remainder is sent on the next drain iteration.
        if let Some(index) = events
            .iter()
            .position(|event| event["id"].as_i64().is_some_and(|id| id <= last_push_id))
        {
            events.remove(index);
        } else if events.len() > 1 {
            events.pop();
        } else if let Some(only) = events.first_mut()
            && shrink_level(only) < SHRINK_LAST_LEVEL
        {
            // One new event larger than the whole budget used to be popped
            // here. The cursor then stayed below it, every later push rebuilt
            // the same batch and dropped the same event, and only the state
            // snapshot went out: the device stopped relaying events for good
            // while its instances still looked healthy. Send a shrunk copy so
            // the cursor moves past it; the local row keeps the full event.
            shrink_event(only);
        } else {
            return Err(oversized_state_error(state, max_bytes));
        }
    }
}

/// Say what made the state too big: its size, how many agents it lists and
/// which row is largest, so a log reader can tell many rows from one huge one.
fn oversized_state_error(state: &Value, max_bytes: usize) -> String {
    let state_bytes = serde_json::to_vec(state).map_or(0, |v| v.len());
    let rows = state["instances"].as_object();
    let count = rows.map_or(0, |r| r.len());
    let largest = rows.and_then(|r| {
        r.iter()
            .map(|(name, row)| (name, serde_json::to_vec(row).map_or(0, |v| v.len())))
            .max_by_key(|(_, bytes)| *bytes)
    });
    let largest = largest
        .map(|(name, bytes)| format!(", largest {name} at {bytes} bytes"))
        .unwrap_or_default();
    format!(
        "relay state exceeds safe MQTT payload budget of {max_bytes} bytes: \
         state is {state_bytes} bytes for {count} agents{largest}"
    )
}

/// Longest string kept by the first shrink level.
const SHRINK_STRING_CHARS: usize = 4096;
/// Top-level data fields at or under this size survive the second level.
const SHRINK_KEEP_FIELD_BYTES: usize = 512;
pub(crate) const SHRINK_MARKER: &str = "_relay_truncated";
/// Fields the last level keeps: enough for a waiter to match and fail an RPC.
const SHRINK_ALWAYS_KEEP: [&str; 4] = ["request_id", "action", "ok", "error"];
pub(crate) const SHRINK_LAST_LEVEL: u64 = 3;
/// Field names listed in the marker; the rest are only counted.
const SHRINK_DROPPED_NAMES: usize = 16;

pub(crate) fn shrink_level(event: &Value) -> u64 {
    event["data"][SHRINK_MARKER]["level"].as_u64().unwrap_or(0)
}

/// Make an oversized event fit, in up to three steps. Level 1 cuts every long
/// string in `data`. Level 2 keeps only the small top-level fields (sender,
/// ids, request ids) and drops the rest. Level 3 keeps only the RPC routing
/// fields, because many small fields can still add up past the budget and
/// there must be a step that always fits (short of an oversized state). Each
/// level leaves a marker saying how big the original was, so a reader can tell
/// a relayed copy was cut. An rpc_result that loses its `result` is turned
/// into an explicit failure: `ok: true` with no result would let the requester
/// report success on an answer it never received.
pub(crate) fn shrink_event(event: &mut Value) {
    let is_rpc_result = event["type"].as_str() == Some("rpc_result");
    let original_bytes = serde_json::to_string(&event["data"])
        .map(|s| s.len())
        .unwrap_or(0);
    let level = shrink_level(event) + 1;
    let Some(data) = event.get_mut("data").and_then(|d| d.as_object_mut()) else {
        event["data"] = json!({ SHRINK_MARKER: {"level": SHRINK_LAST_LEVEL, "original_bytes": original_bytes} });
        return;
    };
    let original_bytes = data
        .get(SHRINK_MARKER)
        .and_then(|m| m["original_bytes"].as_u64())
        .map(|b| b as usize)
        .unwrap_or(original_bytes);
    // Read what earlier levels dropped BEFORE removing their marker.
    let already_dropped = event_marker_dropped(data).unwrap_or_default();
    let already_count = data
        .get(SHRINK_MARKER)
        .and_then(|m| m["dropped_count"].as_u64())
        .unwrap_or(0) as usize;
    data.remove(SHRINK_MARKER);
    let mut dropped: Vec<String> = Vec::new();
    if level == 1 {
        for value in data.values_mut() {
            shrink_strings(value);
        }
    } else {
        let doomed: Vec<String> = data
            .iter()
            .filter(|(k, v)| {
                if level >= SHRINK_LAST_LEVEL {
                    !SHRINK_ALWAYS_KEEP.contains(&k.as_str())
                } else {
                    serde_json::to_string(v)
                        .map(|s| s.len())
                        .unwrap_or(usize::MAX)
                        > SHRINK_KEEP_FIELD_BYTES
                }
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in doomed {
            data.remove(&key);
            dropped.push(key);
        }
    }
    if is_rpc_result
        && (dropped.iter().any(|k| k == "result") || already_dropped.iter().any(|k| k == "result"))
    {
        data.insert("ok".to_string(), json!(false));
        let error = format!(
            "rpc result too large to relay ({original_bytes} bytes); the relayed copy was cut"
        );
        // Callers and backfill read the reason from `result.error`; a top-level `error`
        // alone left them reporting "unknown remote error".
        data.insert("result".to_string(), json!({ "error": error }));
        match data.get_mut("error") {
            Some(Value::String(existing)) if existing.contains(&error) => {}
            Some(Value::String(existing)) if !existing.is_empty() => {
                existing.push_str("; ");
                existing.push_str(&error);
            }
            _ => {
                data.insert("error".to_string(), json!(error));
            }
        }
    }
    // Bounded: a level-3 shrink of an event with thousands of small fields would otherwise
    // copy every name into the marker, which could overflow the budget the shrink exists to
    // meet. The count stays exact; `result` is listed first so a cut RPC answer is always
    // named.
    let mut total_dropped = already_count.max(already_dropped.len());
    let mut all_dropped = already_dropped;
    for key in dropped {
        if !all_dropped.contains(&key) {
            all_dropped.push(key);
            total_dropped += 1;
        }
    }
    if let Some(i) = all_dropped.iter().position(|k| k == "result") {
        let result = all_dropped.remove(i);
        all_dropped.insert(0, result);
    }
    all_dropped.truncate(SHRINK_DROPPED_NAMES);
    let mut marker = json!({"level": level, "original_bytes": original_bytes});
    if !all_dropped.is_empty() {
        marker["dropped"] = json!(all_dropped);
    }
    if total_dropped > all_dropped.len() {
        marker["dropped_count"] = json!(total_dropped);
    }
    data.insert(SHRINK_MARKER.to_string(), marker);
}

/// Field names an earlier shrink level already dropped, so the marker keeps the
/// full list across levels.
fn event_marker_dropped(data: &serde_json::Map<String, Value>) -> Option<Vec<String>> {
    data.get(SHRINK_MARKER)?["dropped"].as_array().map(|items| {
        items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    })
}

fn shrink_strings(value: &mut Value) {
    match value {
        Value::String(s) => {
            let chars = s.chars().count();
            if chars > SHRINK_STRING_CHARS {
                let kept: String = s.chars().take(SHRINK_STRING_CHARS).collect();
                *s = format!(
                    "{kept}\n[truncated by hcom relay: {chars} characters, too large to relay]"
                );
            }
        }
        Value::Array(items) => items.iter_mut().for_each(shrink_strings),
        Value::Object(map) => map.values_mut().for_each(shrink_strings),
        _ => {}
    }
}

/// Account e-mail for a local agent, read from the tool config dir it was
/// launched with. Only sent when `relay_share_account` is on. A context
/// captured without the dir key (bound by an older hcom) leaves the account
/// unknown: the default account may not be the agent's.
fn shared_account_email(tool: Option<&str>, launch_context: Option<&str>) -> Option<String> {
    use std::sync::{LazyLock, Mutex};
    static EMAILS: LazyLock<Mutex<crate::accounts::EmailCache>> = LazyLock::new(Default::default);
    let tool: crate::tool::Tool = tool.unwrap_or("claude").parse().ok()?;
    let key = crate::accounts::config_dir_env_key(tool)?;
    let context = serde_json::from_str::<Value>(launch_context?).ok()?;
    // "" is the tool's default dir.
    let dir = context.get("env")?.get(key)?.as_str()?.to_string();
    let dir = Some(dir).filter(|d| !d.is_empty());
    EMAILS
        .lock()
        .ok()?
        .email(tool, dir.as_deref().map(std::path::Path::new))
}

/// Build current instance state snapshot for publishing.
/// Only includes local instances (no origin_device_id).
/// `share_account` is the `relay_share_account` setting.
pub fn build_state(db: &HcomDb, device_uuid: &str, share_account: bool) -> Value {
    let short_id = device_short_id_for_db(db, device_uuid);

    let instances = match db.conn().prepare(
        "SELECT name, status, status_context, status_detail, status_time, parent_name,
                directory, transcript_path,
                wait_timeout, last_stop, tcp_mode, tag, tool, background, launch_context
         FROM instances WHERE COALESCE(origin_device_id, '') = ''",
    ) {
        Ok(mut stmt) => {
            let rows: Vec<_> = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,          // name
                        row.get::<_, Option<String>>(1)?,  // status
                        row.get::<_, Option<String>>(2)?,  // status_context
                        row.get::<_, Option<String>>(3)?,  // status_detail
                        row.get::<_, Option<f64>>(4)?,     // status_time
                        row.get::<_, Option<String>>(5)?,  // parent_name
                        row.get::<_, Option<String>>(6)?,  // directory
                        row.get::<_, Option<String>>(7)?,  // transcript_path
                        row.get::<_, Option<i64>>(8)?,     // wait_timeout
                        row.get::<_, Option<f64>>(9)?,     // last_stop
                        row.get::<_, Option<bool>>(10)?,   // tcp_mode
                        row.get::<_, Option<String>>(11)?, // tag
                        row.get::<_, Option<String>>(12)?, // tool
                        row.get::<_, Option<bool>>(13)?,   // background
                        row.get::<_, Option<String>>(14)?, // launch_context
                    ))
                })
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default();

            let mut entries = Vec::new();
            for row in rows {
                let name = &row.0;
                // Skip internal instances
                if name.starts_with('_') || name.starts_with("sys_") {
                    continue;
                }
                let account = if share_account {
                    shared_account_email(row.12.as_deref(), row.14.as_deref())
                } else {
                    None
                };
                let mut entry = json!({
                        "enabled": true,
                        "status": row.1.as_deref().unwrap_or("unknown"),
                        "context": row.2.as_deref().unwrap_or(""),
                        "status_time": row.4.unwrap_or(0.0),
                        "parent": row.5,
                        "directory": row.6,
                        "transcript": row.7,
                        "wait_timeout": row.8.unwrap_or(86400),
                        "last_stop": row.9.unwrap_or(0.0),
                        "tcp_mode": row.10.unwrap_or(false),
                        "tag": row.11,
                        "tool": row.12.as_deref().unwrap_or("claude"),
                        "background": row.13.unwrap_or(false),
                });
                if let Some(email) = account {
                    entry["account_email"] = json!(email);
                }
                entries.push((name.clone(), row.3.clone().unwrap_or_default(), entry));
            }
            Value::Object(with_detail_previews(db, &short_id, entries))
        }
        Err(_) => json!({}),
    };

    // Get reset timestamp (local only — exclude imported events)
    let reset_ts = db
        .conn()
        .query_row(
            "SELECT timestamp FROM events
             WHERE type = 'life' AND instance = '_device'
             AND json_extract(data, '$.action') = 'reset'
             AND json_extract(data, '$._relay') IS NULL
             ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .and_then(|ts| parse_iso_timestamp_to_epoch(&ts))
        .unwrap_or(0.0);
    let capabilities = json!(super::control::advertised_remote_capabilities());

    json!({
        "instances": instances,
        "short_id": short_id,
        "reset_ts": reset_ts,
        "capabilities": capabilities,
        "max_packet": super::MQTT_MAX_PACKET_BYTES,
    })
}

/// Fill in each snapshot row's status detail: whole when short, otherwise a
/// marked preview naming the command that fetches the rest. Previews start at
/// the longest level and get shorter while the snapshot would take more than
/// three quarters of a publish, so many long details cannot crowd events out or
/// fail the push. Rows are never dropped. Only the display-only snapshot is
/// cut; relayed status events keep the full text so subscriptions and filters
/// on remote events still match all of it.
fn with_detail_previews(
    db: &HcomDb,
    short_id: &str,
    entries: Vec<(String, String, Value)>,
) -> serde_json::Map<String, Value> {
    // Leave a quarter of the publish for events.
    let target = sealed_payload_budget(db) / 4 * 3;
    let mut map = serde_json::Map::new();
    let mut used = super::DETAIL_PREVIEW_LEVELS[0];
    for &max_chars in &super::DETAIL_PREVIEW_LEVELS {
        used = max_chars;
        map = entries
            .iter()
            .map(|(name, detail, row)| {
                let mut row = row.clone();
                let preview = (detail.chars().count() > max_chars)
                    .then(|| {
                        let cmd = format!(
                            "hcom events --remote-fetch --device {short_id} --agent {name} --type status --last 1 --full"
                        );
                        super::preview_detail(detail, max_chars, &cmd)
                    })
                    .flatten();
                row["detail"] = json!(preview.unwrap_or_else(|| detail.clone()));
                (name.clone(), row)
            })
            .collect();
        if serde_json::to_vec(&map).map_or(0, |v| v.len()) <= target {
            break;
        }
    }
    log_preview_level(used);
    map
}

/// Log when previews get shorter or recover, not on every push.
fn log_preview_level(max_chars: usize) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static LAST: AtomicUsize = AtomicUsize::new(super::DETAIL_PREVIEW_LEVELS[0]);
    if LAST.swap(max_chars, Ordering::Relaxed) != max_chars {
        log::log_warn(
            "relay",
            "relay.state_previews_resized",
            &format!(
                "status detail previews now {max_chars} chars to keep the snapshot within the publish budget"
            ),
        );
    }
}

/// Build push payload: state + events, returning (state, events, max_event_id, has_more).
/// Fetches 101 rows, sends first 100 — has_more=true if 101st exists.
pub fn build_push_payload(
    db: &HcomDb,
    device_uuid: &str,
    share_account: bool,
) -> (Value, Vec<Value>, i64, bool) {
    let state = build_state(db, device_uuid, share_account);

    let last_push_id: i64 = safe_kv_get(db, "relay_last_push_id")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let tail_start_id = last_push_id.saturating_sub(RETAINED_EVENT_TAIL);

    let rows: Vec<(i64, String, String, String, String)> = db
        .conn()
        .prepare(
            "SELECT id, timestamp, type, instance, data FROM events
             WHERE id > ? AND instance NOT LIKE '%:%'
             AND instance != '_device'
             AND json_extract(data, '$._relay') IS NULL
             ORDER BY id LIMIT 101",
        )
        .ok()
        .map(|mut stmt| {
            stmt.query_map(rusqlite::params![tail_start_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        })
        .unwrap_or_default();

    let has_more = rows.len() > 100;
    let send_rows = &rows[..rows.len().min(100)];

    let mut events = Vec::new();
    let mut max_id = last_push_id;

    for (id, ts, event_type, instance, data_str) in send_rows {
        let data: Value = serde_json::from_str(data_str).unwrap_or(json!({}));
        events.push(json!({
            "id": id,
            "ts": ts,
            "type": event_type,
            "instance": instance,
            "data": data,
        }));
        if *id > last_push_id {
            max_id = max_id.max(*id);
        }
    }

    (state, events, max_id, has_more)
}

/// Events past the cursor (the retained tail is at or below it).
fn new_event_count(events: &[Value], last_push_id: i64) -> usize {
    events
        .iter()
        .filter(|event| event["id"].as_i64().is_some_and(|id| id > last_push_id))
        .count()
}

/// Whether another drain iteration has work. Only NEW events count: a retained-tail event
/// dropped to fit the budget was already sent, and counting it as "more" made the drain loop
/// republish the same state for its whole time budget after every oversized event, then the
/// periodic timer started another drain.
fn more_to_send(
    source_has_more: bool,
    source_new_events: usize,
    sent: &[Value],
    last_push_id: i64,
) -> bool {
    source_has_more || new_event_count(sent, last_push_id) < source_new_events
}

/// Push state and events via MQTT. Returns (success, has_more).
/// `is_worker` should be true when called from the daemon relay thread.
/// Only call while the MQTT connection is live: the cursor advances on enqueue.
pub fn push(
    db: &HcomDb,
    client: &Client,
    relay_id: &str,
    device_uuid: &str,
    psk: &[u8; 32],
    share_account: bool,
    is_worker: bool,
) -> Result<(bool, bool), String> {
    let last_push_id = safe_kv_get(db, "relay_last_push_id")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let (state, mut events, _unbounded_max_id, source_has_more) =
        build_push_payload(db, device_uuid, share_account);
    let source_new_events = new_event_count(&events, last_push_id);

    let topic = state_topic(relay_id, device_uuid);
    let now_secs = crate::shared::time::now_epoch_f64() as u64;
    let sealed = seal_bounded_payload(
        &state,
        &mut events,
        last_push_id,
        psk,
        relay_id,
        &topic,
        now_secs,
        sealed_payload_budget(db),
    )?;
    let payload_len = sealed.len();
    let max_id = events
        .iter()
        .filter_map(|event| event["id"].as_i64())
        .fold(last_push_id, i64::max);
    let has_more = more_to_send(source_has_more, source_new_events, &events, last_push_id);

    let t0 = Instant::now();

    // Never block the worker loop: a full queue means the connection is
    // stalled, and the loop must stay free to observe its error event.
    client
        .try_publish(&topic, sealed, PublishOptions::at_least_once().retained())
        .map_err(|e| format!("publish: {}", e))?;

    let publish_ms = t0.elapsed().as_millis();

    let now = crate::shared::time::now_epoch_f64();
    safe_kv_set(db, "relay_last_push", Some(&now.to_string()));
    safe_kv_set(db, "relay_last_push_id", Some(&max_id.to_string()));
    safe_kv_set(db, "relay_last_sync", Some(&now.to_string()));
    set_relay_status(db, "ok", None, is_worker);

    log::log_with_fields(
        "INFO",
        "relay",
        "relay.push",
        "",
        &[
            ("events", &events.len().to_string()),
            ("publish_ms", &publish_ms.to_string()),
            ("payload_bytes", &payload_len.to_string()),
        ],
    );

    Ok((true, has_more))
}

/// Parse ISO 8601 timestamp to Unix epoch seconds.
fn parse_iso_timestamp_to_epoch(ts: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .or_else(|_| chrono::DateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ"))
        .ok()
        .map(|dt| dt.timestamp() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::HcomDb;
    use serde_json::json;

    /// The budget toward a peer on an older hcom.
    const MAX_SEALED_PAYLOAD_BYTES: usize = 112 * 1024;

    #[test]
    fn test_parse_iso_timestamp_to_epoch() {
        // RFC 3339
        let ts = parse_iso_timestamp_to_epoch("2024-01-01T00:00:00+00:00");
        assert!(ts.is_some());
        assert!(ts.unwrap() > 0.0);

        // Simple ISO format
        let ts = parse_iso_timestamp_to_epoch("2024-01-01T00:00:00Z");
        assert!(ts.is_some());

        // Invalid
        assert!(parse_iso_timestamp_to_epoch("not a date").is_none());
    }

    #[test]
    fn build_push_payload_includes_recent_retained_tail() {
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();

        let old_id = db
            .log_event("message", "old", &json!({"text": "old"}))
            .unwrap();
        let recent_id = db
            .log_event("message", "recent", &json!({"text": "recent"}))
            .unwrap();
        safe_kv_set(&db, "relay_last_push_id", Some(&recent_id.to_string()));

        let (_state, events, max_id, has_more) = build_push_payload(&db, "device-a", false);

        assert!(!has_more);
        assert_eq!(max_id, recent_id);
        assert!(
            events
                .iter()
                .any(|event| event["id"].as_i64() == Some(old_id)),
            "retained snapshot should include recent already-pushed events"
        );
        assert!(
            events
                .iter()
                .any(|event| event["id"].as_i64() == Some(recent_id))
        );
    }

    #[test]
    fn sealed_payload_is_byte_bounded_and_new_events_advance() {
        let state = json!({"instances": {}});
        let last_push_id = 50;
        let mut events: Vec<Value> = (1..=100)
            .map(|id| json!({"id": id, "data": {"text": "x".repeat(4096)}}))
            .collect();
        let sealed = seal_bounded_payload(
            &state,
            &mut events,
            last_push_id,
            &[0x42; 32],
            "relay-test",
            "relay-test/device-test",
            1_700_000_000,
            MAX_SEALED_PAYLOAD_BYTES,
        )
        .unwrap();

        assert!(sealed.len() <= MAX_SEALED_PAYLOAD_BYTES);
        assert!(events.len() < 100);
        assert!(
            events
                .iter()
                .all(|event| event["id"].as_i64().unwrap() > last_push_id)
        );
        assert_eq!(events.first().unwrap()["id"].as_i64(), Some(51));
        assert!(events.last().unwrap()["id"].as_i64().unwrap() > last_push_id);
    }

    #[test]
    fn a_single_oversized_new_event_is_shrunk_so_the_cursor_moves_past_it() {
        // Before the fix this event was popped, the payload went out with no
        // events, and every later push repeated that: the device never relayed
        // another event.
        let state = json!({"instances": {}});
        let last_push_id = 70;
        let mut events = vec![
            json!({"id": 60, "data": {"text": "tail"}}),
            json!({"id": 71, "type": "message", "data": {
                "from": "luna", "text": "y".repeat(MAX_SEALED_PAYLOAD_BYTES * 2)
            }}),
        ];
        let sealed = seal_bounded_payload(
            &state,
            &mut events,
            last_push_id,
            &[0x42; 32],
            "relay-test",
            "relay-test/device-test",
            1_700_000_000,
            MAX_SEALED_PAYLOAD_BYTES,
        )
        .unwrap();

        assert!(sealed.len() <= MAX_SEALED_PAYLOAD_BYTES);
        assert_eq!(events.len(), 1);
        let only = &events[0];
        assert_eq!(only["id"].as_i64(), Some(71), "the new event is still sent");
        assert_eq!(only["data"]["from"], "luna");
        let text = only["data"]["text"].as_str().unwrap();
        assert!(text.contains("truncated by hcom relay"), "marker present");
        assert!(text.len() < 10_000);
        assert_eq!(only["data"]["_relay_truncated"]["level"], 1);
        assert_eq!(
            only["data"]["_relay_truncated"]["original_bytes"].as_u64(),
            Some((MAX_SEALED_PAYLOAD_BYTES * 2 + r#"{"from":"luna","text":""}"#.len()) as u64)
        );
    }

    #[test]
    fn many_small_strings_fall_back_to_keeping_only_small_fields() {
        // An events RPC answer from an older peer: no single long string, but
        // too large overall. Level 2 keeps request_id so the waiter learns of
        // the failure instead of timing out.
        let state = json!({"instances": {}});
        let rows: Vec<Value> = (0..4000)
            .map(|i| json!({"id": i, "data": {"text": format!("message {i} ").repeat(4)}}))
            .collect();
        let mut events = vec![json!({"id": 5, "type": "rpc_result", "data": {
            "request_id": "req-1", "action": "events", "ok": true,
            "result": {"events": rows, "count": 4000}
        }})];
        seal_bounded_payload(
            &state,
            &mut events,
            4,
            &[0x42; 32],
            "relay-test",
            "relay-test/device-test",
            1_700_000_000,
            MAX_SEALED_PAYLOAD_BYTES,
        )
        .unwrap();

        let data = &events[0]["data"];
        assert_eq!(data["request_id"], "req-1");
        assert!(
            data["result"].get("events").is_none(),
            "the oversized result is gone"
        );
        assert_eq!(data["_relay_truncated"]["level"], 2);
        assert_eq!(data["_relay_truncated"]["dropped"], json!(["result"]));
        // The requester must see a FAILURE, never ok:true with the result silently gone,
        // and the reason must be where callers read it: result.error.
        assert_eq!(data["ok"], false);
        assert!(
            data["result"]["error"]
                .as_str()
                .unwrap()
                .contains("too large to relay")
        );
    }

    #[test]
    fn many_small_fields_reach_a_last_level_that_always_fits() {
        // Thousands of top-level fields, each under the level-2 keep size: level 2 drops
        // nothing, so without a third level the relay stopped publishing for good.
        let state = json!({"instances": {}});
        let mut data = serde_json::Map::new();
        data.insert("request_id".into(), json!("req-9"));
        data.insert("action".into(), json!("events"));
        data.insert("ok".into(), json!(true));
        for i in 0..6000 {
            data.insert(format!("k{i:05}"), json!("v".repeat(40)));
        }
        let mut events = vec![json!({"id": 9, "type": "rpc_result", "data": data})];
        let sealed = seal_bounded_payload(
            &state,
            &mut events,
            8,
            &[0x42; 32],
            "relay-test",
            "relay-test/device-test",
            1_700_000_000,
            MAX_SEALED_PAYLOAD_BYTES,
        )
        .unwrap();

        assert!(sealed.len() <= MAX_SEALED_PAYLOAD_BYTES);
        let data = events[0]["data"].as_object().unwrap();
        assert_eq!(data["_relay_truncated"]["level"], 3);
        assert_eq!(data["request_id"], "req-9");
        assert_eq!(data["action"], "events");
        assert_eq!(
            data["ok"], true,
            "no result field was ever present, so nothing was lost"
        );
        let mut keys: Vec<&str> = data.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["_relay_truncated", "action", "ok", "request_id"]);
        // The marker itself stays small: 6000 dropped names are counted, not listed.
        let marker = &data["_relay_truncated"];
        assert!(marker["dropped"].as_array().unwrap().len() <= 16);
        assert_eq!(marker["dropped_count"], 6000);
    }

    #[test]
    fn an_rpc_result_that_loses_its_result_at_any_level_is_a_failure() {
        let mut event = json!({"id": 3, "type": "rpc_result", "data": {
            "request_id": "req-2", "ok": true, "result": {"big": "x".repeat(2000)}
        }});
        shrink_event(&mut event); // level 1: strings cut, result kept
        assert_eq!(event["data"]["ok"], true);
        shrink_event(&mut event); // level 2: result (> 512 B) dropped
        assert_eq!(event["data"]["ok"], false);
        shrink_event(&mut event); // level 3: the dropped list survives across levels
        assert_eq!(event["data"]["ok"], false);
        assert_eq!(
            event["data"]["_relay_truncated"]["dropped"],
            json!(["result"])
        );
        let reason = event["data"]["result"]["error"].as_str().unwrap();
        assert!(reason.contains("too large to relay"), "{reason}");
        let top = event["data"]["error"].as_str().unwrap();
        assert_eq!(top.matches("too large to relay").count(), 1, "{top}");
        // A non-RPC event is never rewritten into a failure.
        let mut message = json!({"id": 4, "type": "message", "data": {"text": "y".repeat(2000)}});
        shrink_event(&mut message);
        shrink_event(&mut message);
        assert!(message["data"].get("ok").is_none());
    }

    #[test]
    fn a_dropped_tail_event_is_not_more_work_but_a_dropped_new_event_is() {
        let tail = json!({"id": 10, "data": {}});
        let new_a = json!({"id": 11, "data": {}});
        let new_b = json!({"id": 12, "data": {}});
        let source = vec![tail.clone(), new_a.clone(), new_b.clone()];
        let source_new = new_event_count(&source, 10);
        assert_eq!(source_new, 2);
        // The oversized tail row was dropped to fit: both new events went out.
        assert!(!more_to_send(
            false,
            source_new,
            &[new_a.clone(), new_b.clone()],
            10
        ));
        // A new event was trimmed: the drain loop must run again.
        assert!(more_to_send(false, source_new, &[tail, new_a], 10));
        // The source itself had more rows.
        assert!(more_to_send(true, source_new, &[new_b], 10));
    }

    #[test]
    fn oversized_state_fails_instead_of_publishing_an_invalid_packet() {
        let state = json!({"instances": {"oversized": "x".repeat(MAX_SEALED_PAYLOAD_BYTES)}});
        let mut events = Vec::new();
        let error = seal_bounded_payload(
            &state,
            &mut events,
            0,
            &[0x42; 32],
            "relay-test",
            "relay-test/device-test",
            1_700_000_000,
            MAX_SEALED_PAYLOAD_BYTES,
        )
        .unwrap_err();

        assert!(error.contains("exceeds safe MQTT payload budget"));
        assert!(
            error.contains("for 1 agents, largest oversized at"),
            "{error}"
        );
    }

    #[test]
    fn a_large_budget_does_not_resend_a_large_retained_tail() {
        let state = json!({"instances": {}});
        let big = |id: i64| json!({"id": id, "data": {"text": "x".repeat(30_000)}});
        // Ten 30 KB events at or below the cursor (tail) and two new ones.
        let mut events: Vec<Value> = (1..=12).map(big).collect();
        seal_bounded_payload(
            &state,
            &mut events,
            10,
            &[0x42; 32],
            "relay-test",
            "relay-test/device-test",
            1_700_000_000,
            crate::relay::MQTT_MAX_PACKET_BYTES - PACKET_OVERHEAD_BYTES,
        )
        .unwrap();
        let tail = events.iter().filter(|e| e["id"].as_i64().unwrap() <= 10);
        let tail_bytes: usize = tail.map(|e| serde_json::to_vec(e).unwrap().len()).sum();
        assert!(tail_bytes <= MAX_SEALED_PAYLOAD_BYTES, "{tail_bytes}");
        // New events still use the full budget.
        assert!(events.iter().any(|e| e["id"] == 11));
        assert!(events.iter().any(|e| e["id"] == 12));
    }

    #[test]
    fn budget_fits_the_smallest_live_receiver() {
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        let now = crate::shared::time::now_epoch_f64();
        let full = crate::relay::MQTT_MAX_PACKET_BYTES - PACKET_OVERHEAD_BYTES;
        let set = |key: &str, value: Option<&str>| safe_kv_set(&db, key, value);

        // Nobody else on the relay: only our own limit applies.
        assert_eq!(sealed_payload_budget(&db), full);

        // A live peer that announces 1 MiB keeps the full budget.
        set("relay_sync_time_new", Some(&now.to_string()));
        set("relay_max_packet_new", Some("1048576"));
        assert_eq!(sealed_payload_budget(&db), full);

        // A live peer that announces nothing is an older hcom: 128 KiB.
        set("relay_sync_time_old", Some(&now.to_string()));
        assert_eq!(sealed_payload_budget(&db), MAX_SEALED_PAYLOAD_BYTES);

        // Once it goes stale it no longer holds the budget down.
        set("relay_sync_time_old", Some(&(now - 600.0).to_string()));
        assert_eq!(sealed_payload_budget(&db), full);

        // The broker's own limit caps everyone.
        set(crate::relay::BROKER_MAX_PACKET_KEY, Some("262144"));
        assert_eq!(
            sealed_payload_budget(&db),
            256 * 1024 - PACKET_OVERHEAD_BYTES
        );
    }

    #[test]
    fn snapshot_details_are_previews_but_relayed_status_events_are_whole() {
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        let long = "x".repeat(5000);
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_detail,
                                        status_time, created_at)
                 VALUES ('nova', 'sess-1', 'claude', 'active', ?, 1, 0)",
                [&long],
            )
            .unwrap();
        let event_id = db
            .log_event(
                "status",
                "nova",
                &json!({"status": "active", "detail": long}),
            )
            .unwrap();

        let state = build_state(&db, "device-test", false);
        let row_detail = state["instances"]["nova"]["detail"].as_str().unwrap();
        let cmd = crate::relay::preview_fetch_command(row_detail).unwrap();
        assert!(
            cmd.contains("--agent nova --type status --last 1 --full"),
            "{cmd}"
        );
        assert!(row_detail.len() < 1000);

        // Relayed status events keep the whole text for subscriptions and filters.
        let (_, events, _, _) = build_push_payload(&db, "device-test", false);
        let event = events.iter().find(|e| e["id"] == event_id).unwrap();
        assert_eq!(event["data"]["detail"].as_str().unwrap().len(), 5000);

        // The local row keeps the whole text.
        let local: String = db
            .conn()
            .query_row(
                "SELECT status_detail FROM instances WHERE name = 'nova'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(local.len(), 5000);
    }

    #[test]
    fn many_long_details_shorten_every_preview_instead_of_failing_the_push() {
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        // An older peer holds the budget at 112 KiB.
        safe_kv_set(
            &db,
            "relay_sync_time_old",
            Some(&crate::shared::time::now_epoch_f64().to_string()),
        );
        let long = "é".repeat(2000);
        for i in 0..150 {
            db.conn()
                .execute(
                    "INSERT INTO instances (name, session_id, tool, status, status_detail,
                                            status_time, created_at)
                     VALUES (?, ?, 'claude', 'active', ?, 1, 0)",
                    rusqlite::params![format!("a{i}"), format!("s{i}"), long],
                )
                .unwrap();
        }
        let state = build_state(&db, "device-test", false);
        let rows = state["instances"].as_object().unwrap();
        assert_eq!(rows.len(), 150, "no row is dropped");
        assert!(
            serde_json::to_vec(&state["instances"]).unwrap().len()
                <= MAX_SEALED_PAYLOAD_BYTES / 4 * 3
        );
        let detail = rows["a0"]["detail"].as_str().unwrap();
        assert!(
            !detail.starts_with(&"é".repeat(400)),
            "shorter than the first level"
        );
        assert!(crate::relay::preview_fetch_command(detail).is_some());
    }

    #[test]
    fn a_large_state_keeps_the_retained_tail() {
        let state = json!({"instances": {"big": "s".repeat(150_000)}});
        let mut events = vec![
            json!({"id": 1, "data": {"text": "tail"}}),
            json!({"id": 2, "data": {"text": "new"}}),
        ];
        seal_bounded_payload(
            &state,
            &mut events,
            1,
            &[0x42; 32],
            "relay-test",
            "relay-test/device-test",
            1_700_000_000,
            crate::relay::MQTT_MAX_PACKET_BYTES - PACKET_OVERHEAD_BYTES,
        )
        .unwrap();
        assert_eq!(
            events.len(),
            2,
            "room is left, so the tail stays for gap detection"
        );
    }

    #[test]
    fn state_announces_this_devices_packet_limit() {
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        let state = build_state(&db, "device-test", false);
        assert_eq!(
            state["max_packet"].as_u64(),
            Some(crate::relay::MQTT_MAX_PACKET_BYTES as u64)
        );
    }

    #[test]
    fn state_shares_account_emails_only_when_on() {
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        let claude_dir = dir.path().join("claude");
        std::fs::create_dir(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join(".claude.json"),
            r#"{"oauthAccount":{"emailAddress":"luna@example.com"}}"#,
        )
        .unwrap();
        let with_dir = json!({"env": {"CLAUDE_CONFIG_DIR": claude_dir}}).to_string();
        // Bound before the dir key was captured: the account is unknown.
        let old = json!({"env": {"TERM_PROGRAM": "x"}}).to_string();
        for (name, ctx) in [("luna", with_dir), ("nova", old)] {
            db.conn()
                .execute(
                    "INSERT INTO instances (name, session_id, tool, status, status_time,
                                            created_at, launch_context)
                     VALUES (?, ?, 'claude', 'active', 1, 0, ?)",
                    rusqlite::params![name, format!("s-{name}"), ctx],
                )
                .unwrap();
        }

        let on = build_state(&db, "device-test", true);
        assert_eq!(
            on["instances"]["luna"]["account_email"].as_str(),
            Some("luna@example.com")
        );
        assert!(on["instances"]["nova"].get("account_email").is_none());

        let off = build_state(&db, "device-test", false);
        assert!(off["instances"]["luna"].get("account_email").is_none());
    }

    #[test]
    fn state_includes_quiet_children() {
        // A stale child may still be alive; peers need its row to reach it.
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        db.conn()
            .execute_batch(
                "INSERT INTO instances (name, session_id, tool, status, status_time, created_at)
                 VALUES ('nova', 'sess-1', 'claude', 'active', 1, 0);
                 INSERT INTO instances (name, parent_session_id, parent_name, agent_id, tool,
                                        status, status_context, status_time, created_at)
                 VALUES ('nova_task_1', 'sess-1', 'nova', 'a1', 'claude', 'inactive',
                         'subagent:stale', 1, 0);",
            )
            .unwrap();
        let state = build_state(&db, "device-test", false);
        let instances = state["instances"].as_object().unwrap();
        assert!(instances.contains_key("nova"));
        assert!(instances.contains_key("nova_task_1"));
    }

    #[test]
    fn push_fails_fast_on_a_full_queue_instead_of_blocking_the_worker() {
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        let options = rumqttc::MqttOptions::new("push-test", ("127.0.0.1", 1883));
        // Unpolled connection: the one-slot queue never drains.
        let (client, _connection) = Client::builder(options).capacity(1).build();
        client
            .try_publish("relay/device", "queued", PublishOptions::at_least_once())
            .unwrap();

        let start = Instant::now();
        let error = push(&db, &client, "relay", "device", &[0x42; 32], false, false).unwrap_err();
        assert!(error.starts_with("publish:"), "{error}");
        // A blocking publish would wait forever; the slack only absorbs a
        // loaded debug build's db and seal work.
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }
}
