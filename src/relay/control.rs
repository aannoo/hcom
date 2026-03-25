//! Control events — remote stop/kill via MQTT control topic.
//!
//! Control messages are published to {relay_id}/control (non-retained).
//! Used for cross-device instance management (stop/kill remote agents).

use rumqttc::v5::Client;
use rumqttc::v5::mqttbytes::QoS;
use serde_json::{Value, json};
use std::time::Duration;

use crate::config::HcomConfig;
use crate::db::HcomDb;
use crate::log;

use super::{
    control_topic, device_short_id, is_relay_enabled, read_device_uuid, safe_kv_get, safe_kv_set,
};

/// Build control payload JSON bytes. Returns (topic, payload_bytes) or None.
fn build_control_payload(
    config: &HcomConfig,
    action: &str,
    target: &str,
    target_device_short_id: &str,
) -> Option<(String, Vec<u8>)> {
    if !is_relay_enabled(config) {
        return None;
    }

    let relay_id = &config.relay_id;
    if relay_id.is_empty() {
        return None;
    }

    let device_id = read_device_uuid();
    let short_id = device_short_id(&device_id);

    let now = crate::shared::time::now_epoch_f64();

    let control_payload = json!({
        "from_device": device_id,
        "events": [{
            "ts": now,
            "type": "control",
            "instance": "_control",
            "data": {
                "action": action,
                "target": target,
                "target_device": target_device_short_id,
                "from": format!("_:{}", short_id),
                "from_device": device_id,
            },
        }],
    });

    let topic = control_topic(relay_id);
    let payload_bytes = serde_json::to_vec(&control_payload).ok()?;
    Some((topic, payload_bytes))
}

/// Send a control command to a remote device via MQTT using the daemon's long-lived client.
pub fn send_control(
    config: &HcomConfig,
    client: &Client,
    action: &str,
    target: &str,
    target_device_short_id: &str,
) -> bool {
    let (topic, payload_bytes) =
        match build_control_payload(config, action, target, target_device_short_id) {
            Some(v) => v,
            None => return false,
        };

    match client.publish(&topic, QoS::AtLeastOnce, false, payload_bytes) {
        Ok(_) => {
            log::log_with_fields(
                "INFO",
                "relay",
                "relay.control",
                "",
                &[
                    ("action", action),
                    ("target", &format!("{}:{}", target, target_device_short_id)),
                ],
            );
            true
        }
        Err(e) => {
            log::log_warn("relay", "relay.network", &format!("control: {}", e));
            false
        }
    }
}

/// Send a control command via an ephemeral client, waiting for PUBACK (5s timeout).
fn send_control_via_ephemeral(
    config: &HcomConfig,
    client: &super::client::EphemeralClient,
    action: &str,
    target: &str,
    target_device_short_id: &str,
) -> bool {
    let (topic, payload_bytes) =
        match build_control_payload(config, action, target, target_device_short_id) {
            Some(v) => v,
            None => return false,
        };

    let result = client.publish_and_wait(
        &topic,
        QoS::AtLeastOnce,
        false,
        payload_bytes,
        Duration::from_secs(5),
    );

    if result {
        log::log_with_fields(
            "INFO",
            "relay",
            "relay.control",
            "",
            &[
                ("action", action),
                ("target", &format!("{}:{}", target, target_device_short_id)),
            ],
        );
    } else {
        log::log_warn("relay", "relay.network", "control: PUBACK timeout");
    }

    result
}

/// Send a control command using an ephemeral client (for CLI callers without
/// a long-lived relay connection). Waits for PUBACK (up to 5s) before disconnecting.
pub fn send_control_ephemeral(
    config: &HcomConfig,
    action: &str,
    target: &str,
    target_device_short_id: &str,
) -> bool {
    let ephemeral = match super::client::create_ephemeral_client(config) {
        Some(c) => c,
        None => return false,
    };

    let result =
        send_control_via_ephemeral(config, &ephemeral, action, target, target_device_short_id);

    ephemeral.disconnect();
    result
}

/// Process incoming control events targeting this device.
/// Deduplicates by timestamp to avoid re-processing.
pub fn handle_control_events(
    db: &HcomDb,
    events: &[Value],
    own_short_id: &str,
    source_device: &str,
) {
    let last_ctrl_ts: f64 = safe_kv_get(db, &format!("relay_ctrl_{}", source_device))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);

    let mut max_ctrl_ts = last_ctrl_ts;

    for event in events {
        if event.get("type").and_then(|v| v.as_str()) != Some("control") {
            continue;
        }

        let event_ts = event.get("ts").and_then(|v| v.as_f64()).unwrap_or(0.0);

        // Dedup by timestamp
        if event_ts <= last_ctrl_ts {
            continue;
        }
        max_ctrl_ts = max_ctrl_ts.max(event_ts);

        let data = match event.get("data") {
            Some(d) => d,
            None => continue,
        };

        let target_device = data
            .get("target_device")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_uppercase();

        if target_device != own_short_id.to_uppercase() {
            continue; // Not for us
        }

        let action = data.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let target = match data.get("target").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => continue,
        };

        match action {
            "stop" => {
                let initiated_by = data
                    .get("from")
                    .and_then(|v| v.as_str())
                    .unwrap_or("remote");
                crate::hooks::common::stop_instance(db, target, initiated_by, "remote");
                log::log_with_fields(
                    "INFO",
                    "relay",
                    "relay.control_recv",
                    "",
                    &[
                        ("action", "stop"),
                        ("target", target),
                        ("from", initiated_by),
                    ],
                );
            }
            "start" => {
                // Remote start: log only. Local device would need to actually start the process.
                log::log_with_fields(
                    "INFO",
                    "relay",
                    "relay.control_recv",
                    "",
                    &[("action", "start"), ("target", target), ("ignored", "true")],
                );
            }
            _ => {}
        }
    }

    // Persist dedup timestamp
    if max_ctrl_ts > last_ctrl_ts {
        safe_kv_set(
            db,
            &format!("relay_ctrl_{}", source_device),
            Some(&max_ctrl_ts.to_string()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_handle_control_events_filters_by_target() {
        // Control events targeting a different device should be ignored
        let events = vec![json!({
            "type": "control",
            "ts": 1000.0,
            "data": {
                "action": "stop",
                "target": "luna",
                "target_device": "ABCD",
                "from": "_:EFGH",
            }
        })];

        // own_short_id is "WXYZ" — event targets "ABCD", so nothing should happen
        let db =
            HcomDb::open_raw(&tempfile::NamedTempFile::new().unwrap().into_temp_path()).unwrap();
        handle_control_events(&db, &events, "WXYZ", "device-123");

        // No crash, no panic — event was filtered
    }
}
