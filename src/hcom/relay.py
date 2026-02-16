"""Cross-device relay via MQTT pub/sub.

Topic layout:
  {relay_id}/{device_uuid}  — retained state per device
  {relay_id}/control        — non-retained control events (kill, etc.)

Configure with:
  hcom relay new                           — create new relay group
  hcom relay connect <token>               — join existing relay
  hcom relay new --broker mqtts://host:port  — private broker
"""

from __future__ import annotations
import json
import os
import sqlite3
import threading
import time
import socket
from typing import Any

from .core.device import get_device_uuid, get_device_short_id
from .core.db import get_db, log_event, kv_get, kv_set, _write_lock
from .core.config import get_config
from .core.log import log_info, log_warn, log_error
from .shared import parse_iso_timestamp

_relay_worker_flag = threading.local()

# ==================== MQTT Broker Defaults ====================

# Public brokers (TLS). Tried in order during initial setup; first success gets pinned.
DEFAULT_BROKERS = [
    ("broker.emqx.io", 8883),
    ("broker.hivemq.com", 8883),
    ("test.mosquitto.org", 8886),
]


def _safe_kv_get(key: str, default: str | None = None) -> str | None:
    """kv_get that won't crash on DB errors."""
    try:
        return kv_get(key) or default
    except Exception:
        return default


def _safe_kv_set(key: str, value: str | None) -> None:
    """kv_set that won't crash on DB errors."""
    try:
        kv_set(key, value)
    except Exception:
        pass


def _clear_short_id(device_id: str) -> None:
    """Remove relay_short_* mapping for a device (reverse lookup by value)."""
    try:
        from .core.db import kv_prefix
        for key, val in kv_prefix("relay_short_").items():
            if val == device_id:
                _safe_kv_set(key, None)
                break
    except Exception:
        pass


def _mark_as_relay_worker() -> None:
    """Mark current thread as relay worker (daemon threads only)."""
    _relay_worker_flag.is_worker = True


def _is_relay_worker() -> bool:
    return bool(getattr(_relay_worker_flag, "is_worker", False))


def _set_relay_status(status: str, error: str | None = None) -> None:
    """Write relay status to KV. Uses PID lock so only one process owns status."""
    is_worker = _is_relay_worker()
    daemon_active = False
    if not is_worker:
        daemon_active = is_relay_handled_by_daemon()
        if daemon_active:
            return
    pid = str(os.getpid())
    owner = _safe_kv_get("relay_status_owner")
    if status == "ok":
        _safe_kv_set("relay_status_owner", pid)
        _safe_kv_set("relay_status", "ok")
        _safe_kv_set("relay_last_error", None)
    else:
        if owner == pid or not daemon_active:
            _safe_kv_set("relay_status", status)
            _safe_kv_set("relay_last_error", error)


def is_relay_enabled() -> bool:
    """Check if relay is configured AND enabled. Requires relay_id to be set."""
    config = get_config()
    return bool(config.relay_id and config.relay_enabled)


def _get_broker_address() -> tuple[str, int] | None:
    """Get MQTT broker (host, port) from config. Returns None if relay not configured."""
    config = get_config()
    if not config.relay_id or not config.relay_enabled:
        return None
    url = config.relay.strip()
    if url:
        # Parse mqtts://host:port or mqtt://host:port
        import urllib.parse
        parsed = urllib.parse.urlparse(url)
        host = parsed.hostname or url
        port = parsed.port or (8883 if parsed.scheme == "mqtts" else 1883)
        return (host, port)
    return None  # No broker pinned yet — daemon will try fallback list


def _get_relay_id() -> str | None:
    """Get relay_id from config."""
    return get_config().relay_id or None


def _use_tls(broker_port: int) -> bool:
    """Determine if TLS should be used for this broker."""
    config = get_config()
    url = config.relay.strip()
    if url:
        return not url.startswith("mqtt://")  # mqtts:// or unknown → TLS
    # Public brokers: ports 8883/8886 use TLS
    return broker_port in (8883, 8886)


def _state_topic(relay_id: str, device_uuid: str) -> str:
    """Topic for device state: {relay_id}/{device_uuid}"""
    return f"{relay_id}/{device_uuid}"


def _control_topic(relay_id: str) -> str:
    """Topic for control events: {relay_id}/control"""
    return f"{relay_id}/control"


def _wildcard_topic(relay_id: str) -> str:
    """Wildcard subscription: {relay_id}/+ (matches all device + control topics)"""
    return f"{relay_id}/+"


# ==================== State ====================


def build_state() -> dict[str, Any]:
    """Build current instance state snapshot."""
    conn = get_db()
    rows = conn.execute("""
        SELECT name, status, status_context, status_detail, status_time, parent_name, session_id,
            parent_session_id, agent_id, directory, transcript_path, wait_timeout, last_stop,
            tcp_mode, tag, tool, background
        FROM instances WHERE COALESCE(origin_device_id, '') = ''
    """).fetchall()

    instances = {}
    for row in rows:
        name = row["name"]
        if name.startswith("_") or name.startswith("sys_"):
            continue
        instances[name] = {
            "enabled": True,  # Row exists = participating
            "status": row["status"] or "unknown",
            "context": row["status_context"] or "",
            "status_time": row["status_time"] or 0,
            "parent": row["parent_name"] or None,
            "session_id": row["session_id"] or None,
            "parent_session_id": row["parent_session_id"] or None,
            "agent_id": row["agent_id"] or None,
            "directory": row["directory"] or None,
            "transcript": row["transcript_path"] or None,
            "wait_timeout": row["wait_timeout"] or 86400,
            "last_stop": row["last_stop"] or 0,
            "tcp_mode": bool(row["tcp_mode"]),
            "tag": row["tag"] or None,
            "tool": row["tool"] or "claude",
            "background": bool(row["background"]),
            "detail": row["status_detail"] or "",
        }

    # Get reset timestamp (local only - exclude imported events)
    reset_row = conn.execute("""
        SELECT timestamp FROM events
        WHERE type = 'life' AND instance = '_device'
        AND json_extract(data, '$.action') = 'reset'
        AND json_extract(data, '$._relay') IS NULL
        ORDER BY id DESC LIMIT 1
    """).fetchone()

    reset_ts = 0.0
    if reset_row and reset_row["timestamp"]:
        dt = parse_iso_timestamp(reset_row["timestamp"])
        if dt:
            reset_ts = dt.timestamp()

    return {
        "instances": instances,
        "short_id": get_device_short_id(),
        "reset_ts": reset_ts,
    }


# ==================== Push ====================


def build_push_payload() -> tuple[dict[str, Any], list[dict], int, bool]:
    """Build the state + events payload for publishing.

    Returns (state, events, max_event_id, has_more).
    Fetches 101 rows, sends first 100 — has_more=True if 101st exists.
    """
    state = build_state()

    last_push_id = int(_safe_kv_get("relay_last_push_id") or 0)
    conn = get_db()
    rows = conn.execute(
        """
        SELECT id, timestamp, type, instance, data FROM events
        WHERE id > ? AND instance NOT LIKE '%:%'
        AND instance != '_device'
        AND json_extract(data, '$._relay') IS NULL
        ORDER BY id LIMIT 101
    """,
        (last_push_id,),
    ).fetchall()

    has_more = len(rows) > 100
    send_rows = rows[:100]

    events = []
    max_id = last_push_id
    for row in send_rows:
        events.append(
            {
                "id": row["id"],
                "ts": row["timestamp"],
                "type": row["type"],
                "instance": row["instance"],
                "data": json.loads(row["data"]),
            }
        )
        max_id = max(max_id, row["id"])

    return state, events, max_id, has_more


def push(mqtt_client: Any = None, **_kw: Any) -> tuple[bool, str | None, bool]:
    """Push state and new events via MQTT.

    Args:
        mqtt_client: paho.mqtt.client.Client instance (from daemon). If None, not connected.

    Returns:
        (success, error_message, has_more) - has_more=True if more events remain to push
    """
    if not is_relay_enabled():
        return (False, None, False)

    if mqtt_client is None:
        return (False, None, False)  # No MQTT client — daemon handles publishing

    if not mqtt_client.is_connected():
        return (False, None, False)  # Disconnected — paho auto-reconnect will retry

    relay_id = _get_relay_id()
    if not relay_id:
        return (False, None, False)

    device_id = get_device_uuid()
    state, events, max_id, has_more = build_push_payload()

    payload = json.dumps({"state": state, "events": events}).encode()
    topic = _state_topic(relay_id, device_id)

    try:
        # Publish with retain=True so new subscribers get latest state.
        # QoS 1 (at-least-once) — pull-side dedup handles duplicates.
        t0 = time.time()
        result = mqtt_client.publish(topic, payload, qos=1, retain=True)
        result.wait_for_publish(timeout=5)
        publish_ms = int((time.time() - t0) * 1000)

        if not result.is_published():
            _set_relay_status("error", "publish not confirmed")
            return (False, "publish not confirmed", False)

        _safe_kv_set("relay_last_push", str(time.time()))
        _safe_kv_set("relay_last_push_id", str(max_id))
        _set_relay_status("ok")
        log_info("relay", "relay.push", events=len(events), publish_ms=publish_ms,
                 payload_bytes=len(payload))
        return (True, None, has_more)
    except Exception as e:
        # Don't advance cursor — events will be retried on next push
        error = str(e) or "mqtt publish failed"
        _set_relay_status("error", error)
        log_warn("relay", "relay.network", error)
        return (False, error, False)


# ==================== Pull ====================


def handle_mqtt_message(topic: str, payload: bytes, relay_id: str) -> None:
    """Process an incoming MQTT message (called from daemon's on_message).

    Routes by topic suffix:
      {relay_id}/control → _handle_control_events
      {relay_id}/{device_uuid} → _apply_remote_devices
    """
    prefix = relay_id + "/"
    if not topic.startswith(prefix):
        return  # Not our relay group — ignore (safety on shared public brokers)

    suffix = topic[len(prefix):]

    if not payload:
        # Empty payload = device disconnected (LWT or graceful cleanup).
        if suffix and suffix != "control":
            device_id = suffix
            try:
                conn = get_db()
                with _write_lock:
                    conn.execute("DELETE FROM instances WHERE origin_device_id = ?", (device_id,))
                    conn.commit()
                _safe_kv_set(f"relay_sync_time_{device_id}", None)
                _clear_short_id(device_id)
                log_info("relay", "relay.device_gone", device=device_id[:8])
            except Exception:
                pass
        return

    try:
        data = json.loads(payload)
    except (json.JSONDecodeError, UnicodeDecodeError) as e:
        log_warn("relay", "relay.bad_payload", error=str(e))
        return
    own_device = get_device_uuid()

    if suffix == "control":
        # Control event — process directly
        own_short_id = get_device_short_id()
        events = data.get("events", [data] if data.get("type") == "control" else [])
        source_device = data.get("from_device", "unknown")
        if source_device != own_device:
            _handle_control_events(events, own_short_id, source_device)
        return

    # State message from a device
    device_id = suffix
    if device_id == own_device:
        return  # Ignore own messages

    t0 = time.time()
    devices = {device_id: data}
    _apply_remote_devices(devices, own_device)
    apply_ms = int((time.time() - t0) * 1000)
    n_events = len(data.get("events", []))
    n_instances = len(data.get("state", {}).get("instances", {}))
    short_id = data.get("state", {}).get("short_id", device_id[:4].upper())
    log_info("relay", "relay.recv", device=short_id, events=n_events,
             instances=n_instances, apply_ms=apply_ms,
             payload_bytes=len(payload))


def _apply_remote_devices(devices: dict[str, dict], own_device: str) -> None:
    """Apply remote device state and events."""
    conn = get_db()
    own_short_id = get_device_short_id()

    # Get local reset timestamp from KV (set by cmd_reset for cross-process reliability)
    # Fallback to events table for long-running pollers that missed the KV write
    local_reset_ts = float(_safe_kv_get("relay_local_reset_ts") or 0)
    if local_reset_ts == 0:
        row = conn.execute("""
            SELECT timestamp FROM events
            WHERE type='life' AND instance='_device'
              AND json_extract(data, '$.action')='reset'
              AND json_extract(data, '$._relay') IS NULL
            ORDER BY id DESC LIMIT 1
        """).fetchone()
        if row:
            local_reset_ts = _parse_ts(row[0])
            if local_reset_ts:
                _safe_kv_set("relay_local_reset_ts", str(local_reset_ts))
    if local_reset_ts == 0:
        log_warn("relay", "relay.warn", "local_reset_ts=0, quarantine disabled")

    for device_id, payload in devices.items():
        if device_id == own_device:
            continue

        state = payload.get("state", {})
        events = payload.get("events", [])
        short_id = state.get("short_id", device_id[:4].upper())
        reset_ts = state.get("reset_ts", 0)

        # Detect short_id collision: two different devices with same short_id
        # Would cause instance primary key collisions (name:SHORT)
        cached_device = _safe_kv_get(f"relay_short_{short_id}")
        if cached_device and cached_device != device_id:
            log_warn(
                "relay",
                "relay.collision",
                short_id=short_id,
                existing=cached_device[:8],
                incoming=device_id[:8],
            )
            continue  # Skip this device to prevent data corruption
        if not cached_device:
            _safe_kv_set(f"relay_short_{short_id}", device_id)

        # Check for device reset FIRST - always clean old data before deciding to import
        # This must run even if we later skip the device (stale check)
        cached_reset = float(_safe_kv_get(f"relay_reset_{device_id}") or 0)
        if reset_ts > cached_reset:
            with _write_lock:
                conn.execute("DELETE FROM instances WHERE origin_device_id = ?", (device_id,))
                conn.execute(
                    "DELETE FROM events WHERE json_extract(data, '$._relay.device') = ?",
                    (device_id,),
                )
                conn.commit()
            _safe_kv_set(f"relay_reset_{device_id}", str(reset_ts))
            # Reset event cursor so new events from restarted device are imported
            _safe_kv_set(f"relay_events_{device_id}", "0")
            log_info("relay", "relay.reset", device=short_id)

        # Note: Device-level quarantine removed - caused deadlocks when devices reset at different times.
        # Per-event and per-instance timestamp filtering handles stale data instead.

        # Upsert instances from state and remove stale ones atomically
        seen_instances = set()
        for name, inst in state.get("instances", {}).items():
            # Skip instances with no activity or activity from before our reset
            status_time = inst.get("status_time", 0)
            if local_reset_ts > 0 and status_time < local_reset_ts:
                continue

            namespaced = f"{name}:{short_id}"
            seen_instances.add(namespaced)
            # Namespace parent with short_id suffix
            parent_namespaced = f"{inst['parent']}:{short_id}" if inst.get("parent") else None
            # Relay-origin rows must not populate UNIQUE local identifiers
            relay_session_id = None
            relay_parent_session_id = None
            relay_agent_id = None
            try:
                with _write_lock:
                    conn.execute(
                        """
                        INSERT INTO instances (
                            name, origin_device_id, status, status_context, status_detail, status_time,
                            parent_name, directory, transcript_path, created_at,
                            session_id, parent_session_id, agent_id, wait_timeout, last_stop, tcp_mode,
                            tag, tool, background
                        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                        ON CONFLICT(name) DO UPDATE SET
                            status = excluded.status,
                            status_context = excluded.status_context, status_detail = excluded.status_detail,
                            status_time = excluded.status_time,
                            parent_name = excluded.parent_name,
                            directory = excluded.directory, transcript_path = excluded.transcript_path,
                            session_id = excluded.session_id, parent_session_id = excluded.parent_session_id,
                            agent_id = excluded.agent_id, wait_timeout = excluded.wait_timeout,
                            last_stop = excluded.last_stop, tcp_mode = excluded.tcp_mode,
                            tag = excluded.tag, tool = excluded.tool, background = excluded.background
                    """,
                        (
                            namespaced,
                            device_id,
                            inst.get("status", "unknown"),
                            inst.get("context", ""),
                            inst.get("detail", ""),
                            inst.get("status_time", 0),
                            parent_namespaced,
                            inst.get("directory"),
                            inst.get("transcript"),
                            time.time(),
                            relay_session_id,
                            relay_parent_session_id,
                            relay_agent_id,
                            inst.get("wait_timeout", 86400),
                            inst.get("last_stop", 0),
                            inst.get("tcp_mode", False),
                            inst.get("tag"),
                            inst.get("tool", "claude"),
                            inst.get("background", False),
                        ),
                    )
                    conn.commit()
            except sqlite3.Error as e:
                try:
                    conn.rollback()
                except sqlite3.Error:
                    pass
                log_error("relay", "relay.error", e, op="instance_upsert", instance=namespaced)

        # Remove instances no longer in state (stopped/removed on remote)
        # Read + delete under same lock to avoid TOCTOU race
        with _write_lock:
            current_remote = {
                row["name"]
                for row in conn.execute("SELECT name FROM instances WHERE origin_device_id = ?", (device_id,)).fetchall()
            }
            stale = current_remote - seen_instances
            if stale:
                for name in stale:
                    conn.execute("DELETE FROM instances WHERE name = ?", (name,))
                conn.commit()

        # Handle control events targeting this device
        _handle_control_events(events, own_short_id, device_id)

        # Insert events (for history + message delivery)
        # Dedup by monotonic event ID (not timestamp - avoids clock skew issues)
        last_event_id = int(_safe_kv_get(f"relay_events_{device_id}") or 0)

        # Detect ID regression: remote DB was recreated without proper reset event
        # SQLite autoincrement IDs never decrease, so regression = DB recreation
        if events and last_event_id > 0:
            remote_max_id = max(
                (e.get("id", 0) for e in events if e.get("type") != "control"),
                default=0,
            )
            if remote_max_id > 0 and remote_max_id < last_event_id:
                log_info(
                    "relay",
                    "relay.reset",
                    device=short_id,
                    reason=f"id_regression:{remote_max_id}<{last_event_id}",
                )
                with _write_lock:
                    conn.execute("DELETE FROM instances WHERE origin_device_id = ?", (device_id,))
                    conn.execute(
                        "DELETE FROM events WHERE json_extract(data, '$._relay.device') = ?",
                        (device_id,),
                    )
                    conn.commit()
                last_event_id = 0
                _safe_kv_set(f"relay_events_{device_id}", "0")

        max_event_id = last_event_id

        for event in events:
            # Skip control events (already handled above)
            if event.get("type") == "control":
                continue

            # Skip _device events (reset_ts is in state, not events)
            if event.get("instance") == "_device":
                continue

            raw_event_id = event.get("id", 0)
            try:
                event_id = int(raw_event_id)
            except (TypeError, ValueError):
                log_warn("relay", "relay.bad_event_id", device=short_id, raw=raw_event_id)
                continue
            if event_id <= last_event_id:
                continue  # Already have this event

            # Skip events from before our reset (stale data from peer's old DB)
            event_ts = _parse_ts(event.get("ts", 0))
            if local_reset_ts > 0 and event_ts > 0 and event_ts < local_reset_ts:
                continue

            # Namespace instance
            instance = event.get("instance", "")
            if instance and ":" not in instance and not instance.startswith("_"):
                instance = f"{instance}:{short_id}"

            # Namespace 'from' and 'mentions' in message data
            data = event.get("data", {}).copy()
            if "from" in data and ":" not in data["from"]:
                data["from"] = f"{data['from']}:{short_id}"

            # Strip our device suffix from mentions so local instances match
            if "mentions" in data:
                data["mentions"] = [
                    name.rsplit(":", 1)[0] if name.upper().endswith(f":{own_short_id}") else name
                    for name in data["mentions"]
                ]

            # Strip our device suffix from delivered_to so local instances match
            if "delivered_to" in data:
                data["delivered_to"] = [
                    name.rsplit(":", 1)[0] if name.upper().endswith(f":{own_short_id}") else name
                    for name in data["delivered_to"]
                ]

            # Store relay origin for cross-device reply_to resolution
            data["_relay"] = {"device": device_id, "short": short_id, "id": event_id}

            log_event(
                event_type=event.get("type", "unknown"),
                instance=instance,
                data=data,
                timestamp=event.get("ts"),
            )
            max_event_id = max(max_event_id, event_id)

            # Log relay latency for message events
            if event.get("type") == "message" and event_ts > 0:
                latency_ms = int((time.time() - event_ts) * 1000)
                log_info("relay", "relay.msg_recv", device=short_id,
                         remote_id=event_id, latency_ms=latency_ms,
                         **{"from": data.get("from", "?")})

        if max_event_id > last_event_id:
            _safe_kv_set(f"relay_events_{device_id}", str(max_event_id))

        # Update sync timestamp for this device (separate from event ID cursor)
        _safe_kv_set(f"relay_sync_time_{device_id}", str(time.time()))

    # Wake local TCP instances so they see new messages immediately
    from .core.runtime import notify_all_instances

    notify_all_instances()


def _parse_ts(ts) -> float:
    """Parse timestamp to float."""
    if isinstance(ts, (int, float)):
        return float(ts)
    if isinstance(ts, str):
        dt = parse_iso_timestamp(ts)
        if dt:
            return dt.timestamp()
    return 0.0


# ==================== Remote Control ====================


def _create_ephemeral_client() -> Any:
    """Create a short-lived MQTT client for one-shot publishes (CLI callers).

    Connects to the pinned broker, starts loop, waits for CONNACK.
    Returns connected client or None on failure.
    """
    try:
        import paho.mqtt.client as mqtt
        from paho.mqtt.enums import CallbackAPIVersion, MQTTProtocolVersion
    except ImportError:
        return None

    broker = _get_broker_address()
    if not broker:
        return None
    host, port = broker

    client = mqtt.Client(
        CallbackAPIVersion.VERSION2,
        protocol=MQTTProtocolVersion.MQTTv5,
    )
    if _use_tls(port):
        client.tls_set()

    config = get_config()
    if config.relay_token:
        client.username_pw_set(username="hcom", password=config.relay_token)

    connected = threading.Event()

    def on_connect(client, userdata, flags, reason_code, properties):
        if reason_code == 0:
            connected.set()

    client.on_connect = on_connect

    try:
        client.connect(host, port, keepalive=30)
        client.loop_start()
        if not connected.wait(timeout=5):
            client.loop_stop()
            client.disconnect()
            return None
        return client
    except Exception:
        try:
            client.loop_stop()
            client.disconnect()
        except Exception:
            pass
        return None


def send_control(action: str, target: str, device_short_id: str, mqtt_client: Any = None) -> bool:
    """Send control command to remote device via MQTT.

    Args:
        mqtt_client: paho.mqtt.client.Client instance (from daemon).
            If None, creates an ephemeral client (for CLI callers like start/stop).
    """
    if not is_relay_enabled():
        return False

    relay_id = _get_relay_id()
    if not relay_id:
        return False

    device_id = get_device_uuid()
    short_id = get_device_short_id()

    control_payload = {
        "from_device": device_id,
        "events": [{
            "ts": time.time(),
            "type": "control",
            "instance": "_control",
            "data": {
                "action": action,
                "target": target,
                "target_device": device_short_id,
                "from": f"_:{short_id}",
                "from_device": device_id,
            },
        }],
    }

    topic = _control_topic(relay_id)

    # Create ephemeral client if no daemon client provided
    ephemeral = False
    if mqtt_client is None:
        mqtt_client = _create_ephemeral_client()
        if mqtt_client is None:
            return False
        ephemeral = True

    try:
        result = mqtt_client.publish(topic, json.dumps(control_payload).encode(), qos=1, retain=False)
        result.wait_for_publish(timeout=5)
        log_info(
            "relay",
            "relay.control",
            action=action,
            target=f"{target}:{device_short_id}",
        )
        return True
    except Exception as e:
        log_warn("relay", "relay.network", f"control: {e}")
        return False
    finally:
        if ephemeral:
            try:
                mqtt_client.loop_stop()
                mqtt_client.disconnect()
            except Exception:
                pass


def _handle_control_events(events: list[dict], own_short_id: str, source_device: str) -> None:
    """Process control events targeting this device."""
    from .core.tool_utils import stop_instance

    # Dedup: skip already-processed control events from this device
    last_ctrl_ts = float(_safe_kv_get(f"relay_ctrl_{source_device}") or 0)
    max_ctrl_ts = last_ctrl_ts

    for event in events:
        if event.get("type") != "control":
            continue

        # Timestamp dedup
        event_ts = _parse_ts(event.get("ts", 0))
        if event_ts <= last_ctrl_ts:
            continue
        max_ctrl_ts = max(max_ctrl_ts, event_ts)

        data = event.get("data", {})
        target_device = data.get("target_device", "").upper()

        if target_device != own_short_id:
            continue  # Not for us

        action = data.get("action")
        target = data.get("target")

        if not target:
            continue

        if action == "stop":
            initiated_by = data.get("from", "remote")
            stop_instance(target, initiated_by=initiated_by, reason="remote")
            log_info(
                "relay",
                "relay.control_recv",
                action="stop",
                target=target,
                from_=initiated_by,
            )
        elif action == "start":
            # Remote start: with row-exists = participating model, can't re-enable stopped instances
            # Just log the request - local device would need to actually start the process
            log_info(
                "relay",
                "relay.control_recv",
                action="start",
                target=target,
                ignored=True,
            )

    # Persist dedup timestamp
    if max_ctrl_ts > last_ctrl_ts:
        _safe_kv_set(f"relay_ctrl_{source_device}", str(max_ctrl_ts))


# ==================== Daemon Notification ====================


def is_relay_handled_by_daemon() -> bool:
    """Check if daemon is actively handling relay polling.

    Validates port is actually reachable to handle stale ports from crashed daemons.
    Only clears port after consecutive failures to avoid stampede from transient timeouts.
    """
    port = _safe_kv_get("relay_daemon_port")
    if not port:
        return False

    # Actually verify the port is reachable (handles crash scenarios)
    sock = None
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(0.1)  # 100ms timeout
        sock.connect(("127.0.0.1", int(port)))
        _safe_kv_set("relay_daemon_fail_count", None)  # Reset on success
        return True
    except Exception:
        # Track consecutive failures — only clear port after 3 in a row
        # Prevents stampede when daemon port is briefly busy
        # Atomic increment to avoid lost updates under concurrency
        try:
            conn = get_db()
            with _write_lock:
                conn.execute(
                    "INSERT INTO kv (key, value) VALUES ('relay_daemon_fail_count', '1') "
                    "ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)"
                )
                conn.commit()
                row = conn.execute("SELECT value FROM kv WHERE key = 'relay_daemon_fail_count'").fetchone()
            fail_count = int(row[0]) if row else 1
            if fail_count >= 3:
                _safe_kv_set("relay_daemon_port", None)
                _safe_kv_set("relay_daemon_fail_count", None)
        except (sqlite3.Error, ValueError, TypeError):
            pass
        return False
    finally:
        if sock:
            try:
                sock.close()
            except Exception:
                pass


def notify_relay_daemon() -> bool:
    """Notify daemon to push. Returns True if daemon is listening."""
    port = _safe_kv_get("relay_daemon_port")
    if not port:
        return False
    sock = None
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(0.05)  # 50ms timeout
        sock.connect(("127.0.0.1", int(port)))
        return True
    except Exception:
        return False
    finally:
        if sock:
            try:
                sock.close()
            except Exception:
                pass


def notify_relay() -> bool:
    """Notify relay handler (daemon) to push immediately."""
    return notify_relay_daemon()


def trigger_push() -> None:
    """Notify daemon to push; fall back to direct push if daemon isn't running."""
    if not notify_relay_daemon():
        push()


def clear_retained_state() -> bool:
    """Publish empty retained message to clear this device's state from broker.

    Used by 'relay off' so remote devices stop seeing stale instances.
    Creates an ephemeral MQTT client if daemon isn't available.
    """
    if not is_relay_enabled():
        return False
    relay_id = _get_relay_id()
    if not relay_id:
        return False
    device_id = get_device_uuid()
    topic = _state_topic(relay_id, device_id)

    client = _create_ephemeral_client()
    if not client:
        return False
    try:
        result = client.publish(topic, b"", qos=1, retain=True)
        result.wait_for_publish(timeout=5)
        return result.is_published()
    except Exception:
        return False
    finally:
        try:
            client.loop_stop()
            client.disconnect()
        except Exception:
            pass


# ==================== Wait Helper ====================


def relay_wait(timeout: float = 25.0) -> bool:
    """Wait for relay data. Returns True if new data arrived in DB.

    Used by cmd_events --wait, cmd_listen, and relay poll.
    With MQTT, the daemon's on_message writes to DB in real-time.
    We poll DB for new remote events (instances with : in name).
    """
    try:
        conn = get_db()
        before = conn.execute(
            "SELECT MAX(id) FROM events WHERE instance LIKE '%:%'"
        ).fetchone()[0] or 0
    except Exception:
        before = 0

    time.sleep(min(timeout, 1.0))

    try:
        conn = get_db()
        after = conn.execute(
            "SELECT MAX(id) FROM events WHERE instance LIKE '%:%'"
        ).fetchone()[0] or 0
        return after > before
    except Exception:
        return False


def get_relay_status() -> dict[str, Any]:
    """Get relay status for TUI display.

    Returns dict with:
        configured: bool - relay_id is set
        enabled: bool - relay is enabled (config flag)
        status: 'ok' | 'error' | None - last operation result
        error: str | None - last error message
        last_push: float - timestamp of last successful push
        broker: str | None - current broker URL
    """
    config = get_config()
    return {
        "configured": bool(config.relay_id),
        "enabled": config.relay_enabled,
        "status": _safe_kv_get("relay_status"),
        "error": _safe_kv_get("relay_last_error"),
        "last_push": float(_safe_kv_get("relay_last_push") or 0),
        "broker": config.relay or None,
    }


def get_remote_devices(max_age: float = 90.0) -> dict[str, dict]:
    """Get remote devices with recent sync activity.

    Derives device list from relay_short_{short_id} → device_id mapping in KV,
    filtered by relay_sync_time freshness.  Excludes own device and stale entries.

    Args:
        max_age: Maximum seconds since last sync to consider a device alive.
                 0 = return all known devices regardless of staleness.
    """
    from .core.db import kv_prefix
    from .core.device import get_device_uuid

    own = get_device_uuid()
    now = time.time()

    # relay_short_{short_id} → device_id  (invert to device_id → short_id)
    short_map = kv_prefix("relay_short_")
    device_to_short: dict[str, str] = {}
    for key, device_id in short_map.items():
        if device_id == own:
            continue
        short_id = key.removeprefix("relay_short_")
        device_to_short[device_id] = short_id

    if not device_to_short:
        return {}

    # Look up sync times, filter by staleness
    sync_map = kv_prefix("relay_sync_time_")
    result = {}
    for device_id, short_id in device_to_short.items():
        sync_val = sync_map.get(f"relay_sync_time_{device_id}")
        sync_time = float(sync_val) if sync_val else 0.0
        if max_age > 0 and (not sync_time or (now - sync_time) > max_age):
            continue
        result[device_id] = {"short_id": short_id, "sync_time": sync_time}

    return result


# ==================== Public API ====================

__all__ = [
    "push",
    "build_push_payload",
    "handle_mqtt_message",
    "relay_wait",
    "build_state",
    "send_control",
    "clear_retained_state",
    "get_relay_status",
    "get_remote_devices",
    "is_relay_enabled",
    "is_relay_handled_by_daemon",
    "notify_relay_daemon",
    "notify_relay",
    "trigger_push",
    "DEFAULT_BROKERS",
]
