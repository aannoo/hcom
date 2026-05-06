//! Event subscription storage, creation, firing, and delivery.
//!
//! `events_sub:` rows in `kv` are the canonical subscription/event-model
//! contract. The key prefix is `events_sub:` followed by a stable subscription
//! ID. Recognized JSON fields are:
//! - `id`: stable subscription ID, also encoded in the key.
//! - `caller`: agent or external sender that owns the subscription.
//! - `sql`: SQL predicate evaluated against `events_v`.
//! - `params`: optional SQL parameters for parameterized internal subs.
//! - `filters`: original structured filters and internal metadata.
//! - `thread_name`: thread name for delivery-only membership rows.
//! - `on_hit_text`: optional message to send when the subscription fires.
//! - `caller_kind`: frozen sender kind for `on_hit_text` provenance.
//! - `last_id`: cursor for the last event processed by this subscription.
//! - `created`: creation timestamp used for ordering/listing.
//! - `once`: remove the subscription after its first match.
//! - `delivery_only`: internal row used for routing state, not notification.
//! - `auto_thread_member`: delivery-only thread membership marker.
//!
//! Subscription kinds stored under this prefix are filter subs, SQL subs,
//! request watches (`reqwatch-*`), delivery-only thread members, and collision
//! subs. New code should not write `events_sub:` rows outside this module.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::params;
use serde_json::json;

use super::HcomDb;
use crate::core::filters::{FILE_WRITE_CONTEXTS, build_sql_from_flags};
use crate::shared::constants::MENTION_PATTERN;

fn subscription_is_delivery_only(sub: &serde_json::Value) -> bool {
    match sub.get("delivery_only") {
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(serde_json::Value::Number(n)) => n.as_i64() == Some(1),
        Some(serde_json::Value::String(s)) => s.eq_ignore_ascii_case("true") || s == "1",
        _ => false,
    }
}

/// Stable subscription ID for automatic thread membership rows.
pub(crate) fn thread_membership_sub_id(thread: &str, member: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(format!("thread-member:{thread}:{member}").as_bytes());
    let hash = hasher.finalize();
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    format!("sub-{}", &hex[..8])
}

/// Outcome of a subscription insert attempt.
pub(crate) enum SubCreateOutcome {
    Created { id: String, final_sql: String },
    AlreadyExists { id: String },
}

/// Build and insert a filter-based subscription row into `kv`.
pub(crate) fn create_filter_subscription(
    db: &HcomDb,
    filters: &HashMap<String, Vec<String>>,
    sql_parts: &[String],
    caller: &str,
    once: bool,
    on_hit: Option<&str>,
) -> Result<SubCreateOutcome, String> {
    let mut sql = match build_sql_from_flags(filters) {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => return Err("No valid filters provided".to_string()),
        Err(e) => return Err(format!("Filter error: {e}")),
    };

    if !sql_parts.is_empty() {
        let manual_sql = sql_parts.join(" ").replace("\\!", "!");
        if let Err(e) = db.conn().execute(
            &format!("SELECT 1 FROM events_v WHERE ({manual_sql}) LIMIT 0"),
            [],
        ) {
            return Err(format!("Invalid SQL: {e}"));
        }
        sql = format!("({sql}) AND ({manual_sql})");
    }

    if filters.contains_key("collision") {
        let self_relevance = collision_self_relevance_sql(caller);
        sql = format!("({sql}) AND {self_relevance}");
    }

    let id_source = format!(
        "{}:{}:{}:{}:{}",
        caller,
        serde_json::to_string(filters).unwrap_or_default(),
        sql,
        once,
        on_hit.unwrap_or(""),
    );
    let hash = sha256_hash(&id_source);
    let sub_id = format!("sub-{}", &hash[..8]);
    let sub_key = format!("events_sub:{sub_id}");

    if db.kv_get(&sub_key).ok().flatten().is_some() {
        return Ok(SubCreateOutcome::AlreadyExists { id: sub_id });
    }

    let now = crate::shared::time::now_epoch_f64();
    let last_id = db.get_last_event_id();

    let mut sub_data = json!({
        "id": sub_id,
        "caller": caller,
        "filters": filters,
        "sql": sql,
        "created": now,
        "last_id": last_id,
        "once": once,
    });
    if let Some(text) = on_hit {
        sub_data["on_hit_text"] = json!(text);
        sub_data["caller_kind"] = json!(resolve_caller_kind(db, caller));
    }

    let _ = db.kv_set(&sub_key, Some(&sub_data.to_string()));

    Ok(SubCreateOutcome::Created {
        id: sub_id,
        final_sql: sql,
    })
}

/// Build and insert a raw-SQL subscription row into `kv`.
pub(crate) fn build_and_insert_sql_subscription(
    db: &HcomDb,
    sql_parts: &[String],
    caller: &str,
    once: bool,
    on_hit: Option<&str>,
) -> Result<SubCreateOutcome, String> {
    let sql = sql_parts.join(" ").replace("\\!", "!");

    if let Err(e) = db
        .conn()
        .execute(&format!("SELECT 1 FROM events_v WHERE ({sql}) LIMIT 0"), [])
    {
        return Err(format!("Invalid SQL: {e}"));
    }

    let hash = sha256_hash(&format!("{caller}{sql}{once}{}", on_hit.unwrap_or("")));
    let sub_id = format!("sub-{}", &hash[..8]);
    let sub_key = format!("events_sub:{sub_id}");

    if db.kv_get(&sub_key).ok().flatten().is_some() {
        return Ok(SubCreateOutcome::AlreadyExists { id: sub_id });
    }

    let now = crate::shared::time::now_epoch_f64();
    let last_id = db.get_last_event_id();

    let mut sub_data = json!({
        "id": sub_id,
        "sql": sql,
        "caller": caller,
        "once": once,
        "last_id": last_id,
        "created": now,
    });
    if let Some(text) = on_hit {
        sub_data["on_hit_text"] = json!(text);
        sub_data["caller_kind"] = json!(resolve_caller_kind(db, caller));
    }

    let _ = db.kv_set(&sub_key, Some(&sub_data.to_string()));

    Ok(SubCreateOutcome::Created {
        id: sub_id,
        final_sql: sql,
    })
}

/// Create request-watch subscriptions for each recipient.
pub(crate) fn create_request_watches(
    db: &HcomDb,
    sender: &str,
    request_event_id: i64,
    recipients: &[String],
) {
    let last_id = db.get_last_event_id();
    let now = crate::shared::time::now_epoch_f64();

    for recipient in recipients {
        let sub_id = format!("reqwatch-{request_event_id}-{recipient}");
        let sub_key = format!("events_sub:{sub_id}");

        let sql = "(type='status' AND instance=? AND status_val='listening') OR (type='life' AND instance=? AND life_action='stopped')";

        let sub_data = serde_json::json!({
            "id": sub_id,
            "caller": sender,
            "sql": sql,
            "params": [recipient, recipient],
            "filters": {
                "request_watch": true,
                "request_id": request_event_id,
                "target": recipient,
            },
            "once": true,
            "last_id": last_id,
            "created": now,
        });

        let _ = db.kv_set(&sub_key, Some(&sub_data.to_string()));
    }
}

/// Remove all event subscriptions owned by an instance.
pub(crate) fn cleanup_subscriptions(db: &HcomDb, name: &str) -> Result<u32> {
    let deleted = db.conn.execute(
        "DELETE FROM kv
         WHERE key LIKE 'events_sub:%'
           AND json_extract(value, '$.caller') = ?
           AND COALESCE(json_extract(value, '$.delivery_only'), 0) != 1",
        params![name],
    )?;
    Ok(deleted as u32)
}

/// Remove delivery-only thread memberships for an instance name reuse.
pub(crate) fn cleanup_thread_memberships_for_name_reuse(db: &HcomDb, name: &str) -> Result<u32> {
    let deleted = db.conn.execute(
        "DELETE FROM kv
         WHERE key LIKE 'events_sub:%'
           AND json_extract(value, '$.caller') = ?
           AND json_extract(value, '$.auto_thread_member') = 1
           AND COALESCE(json_extract(value, '$.delivery_only'), 0) = 1",
        params![name],
    )?;
    Ok(deleted as u32)
}

/// Return active members of a thread in join order.
pub(crate) fn get_thread_members(db: &HcomDb, thread: &str) -> Vec<String> {
    let active_instances: HashSet<String> = db
        .conn()
        .prepare("SELECT name FROM instances")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|r| r.ok())
                .collect()
        })
        .unwrap_or_default();

    let rows: Vec<String> = db
        .conn()
        .prepare(
            "SELECT value FROM kv
             WHERE key LIKE 'events_sub:%'
               AND json_extract(value, '$.auto_thread_member') = 1
               AND json_extract(value, '$.thread_name') = ?
             ORDER BY json_extract(value, '$.created') ASC, key ASC",
        )
        .ok()
        .map(|mut stmt| {
            stmt.query_map(rusqlite::params![thread], |row| row.get::<_, String>(0))
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|r| r.ok())
                .collect()
        })
        .unwrap_or_default();

    let mut members = Vec::new();
    let mut seen = HashSet::new();
    for value in rows {
        let caller = serde_json::from_str::<serde_json::Value>(&value)
            .ok()
            .and_then(|sub| sub.get("caller").and_then(|v| v.as_str()).map(String::from));
        if let Some(caller) = caller {
            if active_instances.contains(&caller) && seen.insert(caller.clone()) {
                members.push(caller);
            }
        }
    }
    members
}

/// Upsert memberships for recipients of a thread message.
pub(crate) fn add_thread_memberships(
    db: &HcomDb,
    thread: &str,
    sender: Option<&str>,
    recipients: &[String],
) {
    let mut members = recipients.to_vec();
    if let Some(sender) = sender {
        members.push(sender.to_string());
    }

    let now = crate::shared::time::now_epoch_f64();
    let last_id = db.get_last_event_id();
    let mut seen = HashSet::new();
    for (idx, member) in members.into_iter().enumerate() {
        if !seen.insert(member.clone()) {
            continue;
        }
        let sub_id = thread_membership_sub_id(thread, &member);
        let key = format!("events_sub:{sub_id}");
        let data = serde_json::json!({
            "id": sub_id,
            "caller": member,
            "thread_name": thread,
            "auto_thread_member": true,
            "delivery_only": true,
            "sql": "0",
            "created": now + (idx as f64 * 0.000001),
            "last_id": last_id,
            "once": false,
        });
        let _ = db.kv_set(&key, Some(&data.to_string()));
    }
}

/// Check subscriptions and send matching notifications.
/// Called inline from log_event(). Errors logged, never propagated.
pub(crate) fn process_logged_event(
    db: &HcomDb,
    event_id: i64,
    event_type: &str,
    instance: &str,
    data: &serde_json::Value,
) {
    // Recursion guard: skip events that could cause notification loops.
    if instance.starts_with("sys_") {
        return;
    }
    if event_type == "message" {
        let sender = data.get("from").and_then(|v| v.as_str()).unwrap_or("");
        let sender_kind = data
            .get("sender_kind")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if sender == "[hcom-events]" || sender_kind == "system" {
            return;
        }
    }

    if event_type == "message" {
        let msg_sender = data.get("from").and_then(|v| v.as_str()).unwrap_or("");
        let reply_to_id = data.get("reply_to_local").and_then(|v| v.as_i64());

        if let Some("mentions") = data.get("scope").and_then(|v| v.as_str()) {
            let msg_delivered_to: Vec<String> = data
                .get("delivered_to")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if !msg_sender.is_empty() && !msg_delivered_to.is_empty() {
                cancel_request_watches_by_flow(db, msg_sender, &msg_delivered_to, reply_to_id);
            }
        }

        if let Some(rid) = reply_to_id {
            if !msg_sender.is_empty() {
                cancel_request_watches_by_reply_id(db, msg_sender, rid);
            }
        }
    }

    let rows: Vec<(String, String)> = match db.conn.prepare_cached(
        "SELECT key, value FROM kv
         WHERE key LIKE 'events_sub:%'
           AND COALESCE(json_extract(value, '$.delivery_only'), 0) != 1
           AND COALESCE(json_extract(value, '$.delivery_only'), 'false') != 'true'",
    ) {
        Ok(mut stmt) => stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .ok()
            .map(|iter| iter.filter_map(|r| r.ok()).collect())
            .unwrap_or_default(),
        Err(_) => return,
    };

    if rows.is_empty() {
        return;
    }

    for (key, value) in &rows {
        let sub: serde_json::Value = match serde_json::from_str(value) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if subscription_is_delivery_only(&sub) {
            continue;
        }
        let sub_id = sub
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or(key.as_str());

        let last_id = sub.get("last_id").and_then(|v| v.as_i64()).unwrap_or(0);
        if event_id <= last_id {
            continue;
        }

        let sql = sub.get("sql").and_then(|v| v.as_str()).unwrap_or("");
        if !sql.is_empty() {
            let filter_query = format!("SELECT 1 FROM events_v WHERE id = ? AND ({})", sql);
            let stored_params: Vec<String> = sub
                .get("params")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            let matched = if stored_params.is_empty() {
                db.conn
                    .query_row(&filter_query, params![event_id], |_| Ok(()))
                    .is_ok()
            } else {
                let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(event_id)];
                for p in &stored_params {
                    all_params.push(Box::new(p.clone()));
                }
                let refs: Vec<&dyn rusqlite::types::ToSql> =
                    all_params.iter().map(|p| p.as_ref()).collect();
                db.conn
                    .query_row(&filter_query, refs.as_slice(), |_| Ok(()))
                    .is_ok()
            };

            if !matched {
                continue;
            }
        }

        let sub_filters = sub
            .get("filters")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if sub_filters.get("request_watch").is_some() {
            let request_id = sub_filters
                .get("request_id")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let target = sub_filters
                .get("target")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let sub_caller = sub.get("caller").and_then(|v| v.as_str()).unwrap_or("");
            if request_id > 0 && !target.is_empty() {
                let waterline: i64 = db
                    .conn
                    .query_row(
                        "SELECT last_event_id FROM instances WHERE name = ?",
                        params![target],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                if waterline < request_id {
                    let mut sub_mut = sub.clone();
                    sub_mut["last_id"] = serde_json::json!(event_id);
                    match serde_json::to_string(&sub_mut) {
                        Ok(json) => {
                            if let Err(e) = db.kv_set(key, Some(&json)) {
                                crate::log::log_error(
                                    "db",
                                    "check_event_subscriptions.kv_set",
                                    &format!("{e}"),
                                );
                            }
                        }
                        Err(e) => {
                            crate::log::log_error(
                                "db",
                                "check_event_subscriptions.serialize",
                                &format!("{e}"),
                            );
                        }
                    }
                    continue;
                }

                if !sub_caller.is_empty() {
                    let already_replied: bool = db
                        .conn
                        .query_row(
                            "SELECT 1 FROM events_v WHERE id > ? AND type = 'message' \
                             AND msg_from = ? AND (\
                               (msg_scope = 'mentions' AND msg_delivered_to LIKE '%' || ? || '%') \
                               OR json_extract(data, '$.reply_to_local') = ? \
                             )",
                            params![request_id, target, sub_caller, request_id],
                            |_| Ok(true),
                        )
                        .unwrap_or(false);
                    if already_replied {
                        if let Err(e) = db.kv_set(key, None) {
                            crate::log::log_error(
                                "db",
                                "check_event_subscriptions.kv_set_cleanup",
                                &format!("{e}"),
                            );
                        }
                        continue;
                    }
                }
            }
        }

        let still_exists: bool = db
            .conn
            .query_row("SELECT 1 FROM kv WHERE key = ?", params![key], |_| Ok(true))
            .unwrap_or(false);
        if !still_exists {
            continue;
        }

        let caller = sub.get("caller").and_then(|v| v.as_str()).unwrap_or("");
        if caller.is_empty() {
            continue;
        }

        let filters_opt = sub.get("filters");
        let notification = format_sub_notification(
            db,
            sub_id,
            event_id,
            event_type,
            instance,
            data,
            filters_opt,
        );
        let _ = send_sub_notification(db, caller, &notification);

        if let Some(on_hit_text) = sub.get("on_hit_text").and_then(|v| v.as_str()) {
            let caller_kind = sub
                .get("caller_kind")
                .and_then(|v| v.as_str())
                .unwrap_or("external");
            if let Err(e) = send_message_as(db, caller, caller_kind, on_hit_text) {
                crate::log::log_error("db", "check_event_subscriptions.on_hit", &format!("{e}"));
            }
        }

        if sub.get("once").and_then(|v| v.as_bool()).unwrap_or(false) {
            if let Err(e) = db.kv_set(key, None) {
                crate::log::log_error(
                    "db",
                    "check_event_subscriptions.kv_set_once",
                    &format!("{e}"),
                );
            }
        } else {
            let mut sub_mut = sub.clone();
            sub_mut["last_id"] = serde_json::json!(event_id);
            match serde_json::to_string(&sub_mut) {
                Ok(json) => {
                    if let Err(e) = db.kv_set(key, Some(&json)) {
                        crate::log::log_error(
                            "db",
                            "check_event_subscriptions.kv_set_cursor",
                            &format!("{e}"),
                        );
                    }
                }
                Err(e) => {
                    crate::log::log_error(
                        "db",
                        "check_event_subscriptions.serialize_cursor",
                        &format!("{e}"),
                    );
                }
            }
        }
    }
}

/// Load all reqwatch subscriptions as (key, parsed_sub, filters) tuples.
pub(crate) fn load_reqwatch_subs(
    db: &HcomDb,
) -> Vec<(String, serde_json::Value, serde_json::Value)> {
    let rows: Vec<(String, String)> = match db
        .conn
        .prepare_cached("SELECT key, value FROM kv WHERE key LIKE 'events_sub:reqwatch-%'")
    {
        Ok(mut stmt) => stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .ok()
            .map(|iter| iter.filter_map(|r| r.ok()).collect())
            .unwrap_or_default(),
        Err(_) => return vec![],
    };

    rows.into_iter()
        .filter_map(|(key, value)| {
            let sub: serde_json::Value = serde_json::from_str(&value).ok()?;
            let filters = sub.get("filters")?.clone();
            Some((key, sub, filters))
        })
        .collect()
}

/// Cancel request-watch subs when watched target messages the requester.
pub(crate) fn cancel_request_watches_by_flow(
    db: &HcomDb,
    sender: &str,
    delivered_to: &[String],
    reply_to_id: Option<i64>,
) {
    for (key, sub, filters) in &load_reqwatch_subs(db) {
        let target = filters.get("target").and_then(|v| v.as_str()).unwrap_or("");
        let sub_caller = sub.get("caller").and_then(|v| v.as_str()).unwrap_or("");

        if target == sender && delivered_to.iter().any(|d| d == sub_caller) {
            if let Some(rid) = reply_to_id {
                let req_id = filters
                    .get("request_id")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                if req_id != rid {
                    continue;
                }
            }
            if let Err(e) = db.kv_set(key, None) {
                crate::log::log_error(
                    "db",
                    "cancel_request_watches_by_flow.kv_set",
                    &format!("{e}"),
                );
            }
        }
    }
}

/// Cancel request-watch subs by explicit reply_to match.
pub(crate) fn cancel_request_watches_by_reply_id(db: &HcomDb, sender: &str, reply_to_id: i64) {
    for (key, _sub, filters) in &load_reqwatch_subs(db) {
        let target = filters.get("target").and_then(|v| v.as_str()).unwrap_or("");
        let req_id = filters
            .get("request_id")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        if target == sender && req_id == reply_to_id {
            if let Err(e) = db.kv_set(key, None) {
                crate::log::log_error(
                    "db",
                    "cancel_request_watches_by_reply.kv_set",
                    &format!("{e}"),
                );
            }
        }
    }
}

/// Send a system notification message.
pub(crate) fn send_system_message(
    db: &HcomDb,
    sender_name: &str,
    message: &str,
) -> Result<Vec<String>> {
    send_message_as(db, sender_name, "system", message)
}

/// Send a message from a specific sender kind.
pub(crate) fn send_message_as(
    db: &HcomDb,
    sender_name: &str,
    sender_kind: &str,
    message: &str,
) -> Result<Vec<String>> {
    let mut stmt = db.conn.prepare_cached("SELECT name, tag FROM instances")?;
    let instances: Vec<(String, Option<String>)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mentions: Vec<String> = MENTION_PATTERN
        .captures_iter(message)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
        .collect();

    let (scope, mention_list, delivered_to) = if mentions.is_empty() {
        let delivered: Vec<String> = instances
            .iter()
            .filter(|(name, _)| name != sender_name)
            .map(|(name, _)| name.clone())
            .collect();
        ("broadcast".to_string(), vec![], delivered)
    } else {
        let mut matched = Vec::new();
        for mention in &mentions {
            let mention_lower = mention.to_lowercase();
            for (name, tag) in &instances {
                let full = match tag.as_ref().filter(|t| !t.is_empty()) {
                    Some(t) => format!("{}-{}", t, name),
                    None => name.clone(),
                };
                if (full.to_lowercase().starts_with(&mention_lower)
                    || name.to_lowercase().starts_with(&mention_lower))
                    && !matched.contains(name)
                {
                    matched.push(name.clone());
                }
            }
        }
        let delivered: Vec<String> = matched
            .iter()
            .filter(|n| n.as_str() != sender_name)
            .cloned()
            .collect();
        ("mentions".to_string(), matched, delivered)
    };

    let mut event_data = serde_json::json!({
        "from": sender_name,
        "sender_kind": sender_kind,
        "scope": scope,
        "text": message,
        "delivered_to": delivered_to,
    });
    if !mention_list.is_empty() {
        event_data["mentions"] = serde_json::json!(mention_list);
    }

    let routing_instance = match sender_kind {
        "instance" => sender_name.to_string(),
        "external" => format!("ext_{}", sender_name),
        _ => format!("sys_{}", sender_name),
    };
    db.log_event("message", &routing_instance, &event_data)?;

    Ok(delivered_to)
}

fn resolve_caller_kind(db: &HcomDb, caller: &str) -> &'static str {
    let exists: bool = db
        .conn()
        .query_row(
            "SELECT 1 FROM instances WHERE name = ?",
            rusqlite::params![caller],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if exists { "instance" } else { "external" }
}

fn collision_self_relevance_sql(caller: &str) -> String {
    let caller_escaped = caller.replace('\'', "''");
    format!(
        "(events_v.instance = '{caller_escaped}' OR EXISTS (SELECT 1 FROM events_v e2 WHERE e2.type = 'status' AND e2.status_context IN {ctx} AND e2.status_detail = events_v.status_detail AND e2.instance = '{caller_escaped}' AND ABS(strftime('%s', events_v.timestamp) - strftime('%s', e2.timestamp)) < 30))",
        ctx = FILE_WRITE_CONTEXTS
    )
}

fn format_sub_notification(
    db: &HcomDb,
    sub_id: &str,
    event_id: i64,
    event_type: &str,
    instance: &str,
    data: &serde_json::Value,
    filters: Option<&serde_json::Value>,
) -> String {
    if let Some(f) = filters {
        if f.get("request_watch").is_some() {
            let request_id = f
                .get("request_id")
                .and_then(|v| v.as_i64())
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".to_string());
            let target = f.get("target").and_then(|v| v.as_str()).unwrap_or(instance);
            let action = if event_type == "status" {
                "went idle"
            } else {
                "stopped"
            };
            return format!(
                "[sub:{}] #{} {} {} without responding to your request #{}",
                sub_id, event_id, target, action, request_id
            );
        }

        if f.get("collision").is_some() && event_type == "status" {
            let file_path = data.get("detail").and_then(|v| v.as_str()).unwrap_or("?");
            if let Some(partner) = find_collision_partner(db, event_id, instance, file_path) {
                return format!(
                    "\u{26a0}\u{fe0f} COLLISION [sub:{}] #{}: {} and {} both edited {}",
                    sub_id, event_id, instance, partner, file_path
                );
            }
            return format!(
                "\u{26a0}\u{fe0f} COLLISION [sub:{}] #{}: {} edited {} (conflict with another agent)",
                sub_id, event_id, instance, file_path
            );
        }
    }

    let mut parts = vec![
        format!("[sub:{}]", sub_id),
        format!("#{}", event_id),
        event_type.to_string(),
        instance.to_string(),
    ];

    match event_type {
        "message" => {
            let mut text = data
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if text.len() > 60 {
                let mut end = 57;
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                text = format!("{}...", &text[..end]);
            }
            text = text.replace('@', "(at)");
            let from = data.get("from").and_then(|v| v.as_str()).unwrap_or("?");
            parts.push(format!("from:{}", from));
            parts.push(format!("\"{}\"", text));
        }
        "status" => {
            parts.push(
                data.get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string(),
            );
            if let Some(ctx) = data.get("context").and_then(|v| v.as_str()) {
                if !ctx.is_empty() {
                    parts.push(ctx.to_string());
                    if let Some(detail) = data.get("detail").and_then(|v| v.as_str()) {
                        if !detail.is_empty() {
                            let truncated = if detail.len() > 40 {
                                if ctx.contains("Bash") {
                                    let end = (0..=37)
                                        .rev()
                                        .find(|&i| detail.is_char_boundary(i))
                                        .unwrap_or(0);
                                    format!("{}...", &detail[..end])
                                } else {
                                    let start = (detail.len().saturating_sub(37)..=detail.len())
                                        .find(|&i| detail.is_char_boundary(i))
                                        .unwrap_or(detail.len());
                                    format!("...{}", &detail[start..])
                                }
                            } else {
                                detail.to_string()
                            };
                            parts.push(truncated);
                        }
                    }
                }
            }
        }
        "life" => {
            parts.push(
                data.get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string(),
            );
            if let Some(by) = data.get("by").and_then(|v| v.as_str()) {
                if !by.is_empty() {
                    parts.push(format!("by:{}", by));
                }
            }
        }
        _ => {}
    }

    parts.join(" | ")
}

fn find_collision_partner(
    db: &HcomDb,
    event_id: i64,
    instance: &str,
    file_path: &str,
) -> Option<String> {
    db.conn
        .query_row(
            &format!(
                "SELECT e.instance FROM events_v e
                 WHERE e.type = 'status' AND e.status_context IN {}
                 AND e.status_detail = ?
                 AND e.instance != ?
                 AND EXISTS (
                     SELECT 1 FROM events_v ev WHERE ev.id = ?
                     AND ABS(strftime('%s', ev.timestamp) - strftime('%s', e.timestamp)) < 30
                 )
                 ORDER BY e.id DESC LIMIT 1",
                FILE_WRITE_CONTEXTS
            ),
            params![file_path, instance, event_id],
            |row| row.get::<_, String>(0),
        )
        .ok()
}

fn send_sub_notification(db: &HcomDb, caller: &str, message: &str) -> bool {
    let row: Option<(String, Option<String>)> = db
        .conn
        .query_row(
            "SELECT name, tag FROM instances WHERE name = ?",
            params![caller],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .ok();

    let Some((name, tag)) = row else {
        return false;
    };

    let full_name = match tag.filter(|t| !t.is_empty()) {
        Some(t) => format!("{}-{}", t, name),
        None => name,
    };

    let text = format!("@{} {}", full_name, message);
    send_system_message(db, "[hcom-events]", &text).is_ok()
}

/// SHA-256 hex hash.
fn sha256_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use std::path::PathBuf;

    fn setup_full_test_db() -> (HcomDb, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(10_000);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!(
            "test_hcom_subscriptions_{}_{}.db",
            std::process::id(),
            test_id
        ));

        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        (db, db_path)
    }

    fn cleanup_test_db(path: PathBuf) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_sha256_hash() {
        let h1 = sha256_hash("test input");
        let h2 = sha256_hash("test input");
        let h3 = sha256_hash("different input");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert_eq!(h1.len(), 64);
        assert_eq!(&h1[..8], "9dfe6f15");
    }

    #[test]
    fn test_collision_self_relevance_matches_filter_constants() {
        let sql = collision_self_relevance_sql("luna");
        assert!(sql.contains(FILE_WRITE_CONTEXTS));
        assert!(sql.contains("< 30"));
        assert!(!sql.contains("tool:edit_file"));
        assert!(!sql.contains("< 20"));
    }

    #[test]
    fn test_subscription_recursion_guard_sys_prefix() {
        let (db, db_path) = setup_full_test_db();

        // Create a subscription
        let sub = serde_json::json!({
            "id": "test-sub",
            "caller": "luna",
            "sql": "type = 'message'",
            "last_id": 0
        });
        db.kv_set("events_sub:test", Some(&sub.to_string()))
            .unwrap();

        // Log event from sys_ instance - should NOT trigger subscription
        let data = serde_json::json!({"from": "[hcom-events]", "text": "test"});
        db.log_event("message", "sys_[hcom-events]", &data).unwrap();

        // Sub should not be updated (last_id should still be 0)
        let sub_after = db.kv_get("events_sub:test").unwrap().unwrap();
        let sub_val: serde_json::Value = serde_json::from_str(&sub_after).unwrap();
        assert_eq!(sub_val["last_id"], 0);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_subscription_recursion_guard_system_sender_kind() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0)",
                [],
            )
            .unwrap();

        let sub = serde_json::json!({
            "id": "test-sub",
            "caller": "luna",
            "sql": "type = 'message'",
            "last_id": 0
        });
        db.kv_set("events_sub:test", Some(&sub.to_string()))
            .unwrap();

        // Log system message - recursion guard should skip
        let data = serde_json::json!({
            "from": "[hcom-events]",
            "sender_kind": "system",
            "text": "notification"
        });
        db.log_event("message", "ext_test", &data).unwrap();

        // Sub should not be updated
        let sub_after = db.kv_get("events_sub:test").unwrap().unwrap();
        let sub_val: serde_json::Value = serde_json::from_str(&sub_after).unwrap();
        assert_eq!(sub_val["last_id"], 0);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_subscription_matches_and_updates_cursor() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, tag, created_at) VALUES ('luna', '', 1000.0)",
                [],
            )
            .unwrap();

        // Create subscription that matches all status events
        let sub = serde_json::json!({
            "id": "test-sub",
            "caller": "luna",
            "sql": "type = 'status'",
            "last_id": 0
        });
        db.kv_set("events_sub:test", Some(&sub.to_string()))
            .unwrap();

        // Log a status event (not from sys_, not system sender_kind)
        let data = serde_json::json!({"status": "active", "context": "test"});
        let event_id = db.log_event("status", "nova", &data).unwrap();

        // Sub should be updated with new last_id
        let sub_after = db.kv_get("events_sub:test").unwrap().unwrap();
        let sub_val: serde_json::Value = serde_json::from_str(&sub_after).unwrap();
        assert_eq!(sub_val["last_id"], event_id);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_subscription_once_removes_after_match() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, tag, created_at) VALUES ('luna', '', 1000.0)",
                [],
            )
            .unwrap();

        let sub = serde_json::json!({
            "id": "once-sub",
            "caller": "luna",
            "sql": "type = 'status'",
            "once": true,
            "last_id": 0
        });
        db.kv_set("events_sub:once-test", Some(&sub.to_string()))
            .unwrap();

        // Log a matching event
        let data = serde_json::json!({"status": "active"});
        db.log_event("status", "nova", &data).unwrap();

        // Subscription should be removed
        assert!(db.kv_get("events_sub:once-test").unwrap().is_none());

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_subscription_sql_error_graceful() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, tag, created_at) VALUES ('luna', '', 1000.0)",
                [],
            )
            .unwrap();

        // Bad SQL subscription
        let bad_sub = serde_json::json!({
            "id": "bad-sql",
            "caller": "luna",
            "sql": "INVALID SQL %%% BROKEN",
            "last_id": 0
        });
        db.kv_set("events_sub:bad", Some(&bad_sub.to_string()))
            .unwrap();

        // Good SQL subscription
        let good_sub = serde_json::json!({
            "id": "good-sql",
            "caller": "luna",
            "sql": "type = 'status'",
            "last_id": 0
        });
        db.kv_set("events_sub:good", Some(&good_sub.to_string()))
            .unwrap();

        // Log a matching event — should not crash despite bad SQL sub
        let data = serde_json::json!({"status": "active"});
        let event_id = db.log_event("status", "nova", &data).unwrap();

        // Bad sub should remain untouched (last_id still 0)
        let bad_after = db.kv_get("events_sub:bad").unwrap().unwrap();
        let bad_val: serde_json::Value = serde_json::from_str(&bad_after).unwrap();
        assert_eq!(bad_val["last_id"], 0, "Bad SQL sub should not advance");

        // Good sub should have fired
        let good_after = db.kv_get("events_sub:good").unwrap().unwrap();
        let good_val: serde_json::Value = serde_json::from_str(&good_after).unwrap();
        assert_eq!(good_val["last_id"], event_id, "Good sub should advance");

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_cancel_request_watches_by_flow() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, tag, created_at) VALUES ('requester', '', 1000.0)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO instances (name, tag, created_at) VALUES ('responder', '', 1000.0)",
                [],
            )
            .unwrap();

        // Create a request-watch subscription
        let reqwatch = serde_json::json!({
            "id": "reqwatch-test",
            "caller": "requester",
            "sql": "type = 'status'",
            "last_id": 0,
            "once": true,
            "filters": {
                "request_watch": true,
                "target": "responder",
                "request_id": 42
            }
        });
        db.kv_set("events_sub:reqwatch-test", Some(&reqwatch.to_string()))
            .unwrap();

        // Simulate responder replying to requester with reply_to matching request_id
        cancel_request_watches_by_flow(&db, "responder", &["requester".to_string()], Some(42));

        // Subscription should be deleted
        assert!(
            db.kv_get("events_sub:reqwatch-test").unwrap().is_none(),
            "Request-watch should be cancelled when target replies"
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_cancel_request_watches_wrong_reply_id() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, tag, created_at) VALUES ('requester', '', 1000.0)",
                [],
            )
            .unwrap();

        let reqwatch = serde_json::json!({
            "id": "reqwatch-test2",
            "caller": "requester",
            "sql": "type = 'status'",
            "last_id": 0,
            "once": true,
            "filters": {
                "request_watch": true,
                "target": "responder",
                "request_id": 42
            }
        });
        db.kv_set("events_sub:reqwatch-test2", Some(&reqwatch.to_string()))
            .unwrap();

        // Reply with wrong request_id — should NOT cancel
        cancel_request_watches_by_flow(&db, "responder", &["requester".to_string()], Some(99));

        assert!(
            db.kv_get("events_sub:reqwatch-test2").unwrap().is_some(),
            "Request-watch should NOT be cancelled for mismatched reply_to"
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_cancel_request_watches_by_reply_id_via_log_event() {
        // End-to-end: log a broadcast message with reply_to_local → should cancel reqwatch via Path 2
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, tag, created_at) VALUES ('requester', '', 1000.0)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO instances (name, tag, created_at) VALUES ('responder', '', 1000.0)",
                [],
            )
            .unwrap();

        // First, log a request message so we have an event_id to reply to
        let req_data = serde_json::json!({
            "from": "requester",
            "sender_kind": "instance",
            "scope": "mentions",
            "text": "do the thing",
            "delivered_to": ["responder"],
            "intent": "request",
            "mentions": ["responder"]
        });
        let request_id = db.log_event("message", "requester", &req_data).unwrap();

        // Create a request-watch subscription
        let reqwatch = serde_json::json!({
            "id": format!("reqwatch-{}-responder", request_id),
            "caller": "requester",
            "sql": "(type='status' AND instance=? AND status_val='listening')",
            "params": ["responder"],
            "last_id": request_id,
            "once": true,
            "filters": {
                "request_watch": true,
                "target": "responder",
                "request_id": request_id
            }
        });
        let sub_key = format!("events_sub:reqwatch-{}-responder", request_id);
        db.kv_set(&sub_key, Some(&reqwatch.to_string())).unwrap();

        // Now log a BROADCAST ack from responder with reply_to_local = request_id
        let ack_data = serde_json::json!({
            "from": "responder",
            "sender_kind": "instance",
            "scope": "broadcast",
            "text": "done with the task",
            "delivered_to": ["requester"],
            "intent": "ack",
            "reply_to": request_id.to_string(),
            "reply_to_local": request_id
        });
        db.log_event("message", "responder", &ack_data).unwrap();

        // Reqwatch should be cancelled via Path 2
        assert!(
            db.kv_get(&sub_key).unwrap().is_none(),
            "Request-watch should be cancelled when target sends broadcast with reply_to_local matching request_id"
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_subscription_recursion_guard_hcom_events_sender() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, tag, created_at) VALUES ('luna', '', 1000.0)",
                [],
            )
            .unwrap();

        let sub = serde_json::json!({
            "id": "test-sub",
            "caller": "luna",
            "sql": "type = 'message'",
            "last_id": 0
        });
        db.kv_set("events_sub:test", Some(&sub.to_string()))
            .unwrap();

        // Log message from [hcom-events] (non-sys_ instance) — guard B should skip
        let data = serde_json::json!({
            "from": "[hcom-events]",
            "text": "notification from events"
        });
        db.log_event("message", "ext_notifier", &data).unwrap();

        // Sub should not be updated
        let sub_after = db.kv_get("events_sub:test").unwrap().unwrap();
        let sub_val: serde_json::Value = serde_json::from_str(&sub_after).unwrap();
        assert_eq!(
            sub_val["last_id"], 0,
            "[hcom-events] sender should be blocked by guard B"
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_send_system_message_broadcast() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('nova', 1000.0)",
                [],
            )
            .unwrap();

        // No @mentions = broadcast
        let delivered = db
            .send_system_message("[hcom-test]", "hello everyone")
            .unwrap();
        assert_eq!(delivered.len(), 2);
        assert!(delivered.contains(&"luna".to_string()));
        assert!(delivered.contains(&"nova".to_string()));

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_send_system_message_targeted() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, tag, created_at) VALUES ('luna', '', 1000.0)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO instances (name, tag, created_at) VALUES ('nova', '', 1000.0)",
                [],
            )
            .unwrap();

        // With @mention = targeted
        let delivered = db
            .send_system_message("[hcom-test]", "@luna your task is done")
            .unwrap();
        assert_eq!(delivered.len(), 1);
        assert!(delivered.contains(&"luna".to_string()));

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_send_system_message_with_tag() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, tag, created_at) VALUES ('luna', 'api', 1000.0)",
                [],
            )
            .unwrap();

        // Mention by full name (tag-name)
        let delivered = db
            .send_system_message("[hcom-test]", "@api-luna your task is done")
            .unwrap();
        assert_eq!(delivered.len(), 1);
        assert!(delivered.contains(&"luna".to_string()));

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_on_hit_provenance_instance_caller() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0), ('nova', 1000.0)",
                [],
            )
            .unwrap();

        let sub = serde_json::json!({
            "id": "sub-onhit1",
            "caller": "luna",
            "caller_kind": "instance",
            "sql": "type = 'message' AND msg_from = 'nova'",
            "created": 1000.0,
            "last_id": 0,
            "once": false,
            "on_hit_text": "starting review now",
        });
        db.kv_set("events_sub:sub-onhit1", Some(&sub.to_string()))
            .unwrap();

        db.log_event(
            "message",
            "nova",
            &serde_json::json!({
                "from": "nova",
                "sender_kind": "instance",
                "scope": "broadcast",
                "text": "heads up",
                "delivered_to": ["luna"],
            }),
        )
        .unwrap();

        // Find the on-hit event: from=luna, sender_kind=instance, text matches
        let row: Option<(String, String)> = db
            .conn
            .query_row(
                "SELECT json_extract(data, '$.sender_kind'), json_extract(data, '$.text') \
                 FROM events WHERE json_extract(data, '$.from') = 'luna' \
                 AND json_extract(data, '$.text') = 'starting review now' LIMIT 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .ok();
        assert!(row.is_some(), "on-hit message should be logged");
        let (kind, text) = row.unwrap();
        assert_eq!(
            kind, "instance",
            "caller 'luna' is an instance → sender_kind=instance"
        );
        assert_eq!(
            text, "starting review now",
            "on-hit text sent verbatim, no @-prefix"
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_on_hit_external_caller_and_mention_routing() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('dbadmin', 1000.0), ('nova', 1000.0)",
                [],
            )
            .unwrap();

        // Caller 'bigboss' is NOT in instances → external kind.
        // on_hit_text mentions @dbadmin → normal mention routing must deliver to dbadmin only.
        let sub = serde_json::json!({
            "id": "sub-onhit2",
            "caller": "bigboss",
            "caller_kind": "external",
            "sql": "type = 'message' AND msg_from = 'nova'",
            "created": 1000.0,
            "last_id": 0,
            "once": false,
            "on_hit_text": "@dbadmin review the change",
        });
        db.kv_set("events_sub:sub-onhit2", Some(&sub.to_string()))
            .unwrap();

        db.log_event(
            "message",
            "nova",
            &serde_json::json!({
                "from": "nova",
                "sender_kind": "instance",
                "scope": "broadcast",
                "text": "trigger",
                "delivered_to": ["dbadmin"],
            }),
        )
        .unwrap();

        let row: Option<(String, String, String)> = db
            .conn
            .query_row(
                "SELECT json_extract(data, '$.sender_kind'), \
                        json_extract(data, '$.scope'), \
                        json_extract(data, '$.delivered_to') \
                 FROM events WHERE json_extract(data, '$.from') = 'bigboss' LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .ok();
        assert!(
            row.is_some(),
            "on-hit message from bigboss should be logged"
        );
        let (kind, scope, delivered) = row.unwrap();
        assert_eq!(
            kind, "external",
            "non-instance caller → sender_kind=external"
        );
        assert_eq!(scope, "mentions", "text contains @mention → mentions scope");
        assert!(
            delivered.contains("dbadmin"),
            "delivered_to must include dbadmin"
        );
        assert!(
            !delivered.contains("bigboss"),
            "caller itself is not auto-mentioned"
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_on_hit_caller_kind_captured_at_creation() {
        // Verify resolve_caller_kind via create_filter_subscription:
        // instance caller → caller_kind=instance
        // non-instance caller (e.g. bigboss from -b) → caller_kind=external
        use std::collections::HashMap;

        let (db, db_path) = setup_full_test_db();
        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0)",
                [],
            )
            .unwrap();

        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert("agent".to_string(), vec!["luna".to_string()]);
        filters.insert("status".to_string(), vec!["listening".to_string()]);

        create_filter_subscription(&db, &filters, &[], "luna", false, Some("hi")).unwrap();
        create_filter_subscription(&db, &filters, &[], "bigboss", false, Some("hi")).unwrap();

        let luna_kind: String = db
            .conn
            .query_row(
                "SELECT json_extract(value, '$.caller_kind') FROM kv \
                 WHERE key LIKE 'events_sub:%' \
                 AND json_extract(value, '$.caller') = 'luna' LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(luna_kind, "instance");

        let bb_kind: String = db
            .conn
            .query_row(
                "SELECT json_extract(value, '$.caller_kind') FROM kv \
                 WHERE key LIKE 'events_sub:%' \
                 AND json_extract(value, '$.caller') = 'bigboss' LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(bb_kind, "external");

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_on_hit_provenance_stable_after_caller_stops() {
        // Sub created by an instance stays sender_kind=instance at fire time
        // even if that instance row has been deleted before the match.
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0), ('nova', 1000.0)",
                [],
            )
            .unwrap();

        let sub = serde_json::json!({
            "id": "sub-stab1",
            "caller": "luna",
            "caller_kind": "instance",
            "sql": "type = 'message' AND msg_from = 'nova'",
            "created": 1000.0,
            "last_id": 0,
            "once": false,
            "on_hit_text": "still luna",
        });
        db.kv_set("events_sub:sub-stab1", Some(&sub.to_string()))
            .unwrap();

        // Caller disappears before the sub fires.
        db.conn
            .execute("DELETE FROM instances WHERE name = 'luna'", [])
            .unwrap();

        db.log_event(
            "message",
            "nova",
            &serde_json::json!({
                "from": "nova",
                "sender_kind": "instance",
                "scope": "broadcast",
                "text": "trigger",
                "delivered_to": [],
            }),
        )
        .unwrap();

        let kind: Option<String> = db
            .conn
            .query_row(
                "SELECT json_extract(data, '$.sender_kind') FROM events \
                 WHERE json_extract(data, '$.from') = 'luna' \
                 AND json_extract(data, '$.text') = 'still luna' LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok();
        assert_eq!(
            kind.as_deref(),
            Some("instance"),
            "provenance captured at creation must survive caller stop"
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_on_hit_unmatched_mention_delivers_to_nobody() {
        // Documents current behavior: an on-hit text mentioning a nonexistent
        // agent produces a mentions-scope event with empty delivered_to.
        // This mirrors how send_system_message behaves for typos — no error,
        // no fallback to broadcast. If we ever tighten mention validation for
        // on-hit, update this test.
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0), ('nova', 1000.0)",
                [],
            )
            .unwrap();

        let sub = serde_json::json!({
            "id": "sub-typo1",
            "caller": "luna",
            "caller_kind": "instance",
            "sql": "type = 'message' AND msg_from = 'nova'",
            "created": 1000.0,
            "last_id": 0,
            "once": false,
            "on_hit_text": "@notarealagent hello",
        });
        db.kv_set("events_sub:sub-typo1", Some(&sub.to_string()))
            .unwrap();

        db.log_event(
            "message",
            "nova",
            &serde_json::json!({
                "from": "nova",
                "sender_kind": "instance",
                "scope": "broadcast",
                "text": "trigger",
                "delivered_to": [],
            }),
        )
        .unwrap();

        let row: Option<(String, String)> = db
            .conn
            .query_row(
                "SELECT json_extract(data, '$.scope'), \
                        json_extract(data, '$.delivered_to') \
                 FROM events WHERE json_extract(data, '$.from') = 'luna' \
                 AND json_extract(data, '$.text') = '@notarealagent hello' LIMIT 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .ok();
        assert!(row.is_some(), "on-hit message should still be logged");
        let (scope, delivered) = row.unwrap();
        assert_eq!(
            scope, "mentions",
            "unmatched @ still produces mentions scope"
        );
        assert_eq!(delivered, "[]", "nobody matched → empty delivered_to");

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_delivery_only_subscription_does_not_emit_notifications() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0), ('nova', 1000.0)",
                [],
            )
            .unwrap();

        let member = serde_json::json!({
            "id": "sub-thread123",
            "caller": "luna",
            "thread_name": "debate-1",
            "auto_thread_member": true,
            "delivery_only": true,
            "created": 1000.0,
            "last_id": 0,
            "once": false
        });
        db.kv_set("events_sub:sub-thread123", Some(&member.to_string()))
            .unwrap();

        let data = serde_json::json!({
            "from": "nova",
            "sender_kind": "instance",
            "scope": "broadcast",
            "text": "hello",
            "delivered_to": ["luna"]
        });
        db.log_event("message", "nova", &data).unwrap();

        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "delivery-only subscriptions must not create notifications"
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_delivery_only_subscription_does_not_emit_status_notifications() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0), ('nova', 1000.0)",
                [],
            )
            .unwrap();

        let member = serde_json::json!({
            "id": "sub-thread123",
            "caller": "luna",
            "thread_name": "debate-1",
            "auto_thread_member": true,
            "delivery_only": true,
            "created": 1000.0,
            "last_id": 0,
            "once": false
        });
        db.kv_set("events_sub:sub-thread123", Some(&member.to_string()))
            .unwrap();

        let data = serde_json::json!({
            "status": "active",
            "context": "tool:shell",
            "detail": "hcom listen 1 --name nova"
        });
        db.log_event("status", "nova", &data).unwrap();

        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "delivery-only subscriptions must not create status notifications"
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_cleanup_subscriptions_keeps_delivery_only_memberships() {
        let (db, db_path) = setup_full_test_db();

        let normal = serde_json::json!({
            "id": "sub-normal",
            "caller": "luna",
            "sql": "type = 'message'",
            "last_id": 0
        });
        let thread_member = serde_json::json!({
            "id": "sub-thread",
            "caller": "luna",
            "thread_name": "debate-1",
            "auto_thread_member": true,
            "delivery_only": true,
            "created": 1000.0,
            "last_id": 0
        });
        db.kv_set("events_sub:sub-normal", Some(&normal.to_string()))
            .unwrap();
        db.kv_set("events_sub:sub-thread", Some(&thread_member.to_string()))
            .unwrap();

        let deleted = db.cleanup_subscriptions("luna").unwrap();
        assert_eq!(deleted, 1);
        assert!(db.kv_get("events_sub:sub-normal").unwrap().is_none());
        assert!(db.kv_get("events_sub:sub-thread").unwrap().is_some());

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_thread_members_filters_stale_names() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0), ('nova', 1000.0)",
                [],
            )
            .unwrap();

        db.add_thread_memberships(
            "debate-1",
            Some("luna"),
            &["nova".to_string(), "ghost".to_string()],
        );

        let stored: String = db
            .conn
            .query_row(
                "SELECT value FROM kv WHERE key = ?",
                params![format!(
                    "events_sub:{}",
                    thread_membership_sub_id("debate-1", "luna")
                )],
                |row| row.get(0),
            )
            .unwrap();
        assert!(stored.contains("\"sql\":\"0\""));

        assert_eq!(
            db.get_thread_members("debate-1"),
            vec!["nova".to_string(), "luna".to_string()]
        );

        cleanup_test_db(db_path);
    }
}
