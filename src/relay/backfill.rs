//! Catch-up backfill for relay event gaps.
//!
//! A peer's retained snapshot carries a contiguous run of its own-origin
//! events: the retained tail below its last push plus whatever is new. A device
//! that was offline, or missed non-retained publishes, while the peer pushed
//! past that tail sees the next snapshot start above its import cursor. The
//! cursor then jumps to the newest carried id and every event in between is
//! lost without a trace: messages that were never delivered and never reported
//! as missing.
//!
//! Import records such a range as a gap. The relay worker loop asks the peer
//! for its own-origin events in the range through the existing `events` RPC and
//! imports the answer through the same namespacing path as live events,
//! deduplicated by (device, remote id). The request is sent from the worker
//! loop and the answer is picked up there too: the answer rides back inside the
//! peer's next state snapshot, which the inbound MQTT handler imports, so the
//! handler itself must never wait for it.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::HcomDb;
use crate::log;

use super::{safe_kv_get, safe_kv_set};

pub(crate) const GAP_KEY_PREFIX: &str = "relay_gap_";
/// Events asked for per request. The answer travels back inside the peer's
/// state snapshot, so it has to leave room for the snapshot itself.
pub(crate) const BACKFILL_BATCH: usize = 100;
/// Byte budget asked of the peer for one answer. Peers that predate the
/// `max_bytes` parameter apply their own 96 KiB cap.
pub(crate) const BACKFILL_MAX_BYTES: usize = 32 * 1024;
/// Budget for the one-event request used when a single event is larger than
/// `BACKFILL_MAX_BYTES`.
pub(crate) const BACKFILL_WIDE_MAX_BYTES: usize = 96 * 1024;
/// Floor for the normal byte budget once answers have come back cut to fit the peer's publish.
pub(crate) const BACKFILL_MIN_BYTES: usize = 4 * 1024;
/// Seconds to wait for an answer before asking again.
pub(crate) const BACKFILL_RETRY_SECS: f64 = 30.0;
/// Requests without progress before a gap is given up (and logged).
pub(crate) const BACKFILL_MAX_ATTEMPTS: u32 = 6;
/// Gaps kept per peer. Beyond this the oldest is dropped and logged.
const MAX_GAPS_PER_DEVICE: usize = 16;

/// One missing range of a peer's event ids, both bounds exclusive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Gap {
    /// Our import cursor for the peer when the gap was seen.
    pub after: i64,
    /// The oldest id the peer's snapshot carried; narrows as batches arrive.
    pub before: i64,
    pub short_id: String,
    pub detected_at: f64,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub sent_at: f64,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub recovered: u64,
    /// Next request asks for a single event with the wide byte budget.
    #[serde(default)]
    pub wide: bool,
    /// Answers that came back cut to fit the peer's publish. Each one halves the next normal
    /// request's byte budget, down to `BACKFILL_MIN_BYTES`, so a large peer state leaves room
    /// for a smaller answer instead of every retry failing the same way until the gap is
    /// abandoned.
    #[serde(default)]
    pub shrink: u32,
}

fn gap_key(device_id: &str) -> String {
    format!("{GAP_KEY_PREFIX}{device_id}")
}

pub(crate) fn load_gaps(db: &HcomDb, device_id: &str) -> Vec<Gap> {
    safe_kv_get(db, &gap_key(device_id))
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_gaps(db: &HcomDb, device_id: &str, gaps: &[Gap]) {
    if gaps.is_empty() {
        safe_kv_set(db, &gap_key(device_id), None);
    } else if let Ok(raw) = serde_json::to_string(gaps) {
        safe_kv_set(db, &gap_key(device_id), Some(&raw));
    }
}

/// Forget every gap for a peer (its event ids restarted).
pub(crate) fn clear_gaps(db: &HcomDb, device_id: &str) {
    safe_kv_set(db, &gap_key(device_id), None);
}

/// Record that the peer's events in (`after`, `before`) were skipped.
pub(crate) fn record_gap(db: &HcomDb, device_id: &str, short_id: &str, after: i64, before: i64) {
    if before <= after + 1 {
        return;
    }
    let mut gaps = load_gaps(db, device_id);
    // A snapshot whose carried events were all filtered out leaves the cursor
    // where it was, so the next snapshot reports the same range again.
    if gaps.iter().any(|g| g.after <= after && before <= g.before) {
        return;
    }
    gaps.push(Gap {
        after,
        before,
        short_id: short_id.to_string(),
        detected_at: crate::shared::time::now_epoch_f64(),
        request_id: None,
        sent_at: 0.0,
        attempts: 0,
        recovered: 0,
        wide: false,
        shrink: 0,
    });
    while gaps.len() > MAX_GAPS_PER_DEVICE {
        let dropped = gaps.remove(0);
        log::log_warn(
            "relay",
            "relay.backfill_dropped",
            &format!(
                "device={} range=({},{}) reason=too_many_gaps",
                dropped.short_id, dropped.after, dropped.before
            ),
        );
    }
    save_gaps(db, device_id, &gaps);
    log::log_with_fields(
        "INFO",
        "relay",
        "relay.gap_detected",
        "",
        &[
            ("device", short_id),
            ("after", &after.to_string()),
            ("before", &before.to_string()),
        ],
    );
}

/// Parameters for the peer's `events` RPC covering the gap. The filter is the
/// push loop's own-origin filter, so the answer is exactly what the missed
/// snapshots would have carried.
/// The normal byte budget after `shrink` cut answers: halved each time, never below the floor.
fn shrunk_budget(shrink: u32) -> usize {
    BACKFILL_MAX_BYTES
        .checked_shr(shrink)
        .unwrap_or(0)
        .max(BACKFILL_MIN_BYTES)
}

pub(crate) fn request_params(gap: &Gap) -> Value {
    let (last, max_bytes) = if gap.wide {
        (1, BACKFILL_WIDE_MAX_BYTES)
    } else {
        (BACKFILL_BATCH, shrunk_budget(gap.shrink))
    };
    json!({
        "sql": format!(
            "id > {} AND id < {} AND instance NOT LIKE '%:%' AND instance != '_device' \
             AND type NOT IN ('control', 'rpc_result') AND json_extract(data, '$._relay') IS NULL",
            gap.after, gap.before
        ),
        "last": last,
        "max_bytes": max_bytes,
    })
}

fn already_imported(db: &HcomDb, device_id: &str, remote_id: i64, ts: &str) -> bool {
    db.conn()
        .query_row(
            "SELECT 1 FROM events WHERE timestamp = ?1
               AND json_extract(data, '$._relay.device') = ?2
               AND json_extract(data, '$._relay.id') = ?3
             LIMIT 1",
            rusqlite::params![ts, device_id, remote_id],
            |_| Ok(()),
        )
        .is_ok()
}

fn local_reset_ts(db: &HcomDb) -> f64 {
    safe_kv_get(db, "relay_local_reset_ts")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0)
}

#[derive(Debug, PartialEq)]
pub(crate) enum AnswerOutcome {
    /// The whole range has been answered.
    Done { imported: u64 },
    /// More events remain below the oldest one returned; the gap narrowed.
    More { imported: u64 },
}

/// Import one `events` RPC answer into the gap it was asked for.
pub(crate) fn apply_answer(
    db: &HcomDb,
    device_id: &str,
    own_short_id: &str,
    gap: &mut Gap,
    response: &Value,
) -> Result<AnswerOutcome, String> {
    // An answer the publishing peer had to shrink to fit its sealed payload (answer + its
    // state) has had strings inside `result.events` cut while `ok` stayed true. Importing it
    // would store partial events as if whole, and the gap would close. Refuse it,
    // and ask for less next time: the peer's state does not shrink between retries, so the
    // same budget would be cut the same way every time.
    if response.get("_relay_truncated").is_some() {
        gap.shrink = gap.shrink.saturating_add(1);
        return Err(format!(
            "the answer was cut to fit the peer's publish; not importing partial events \
             (next request asks for {} bytes)",
            shrunk_budget(gap.shrink)
        ));
    }
    let result = response.get("result").cloned().unwrap_or(Value::Null);
    if !response
        .get("ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let detail = result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown remote error");
        return Err(format!("peer refused: {detail}"));
    }
    let Some(events) = result.get("events").and_then(|v| v.as_array()) else {
        return Err("answer carried no events list".to_string());
    };
    let truncated = result
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let asked_for = if gap.wide { 1 } else { BACKFILL_BATCH };

    if events.is_empty() && truncated {
        if gap.wide {
            return Err("a single event exceeds the widest answer budget".to_string());
        }
        // The newest event in the range alone is larger than the normal budget.
        gap.wide = true;
        return Ok(AnswerOutcome::More { imported: 0 });
    }

    let reset_ts = local_reset_ts(db);
    let mut imported = 0u64;
    let mut oldest = gap.before;
    // One write scope per answer, like snapshot import: one sync for the batch.
    let mut rows = Vec::new();
    db.with_write_scope(|| {
        for event in events {
            let Some(remote_id) = event.get("id").and_then(|v| v.as_i64()) else {
                continue;
            };
            if remote_id <= gap.after || remote_id >= gap.before {
                continue;
            }
            oldest = oldest.min(remote_id);
            if event.get("type").and_then(|v| v.as_str()) == Some("control")
                || event.get("instance").and_then(|v| v.as_str()) == Some("_device")
            {
                continue;
            }
            let event_ts = super::pull::event_epoch(event);
            if reset_ts > 0.0 && event_ts > 0.0 && event_ts < reset_ts {
                continue;
            }
            let ts = super::pull::event_ts_string(event);
            if already_imported(db, device_id, remote_id, &ts) {
                continue;
            }
            rows.push(super::pull::insert_remote_event(
                db,
                device_id,
                &gap.short_id,
                remote_id,
                event,
                own_short_id,
            )?);
            imported += 1;
        }
        Ok(())
    })
    .map_err(|e| format!("importing the answer failed: {e}"))?;
    super::pull::finish_imported_events(db, rows);
    gap.recovered += imported;
    gap.wide = false;

    let more_below = truncated || events.len() >= asked_for;
    if more_below && oldest > gap.after + 1 && oldest < gap.before {
        gap.before = oldest;
        return Ok(AnswerOutcome::More { imported });
    }
    Ok(AnswerOutcome::Done { imported })
}

/// What one tick did, for the caller's logging and tests.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct TickSummary {
    pub requests_sent: u32,
    pub events_imported: u64,
    pub gaps_closed: u32,
    pub gaps_abandoned: u32,
}

/// Peers with at least one open gap.
pub(crate) fn devices_with_gaps(db: &HcomDb) -> Vec<String> {
    let pattern = format!("{GAP_KEY_PREFIX}%");
    db.conn()
        .prepare("SELECT key FROM kv WHERE key LIKE ?1 AND value IS NOT NULL")
        .ok()
        .map(|mut stmt| {
            stmt.query_map(rusqlite::params![pattern], |row| row.get::<_, String>(0))
                .ok()
                .map(|rows| {
                    rows.filter_map(|r| r.ok())
                        .filter_map(|key| key.strip_prefix(GAP_KEY_PREFIX).map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

/// Advance every open gap by one step: collect an answer if one arrived,
/// otherwise (re)send the request when the peer can take it.
///
/// `send(short_id, request_id, params)` publishes an `events` RPC request and
/// reports whether MQTT accepted it. Runs on the relay worker loop.
pub(crate) fn tick(
    db: &HcomDb,
    own_short_id: &str,
    now: f64,
    send: &mut dyn FnMut(&str, &str, &Value) -> bool,
) -> TickSummary {
    let mut summary = TickSummary::default();
    for device_id in devices_with_gaps(db) {
        let mut gaps = load_gaps(db, &device_id);
        if gaps.is_empty() {
            clear_gaps(db, &device_id);
            continue;
        }
        let mut changed = false;

        if let Some(request_id) = gaps[0].request_id.clone() {
            if let Some(response) = super::control::take_rpc_result(db, &request_id) {
                changed = true;
                let gap = &mut gaps[0];
                gap.request_id = None;
                match apply_answer(db, &device_id, own_short_id, gap, &response) {
                    Ok(AnswerOutcome::Done { imported }) => {
                        summary.events_imported += imported;
                        summary.gaps_closed += 1;
                        log::log_with_fields(
                            "INFO",
                            "relay",
                            "relay.backfill_done",
                            "",
                            &[
                                ("device", &gap.short_id),
                                ("after", &gap.after.to_string()),
                                ("before", &gap.before.to_string()),
                                ("recovered", &gap.recovered.to_string()),
                            ],
                        );
                        gaps.remove(0);
                    }
                    Ok(AnswerOutcome::More { imported }) => {
                        summary.events_imported += imported;
                        gap.attempts = 0;
                    }
                    Err(error) => {
                        log::log_warn(
                            "relay",
                            "relay.backfill_answer_err",
                            &format!(
                                "device={} range=({},{}) error={}",
                                gap.short_id, gap.after, gap.before, error
                            ),
                        );
                    }
                }
            } else if now - gaps[0].sent_at >= BACKFILL_RETRY_SECS {
                changed = true;
                gaps[0].request_id = None;
            }
        }

        if gaps.first().is_some_and(|g| g.request_id.is_none()) {
            let gap = &mut gaps[0];
            let accepts = if gap.attempts >= BACKFILL_MAX_ATTEMPTS {
                Some(false)
            } else {
                super::control::peer_accepts_action(
                    db,
                    &gap.short_id,
                    super::control::rpc_action::EVENTS,
                )
            };
            match accepts {
                Some(true) => {
                    let request_id = uuid::Uuid::new_v4().to_string();
                    if send(&gap.short_id, &request_id, &request_params(gap)) {
                        gap.request_id = Some(request_id);
                        gap.sent_at = now;
                        gap.attempts += 1;
                        summary.requests_sent += 1;
                        changed = true;
                    }
                }
                Some(false) => {
                    log::log_warn(
                        "relay",
                        "relay.backfill_abandoned",
                        &format!(
                            "device={} range=({},{}) attempts={} recovered={}",
                            gap.short_id, gap.after, gap.before, gap.attempts, gap.recovered
                        ),
                    );
                    summary.gaps_abandoned += 1;
                    gaps.remove(0);
                    changed = true;
                }
                // Offline or not yet synced: keep the gap and try later.
                None => {}
            }
        }

        if changed {
            save_gaps(db, &device_id, &gaps);
        }
    }
    if summary.events_imported > 0 {
        crate::notify::wake_all(db);
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_helpers::isolated_test_env;
    use serial_test::serial;

    const PEER: &str = "peer-device-uuid";

    fn synced_peer(db: &HcomDb, short: &str, caps: &str) {
        safe_kv_set(db, &format!("relay_short_{short}"), Some(PEER));
        let now = crate::shared::time::now_epoch_f64();
        safe_kv_set(
            db,
            &format!("relay_sync_time_{PEER}"),
            Some(&now.to_string()),
        );
        safe_kv_set(db, &format!("relay_caps_{PEER}"), Some(caps));
    }

    fn message(id: i64, text: &str) -> Value {
        json!({
            "id": id,
            "ts": format!("2026-09-26T10:00:{:02}.000000+00:00", id % 60),
            "type": "message",
            "instance": "luna",
            "data": {"from": "luna", "text": text, "mentions": ["nova:MINE"], "scope": "mentions"},
        })
    }

    fn answer(request_id: &str, events: Vec<Value>, truncated: bool) -> Value {
        let mut result = json!({"events": events, "count": events.len()});
        if truncated {
            result["truncated"] = json!(true);
        }
        json!({"request_id": request_id, "action": "events", "ok": true, "result": result,
               "_relay": {"device": PEER, "short": "ABCD", "id": 999}})
    }

    fn imported_texts(db: &HcomDb) -> Vec<String> {
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT json_extract(data, '$.text') FROM events
                 WHERE type = 'message' AND json_extract(data, '$._relay.device') = ?1
                 ORDER BY json_extract(data, '$._relay.id')",
            )
            .unwrap();
        stmt.query_map(rusqlite::params![PEER], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    #[test]
    #[serial]
    fn record_gap_ignores_adjacent_and_repeated_ranges() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();

        record_gap(&db, PEER, "ABCD", 10, 11);
        assert!(
            load_gaps(&db, PEER).is_empty(),
            "no id lies strictly between"
        );

        record_gap(&db, PEER, "ABCD", 10, 40);
        record_gap(&db, PEER, "ABCD", 10, 40);
        record_gap(&db, PEER, "ABCD", 12, 30);
        let gaps = load_gaps(&db, PEER);
        assert_eq!(gaps.len(), 1);
        assert_eq!((gaps[0].after, gaps[0].before), (10, 40));
    }

    #[test]
    #[serial]
    fn tick_requests_the_range_then_imports_the_answer_and_closes_the_gap() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        synced_peer(&db, "ABCD", r#"["events"]"#);
        record_gap(&db, PEER, "ABCD", 10, 20);

        let mut sent: Vec<(String, String, Value)> = Vec::new();
        let summary = tick(&db, "MINE", 1000.0, &mut |short, req, params| {
            sent.push((short.to_string(), req.to_string(), params.clone()));
            true
        });
        assert_eq!(summary.requests_sent, 1);
        assert_eq!(sent.len(), 1);
        let (short, request_id, params) = sent.remove(0);
        assert_eq!(short, "ABCD");
        let sql = params["sql"].as_str().unwrap();
        assert!(sql.contains("id > 10 AND id < 20"), "{sql}");
        assert!(
            sql.contains("json_extract(data, '$._relay') IS NULL"),
            "{sql}"
        );

        // No answer yet and not timed out: nothing is re-sent.
        let summary = tick(&db, "MINE", 1005.0, &mut |_, _, _| {
            panic!("must not resend")
        });
        assert_eq!(summary, TickSummary::default());

        // The answer arrives inside the peer's snapshot as an imported rpc_result.
        db.log_event(
            "rpc_result",
            "_rpc",
            &answer(
                &request_id,
                vec![message(15, "second"), message(12, "first")],
                false,
            ),
        )
        .unwrap();
        let summary = tick(&db, "MINE", 1006.0, &mut |_, _, _| {
            panic!("must not resend")
        });
        assert_eq!(summary.events_imported, 2);
        assert_eq!(summary.gaps_closed, 1);
        assert!(load_gaps(&db, PEER).is_empty());
        assert_eq!(imported_texts(&db), vec!["first", "second"]);

        // Imported exactly like a live event: namespaced sender, own suffix
        // stripped from mentions so local delivery matches.
        let (from, mentions): (String, String) = db
            .conn()
            .query_row(
                "SELECT json_extract(data, '$.from'), json_extract(data, '$.mentions')
                 FROM events WHERE json_extract(data, '$._relay.id') = 12",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(from, "luna:ABCD");
        assert_eq!(mentions, r#"["nova"]"#);
        // The rendezvous row was consumed.
        assert!(super::super::control::get_rpc_result(&db, &request_id).is_none());
    }

    #[test]
    #[serial]
    fn a_full_batch_narrows_the_gap_and_asks_for_the_rest() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        synced_peer(&db, "ABCD", r#"["events"]"#);
        record_gap(&db, PEER, "ABCD", 1, 500);

        let mut request_id = String::new();
        tick(&db, "MINE", 1000.0, &mut |_, req, _| {
            request_id = req.to_string();
            true
        });
        // Newest-first page of exactly BACKFILL_BATCH events: 400..=499.
        let page: Vec<Value> = (400..500)
            .rev()
            .map(|id| message(id, &format!("m{id}")))
            .collect();
        db.log_event("rpc_result", "_rpc", &answer(&request_id, page, false))
            .unwrap();

        let mut second: Option<Value> = None;
        let summary = tick(&db, "MINE", 1001.0, &mut |_, _, params| {
            second = Some(params.clone());
            true
        });
        assert_eq!(summary.events_imported, 100);
        assert_eq!(summary.gaps_closed, 0);
        let gaps = load_gaps(&db, PEER);
        assert_eq!((gaps[0].after, gaps[0].before), (1, 400));
        let sql = second.expect("follow-up request")["sql"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(sql.contains("id > 1 AND id < 400"), "{sql}");
    }

    #[test]
    #[serial]
    fn a_repeated_answer_does_not_duplicate_events() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        let mut gap = Gap {
            after: 10,
            before: 20,
            short_id: "ABCD".into(),
            detected_at: 0.0,
            request_id: None,
            sent_at: 0.0,
            attempts: 1,
            recovered: 0,
            wide: false,
            shrink: 0,
        };
        let response = answer("r1", vec![message(15, "only")], false);
        assert_eq!(
            apply_answer(&db, PEER, "MINE", &mut gap.clone(), &response),
            Ok(AnswerOutcome::Done { imported: 1 })
        );
        assert_eq!(
            apply_answer(&db, PEER, "MINE", &mut gap, &response),
            Ok(AnswerOutcome::Done { imported: 0 })
        );
        assert_eq!(imported_texts(&db), vec!["only"]);
    }

    #[test]
    #[serial]
    fn an_answer_cut_in_transit_is_refused_not_imported() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        let mut gap = Gap {
            after: 10,
            before: 20,
            short_id: "ABCD".into(),
            detected_at: 0.0,
            request_id: None,
            sent_at: 0.0,
            attempts: 1,
            recovered: 0,
            wide: true,
            shrink: 0,
        };
        // What push's shrink leaves when answer + state overflow the sealed payload:
        // ok stays true, strings inside result.events are cut, and the marker is added.
        let mut response = answer("r2", vec![message(15, "cut short")], false);
        response["_relay_truncated"] = json!({"level": 1, "original_bytes": 120_000});
        let outcome = apply_answer(&db, PEER, "MINE", &mut gap, &response);
        assert!(outcome.is_err(), "{outcome:?}");
        assert!(imported_texts(&db).is_empty(), "no partial event stored");
        assert_eq!(gap.recovered, 0);
    }

    #[test]
    #[serial]
    fn a_cut_answer_halves_the_next_budget_down_to_a_floor() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        let mut gap = Gap {
            after: 10,
            before: 20,
            short_id: "ABCD".into(),
            detected_at: 0.0,
            request_id: None,
            sent_at: 0.0,
            attempts: 1,
            recovered: 0,
            wide: false,
            shrink: 0,
        };
        assert_eq!(request_params(&gap)["max_bytes"], json!(BACKFILL_MAX_BYTES));
        let mut response = answer("r3", vec![message(15, "cut short")], false);
        response["_relay_truncated"] = json!({"level": 1, "original_bytes": 60_000});
        let mut budgets = Vec::new();
        for _ in 0..BACKFILL_MAX_ATTEMPTS {
            assert!(apply_answer(&db, PEER, "MINE", &mut gap, &response).is_err());
            budgets.push(request_params(&gap)["max_bytes"].as_u64().unwrap());
        }
        // A large peer state leaves less room each time; the budget follows it down instead
        // of repeating the answer that was just cut, and never asks for less than the floor.
        assert_eq!(budgets, vec![16_384, 8_192, 4_096, 4_096, 4_096, 4_096]);
        assert!(
            imported_texts(&db).is_empty(),
            "no partial event stored on the way down"
        );
        // A gap persisted before this field existed reads back with no shrink.
        let old: Gap = serde_json::from_value(json!({
            "after": 1, "before": 5, "short_id": "ABCD", "detected_at": 0.0
        }))
        .unwrap();
        assert_eq!(old.shrink, 0);
        assert_eq!(request_params(&old)["max_bytes"], json!(BACKFILL_MAX_BYTES));
    }

    #[test]
    #[serial]
    fn an_empty_answer_closes_the_gap_without_importing() {
        // Ids between two own-origin events are often the peer's imported
        // events, which it never relays; the answer is then empty.
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        synced_peer(&db, "ABCD", r#"["events"]"#);
        record_gap(&db, PEER, "ABCD", 10, 20);
        let mut request_id = String::new();
        tick(&db, "MINE", 1000.0, &mut |_, req, _| {
            request_id = req.to_string();
            true
        });
        db.log_event("rpc_result", "_rpc", &answer(&request_id, vec![], false))
            .unwrap();
        let summary = tick(&db, "MINE", 1001.0, &mut |_, _, _| {
            panic!("must not resend")
        });
        assert_eq!(summary.gaps_closed, 1);
        assert_eq!(summary.events_imported, 0);
        assert!(load_gaps(&db, PEER).is_empty());
    }

    #[test]
    #[serial]
    fn an_oversized_newest_event_is_fetched_alone_with_the_wide_budget() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        synced_peer(&db, "ABCD", r#"["events"]"#);
        record_gap(&db, PEER, "ABCD", 10, 20);
        let mut request_id = String::new();
        tick(&db, "MINE", 1000.0, &mut |_, req, _| {
            request_id = req.to_string();
            true
        });
        db.log_event("rpc_result", "_rpc", &answer(&request_id, vec![], true))
            .unwrap();
        let mut next: Option<Value> = None;
        tick(&db, "MINE", 1001.0, &mut |_, _, params| {
            next = Some(params.clone());
            true
        });
        let next = next.expect("wide request");
        assert_eq!(next["last"], json!(1));
        assert_eq!(next["max_bytes"], json!(BACKFILL_WIDE_MAX_BYTES));
    }

    #[test]
    #[serial]
    fn an_unanswered_request_is_resent_then_abandoned_and_logged() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        synced_peer(&db, "ABCD", r#"["events"]"#);
        record_gap(&db, PEER, "ABCD", 10, 20);

        let mut sends = 0;
        let mut now = 1000.0;
        for _ in 0..(BACKFILL_MAX_ATTEMPTS + 2) {
            // Keep the peer fresh so only the attempt budget ends the gap.
            let fresh = crate::shared::time::now_epoch_f64();
            safe_kv_set(
                &db,
                &format!("relay_sync_time_{PEER}"),
                Some(&fresh.to_string()),
            );
            tick(&db, "MINE", now, &mut |_, _, _| {
                sends += 1;
                true
            });
            now += BACKFILL_RETRY_SECS;
        }
        assert_eq!(sends, BACKFILL_MAX_ATTEMPTS as usize);
        assert!(
            load_gaps(&db, PEER).is_empty(),
            "abandoned after the attempt budget"
        );
    }

    #[test]
    #[serial]
    fn an_offline_peer_keeps_its_gap_without_spending_attempts() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        synced_peer(&db, "ABCD", r#"["events"]"#);
        safe_kv_set(&db, &format!("relay_sync_time_{PEER}"), Some("1.0"));
        record_gap(&db, PEER, "ABCD", 10, 20);

        let summary = tick(&db, "MINE", 1000.0, &mut |_, _, _| {
            panic!("peer is offline")
        });
        assert_eq!(summary, TickSummary::default());
        let gaps = load_gaps(&db, PEER);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].attempts, 0);
    }

    #[test]
    #[serial]
    fn a_peer_without_the_events_action_is_abandoned_at_once() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        synced_peer(&db, "ABCD", r#"["launch"]"#);
        record_gap(&db, PEER, "ABCD", 10, 20);

        let summary = tick(&db, "MINE", 1000.0, &mut |_, _, _| panic!("unsupported"));
        assert_eq!(summary.gaps_abandoned, 1);
        assert!(load_gaps(&db, PEER).is_empty());
    }
}
