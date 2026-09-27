//! Push loop — build state snapshot and events, publish via MQTT.
//!
//! Batches up to 100 events per publish with a 10s drain budget.
//! Tracks progress via KV cursor `relay_last_push_id`.

use rumqttc::v5::Client;
use rumqttc::v5::mqttbytes::QoS;
use serde_json::{Value, json};
use std::time::Instant;

use crate::db::HcomDb;
use crate::log;

use super::crypto;
use super::{device_short_id_for_db, safe_kv_get, safe_kv_set, set_relay_status, state_topic};

const RETAINED_EVENT_TAIL: i64 = 50;

// MQTT clients advertise a 128 KiB maximum packet. Keep the encrypted application payload
// comfortably below that so the topic and MQTT framing cannot push a QoS1 publish over the
// broker limit and permanently occupy the in-flight window.
const MAX_SEALED_PAYLOAD_BYTES: usize = 112 * 1024;

fn seal_bounded_payload(
    state: &Value,
    events: &mut Vec<Value>,
    last_push_id: i64,
    psk: &[u8; 32],
    relay_id: &str,
    topic: &str,
    now_secs: u64,
) -> Result<Vec<u8>, String> {
    loop {
        let payload = json!({
            "state": state,
            "events": events,
        });
        let payload_bytes = serde_json::to_vec(&payload).map_err(|e| format!("json: {e}"))?;
        let sealed = crypto::seal(psk, relay_id, topic, &payload_bytes, now_secs)
            .map_err(|e| format!("seal: {e}"))?;
        if sealed.len() <= MAX_SEALED_PAYLOAD_BYTES {
            return Ok(sealed);
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
            return Err(format!(
                "relay state exceeds safe MQTT payload budget of {MAX_SEALED_PAYLOAD_BYTES} bytes"
            ));
        }
    }
}

/// Longest string kept by the first shrink level.
const SHRINK_STRING_CHARS: usize = 4096;
/// Top-level data fields at or under this size survive the second level.
const SHRINK_KEEP_FIELD_BYTES: usize = 512;
const SHRINK_MARKER: &str = "_relay_truncated";
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
        // alone left them reporting "unknown remote error" (upstream review of #144).
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
    // named (upstream review of #144, round 3).
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

/// Build current instance state snapshot for publishing.
/// Only includes local instances (no origin_device_id).
pub fn build_state(db: &HcomDb, device_uuid: &str) -> Value {
    let short_id = device_short_id_for_db(db, device_uuid);

    let instances = match db.conn().prepare(
        "SELECT name, status, status_context, status_detail, status_time, parent_name,
                directory, transcript_path,
                wait_timeout, last_stop, tcp_mode, tag, tool, background
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
                    ))
                })
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default();

            let mut map = serde_json::Map::new();
            for row in rows {
                let name = &row.0;
                // Skip internal instances
                if name.starts_with('_') || name.starts_with("sys_") {
                    continue;
                }
                map.insert(
                    name.clone(),
                    json!({
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
                        "detail": row.3.as_deref().unwrap_or(""),
                    }),
                );
            }
            Value::Object(map)
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
    })
}

/// Build push payload: state + events, returning (state, events, max_event_id, has_more).
/// Fetches 101 rows, sends first 100 — has_more=true if 101st exists.
pub fn build_push_payload(db: &HcomDb, device_uuid: &str) -> (Value, Vec<Value>, i64, bool) {
    let state = build_state(db, device_uuid);

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
/// periodic timer started another drain (upstream review of PR #144).
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
/// `mqtt_connected` indicates whether the MQTT connection is known to be live.
/// When false, events are still published (rumqttc may buffer and deliver on
/// reconnect) but the cursor is NOT advanced — events will be re-sent on the
/// next push after the connection recovers, preventing silent event loss.
pub fn push(
    db: &HcomDb,
    client: &Client,
    relay_id: &str,
    device_uuid: &str,
    psk: &[u8; 32],
    is_worker: bool,
    mqtt_connected: bool,
) -> Result<(bool, bool), String> {
    let last_push_id = safe_kv_get(db, "relay_last_push_id")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let (state, mut events, _unbounded_max_id, source_has_more) =
        build_push_payload(db, device_uuid);
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
    )?;
    let payload_len = sealed.len();
    let max_id = events
        .iter()
        .filter_map(|event| event["id"].as_i64())
        .fold(last_push_id, i64::max);
    let has_more = more_to_send(source_has_more, source_new_events, &events, last_push_id);

    let t0 = Instant::now();

    // Enqueue into rumqttc's internal channel. With QoS::AtLeastOnce rumqttc
    // handles retransmission if the connection is live. When disconnected,
    // the message may sit in the internal buffer and be delivered on reconnect,
    // but we cannot guarantee it — so we only advance the cursor when
    // mqtt_connected is true.
    let props = super::signing::publish_properties(&sealed).unwrap_or_default();
    client
        .publish_with_properties(&topic, QoS::AtLeastOnce, true, sealed, props)
        .map_err(|e| format!("publish: {}", e))?;

    let publish_ms = t0.elapsed().as_millis();

    if mqtt_connected {
        // Connection is live — advance cursor so these events aren't re-sent.
        let now = crate::shared::time::now_epoch_f64();
        safe_kv_set(db, "relay_last_push", Some(&now.to_string()));
        safe_kv_set(db, "relay_last_push_id", Some(&max_id.to_string()));
        safe_kv_set(db, "relay_last_sync", Some(&now.to_string()));
        set_relay_status(db, "ok", None, is_worker);
    }
    // When disconnected: publish is best-effort (rumqttc may buffer), but
    // cursor stays put so events are re-sent after reconnect.

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

        let (_state, events, max_id, has_more) = build_push_payload(&db, "device-a");

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
        )
        .unwrap_err();

        assert!(error.contains("exceeds safe MQTT payload budget"));
    }
}
