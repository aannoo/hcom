"""Cross-device relay - state-embedded events

Custom server implementation:
  Required endpoints:
    POST /push/{device_id}  — receive JSON: {"state": {...}, "events": [...]}
    GET /poll?since=&timeout=  — long-poll, return {"devices": {...}, "ts": float}
    GET /version  — return {"v": 1}
    GET /devices  — return list of active devices

  Reference: https://huggingface.co/spaces/aannoo/hcom-relay/blob/main/app.py

  Configure with:
    hcom config relay https://your-server.example.com
    hcom config relay_token <token>  # optional auth
"""

from __future__ import annotations
import json
import os
import sqlite3
import threading
import time
import urllib.request
import urllib.error
import socket
from typing import Any

from .core.device import get_device_uuid, get_device_short_id
from .core.db import get_db, log_event, kv_get, kv_set, _write_lock
from .core.config import get_config
from .core.log import log_info, log_warn, log_error
from .shared import parse_iso_timestamp

_poll_ts: float = 0
_poll_ts_lock = threading.Lock()
_relay_worker_flag = threading.local()


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
        # Success: claim ownership
        _safe_kv_set("relay_status_owner", pid)
        _safe_kv_set("relay_status", "ok")
        _safe_kv_set("relay_last_error", None)
    else:
        # Error: only write if we're the owner (the process that last succeeded)
        if owner == pid or not daemon_active:
            _safe_kv_set("relay_status", status)
            _safe_kv_set("relay_last_error", error)


def _get_relay_url() -> str | None:
    """Get relay URL from config (returns None if disabled)."""
    config = get_config()
    if not config.relay_enabled:
        return None
    url = config.relay
    return url.rstrip("/") if url and url.startswith("http") else None


def is_relay_enabled() -> bool:
    """Check if relay is configured AND enabled."""
    config = get_config()
    return bool(config.relay and config.relay_enabled)


def _get_auth_headers() -> dict[str, str]:
    """Get auth headers."""
    headers = {"Content-Type": "application/json"}
    if token := get_config().relay_token:
        headers["Authorization"] = f"Bearer {token}"
    return headers


_last_http_error: str = ""

# *.hf.space proxy fallback state
# None = untested, True = use fallback, False = fallback probe failed
_hf_space_fallback: bool | None = None
_hf_space_fallback_lock = threading.Lock()


def _has_https_proxy() -> bool:
    """Check if an HTTPS proxy is configured."""
    return bool(os.environ.get("HTTPS_PROXY") or os.environ.get("https_proxy"))


def _rewrite_hf_space_url(url: str) -> tuple[str, str | None]:
    """Rewrite *.hf.space URL for proxy compatibility.

    Returns (rewritten_url, original_host) or (url, None) if no rewrite needed.

    Workaround: *.hf.space TLS hangs through some HTTP CONNECT proxies because
    HF Spaces run on direct EC2/ALB (not CloudFront). Routing through huggingface.co
    with the original Host header works when the proxy does TLS termination and
    re-resolves destinations based on Host (e.g. Envoy dynamic_forward_proxy).
    This is fragile and proxy-specific — it won't work on passthrough proxies.
    """
    import re
    m = re.match(r'https://([^/]+\.hf\.space)(/.*)$', url)
    if not m:
        return url, None
    host = m.group(1)
    path = m.group(2)
    return f"https://huggingface.co{path}", host


def _http(method: str, url: str, data: bytes | None = None, timeout: int = 5) -> tuple[int, bytes]:
    """HTTP request. On connection failure, sets _last_http_error with reason.

    For *.hf.space URLs behind a proxy: on first timeout, tries Host-header
    fallback through huggingface.co. Caches result so subsequent calls don't
    pay the timeout penalty. If cached fallback fails, retries direct once
    to self-heal when proxy/env changes.
    """
    global _last_http_error, _hf_space_fallback

    is_hf_space = ".hf.space" in url
    has_proxy = is_hf_space and _has_https_proxy()

    # If we already know fallback works, use it — but retry direct on failure
    if has_proxy and _hf_space_fallback is True:
        rewritten_url, host_header = _rewrite_hf_space_url(url)
        status, body = _http_raw(method, rewritten_url, data=data, timeout=timeout, host_override=host_header)
        if status == 200:
            return status, body
        # Fallback failed — maybe env changed (proxy removed, NO_PROXY set).
        # Retry direct once to self-heal.
        direct_status, direct_body = _http_raw(method, url, data=data, timeout=timeout)
        if direct_status > 0:
            _hf_space_fallback = None  # Reset — direct works now
            return direct_status, direct_body
        return status, body  # Return the fallback result (better error than timeout)

    # Normal request
    status, body = _http_raw(method, url, data=data, timeout=timeout)

    # On timeout for *.hf.space through a proxy, try fallback
    if status == 0 and has_proxy and "timed out" in _last_http_error:
        with _hf_space_fallback_lock:
            if _hf_space_fallback is True:
                # Another thread already confirmed fallback works
                rewritten_url, host_header = _rewrite_hf_space_url(url)
                return _http_raw(method, rewritten_url, data=data, timeout=timeout, host_override=host_header)
            if _hf_space_fallback is None:
                rewritten_url, host_header = _rewrite_hf_space_url(url)
                if host_header:
                    log_info("relay", "relay.hf_fallback", msg="trying Host-header fallback")
                    fb_status, fb_body = _http_raw(method, rewritten_url, data=data, timeout=timeout, host_override=host_header)
                    if fb_status == 200:
                        _hf_space_fallback = True
                        log_info("relay", "relay.hf_fallback", msg="fallback works, using for future requests")
                        return fb_status, fb_body
                    else:
                        _hf_space_fallback = False
                        _last_http_error = "timed out (*.hf.space unreachable through proxy — add *.hf.space to NO_PROXY or proxy allowlist)"

    return status, body


def _http_raw(method: str, url: str, data: bytes | None = None, timeout: int = 5, host_override: str | None = None) -> tuple[int, bytes]:
    """Low-level HTTP request."""
    global _last_http_error
    req = urllib.request.Request(url, data=data, method=method)
    for k, v in _get_auth_headers().items():
        req.add_header(k, v)
    if host_override:
        req.add_header("Host", host_override)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            _last_http_error = ""
            return (resp.status, resp.read())
    except urllib.error.HTTPError as e:
        _last_http_error = ""
        return (e.code, b"")
    except (urllib.error.URLError, socket.timeout) as e:
        reason = str(getattr(e, "reason", e))
        if "CERTIFICATE_VERIFY_FAILED" in reason:
            _last_http_error = "SSL certificate error"
        elif "timed out" in reason.lower():
            _last_http_error = "timed out"
        else:
            _last_http_error = "network unreachable"
        return (0, b"")


# ==================== State ====================


def build_state() -> dict[str, Any]:
    """Build current instance state snapshot."""
    conn = get_db()
    rows = conn.execute("""
        SELECT name, status, status_context, status_time, parent_name, session_id, parent_session_id, agent_id, directory, transcript_path, wait_timeout, last_stop, tcp_mode
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


def push(force: bool = False) -> tuple[bool, str | None]:
    """Push state and new events to server.

    Returns:
        (success, error_message) - error_message is None on success
    """
    url = _get_relay_url()
    if not url:
        return (False, None)  # Not configured, not an error

    # Rate limit
    if not force:
        last = float(_safe_kv_get("relay_last_push") or 0)
        if time.time() - last < 1.0:
            return (True, None)

    device_id = get_device_uuid()
    state = build_state()

    # Get new events since last push (exclude imported events - they have _relay marker)
    last_push_id = int(_safe_kv_get("relay_last_push_id") or 0)
    conn = get_db()
    rows = conn.execute(
        """
        SELECT id, timestamp, type, instance, data FROM events
        WHERE id > ? AND instance NOT LIKE '%:%'
        AND instance != '_device'
        AND json_extract(data, '$._relay') IS NULL
        ORDER BY id LIMIT 100
    """,
        (last_push_id,),
    ).fetchall()

    events = []
    max_id = last_push_id
    for row in rows:
        events.append(
            {
                "id": row["id"],  # Monotonic event ID for dedup on pull
                "ts": row["timestamp"],
                "type": row["type"],
                "instance": row["instance"],
                "data": json.loads(row["data"]),
            }
        )
        max_id = max(max_id, row["id"])

    status, content = _http(
        "POST",
        f"{url}/push/{device_id}",
        data=json.dumps({"state": state, "events": events}).encode(),
        timeout=3,
    )
    if status == 200:
        # KV update after POST — if process dies here, events re-push next cycle.
        # Pull-side dedup (monotonic event ID) handles duplicates on the receiver.
        _safe_kv_set("relay_last_push", str(time.time()))
        _safe_kv_set("relay_last_push_id", str(max_id))
        _set_relay_status("ok")
        log_info("relay", "relay.push", events=len(events))
        return (True, None)
    elif status == 0:
        error = _last_http_error or "network unreachable"
        _set_relay_status("error", error)
        log_warn("relay", "relay.network", error)  # Transient - WARN appropriate
        return (False, error)
    else:
        error = f"server returned {status}"
        _set_relay_status("error", error)
        log_warn("relay", "relay.network", error)  # Transient - WARN appropriate
        return (False, error)


# ==================== Pull ====================


def pull(timeout: int = 0) -> tuple[dict[str, Any], str | None]:
    """Pull remote devices. timeout=0 for immediate, >0 for long-poll.

    Returns:
        (result_dict, error_message) - error_message is None on success
    """
    global _poll_ts

    url = _get_relay_url()
    if not url:
        return ({"devices": {}}, None)  # Not configured

    with _poll_ts_lock:
        since = _poll_ts

    status, content = _http(
        "GET",
        f"{url}/poll?since={since}&timeout={timeout}",
        timeout=timeout + 5 if timeout else 5,
    )
    if status == 200 and content:
        try:
            result = json.loads(content)
        except json.JSONDecodeError as e:
            error = f"invalid json: {e}"
            _set_relay_status("error", error)
            log_error("relay", "relay.error", e, msg="pull: invalid json")
            return ({"devices": {}}, error)

        with _poll_ts_lock:
            new_ts = result.get("ts", _poll_ts)
            if new_ts > _poll_ts:
                _poll_ts = new_ts
        _set_relay_status("ok")

        devices = result.get("devices", {})
        if devices:
            own_device = get_device_uuid()
            _apply_remote_devices(devices, own_device)

        return (result, None)
    elif status == 0:
        error = _last_http_error or "network unreachable"
        _set_relay_status("error", error)
        log_warn("relay", "relay.network", error)
        return ({"devices": {}}, error)
    else:
        error = f"server returned {status}"
        _set_relay_status("error", error)
        log_warn("relay", "relay.network", error)
        return ({"devices": {}}, error)


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
                            name, origin_device_id, status, status_context, status_time,
                            parent_name, directory, transcript_path, created_at,
                            session_id, parent_session_id, agent_id, wait_timeout, last_stop, tcp_mode
                        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                        ON CONFLICT(name) DO UPDATE SET
                            status = excluded.status,
                            status_context = excluded.status_context, status_time = excluded.status_time,
                            parent_name = excluded.parent_name,
                            directory = excluded.directory, transcript_path = excluded.transcript_path,
                            session_id = excluded.session_id, parent_session_id = excluded.parent_session_id,
                            agent_id = excluded.agent_id, wait_timeout = excluded.wait_timeout,
                            last_stop = excluded.last_stop, tcp_mode = excluded.tcp_mode
                    """,
                        (
                            namespaced,
                            device_id,
                            inst.get("status", "unknown"),
                            inst.get("context", ""),
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
                        ),
                    )
                    conn.commit()
            except sqlite3.Error as e:
                try:
                    conn.rollback()
                except Exception:
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


def send_control(action: str, target: str, device_short_id: str) -> bool:
    """Send control command to remote device."""
    url = _get_relay_url()
    if not url:
        return False

    device_id = get_device_uuid()
    short_id = get_device_short_id()

    control_event = {
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
    }

    # Push immediately with control event
    state = build_state()
    status, _ = _http(
        "POST",
        f"{url}/push/{device_id}",
        data=json.dumps({"state": state, "events": [control_event]}).encode(),
        timeout=3,
    )
    if status == 200:
        log_info(
            "relay",
            "relay.control",
            action=action,
            target=f"{target}:{device_short_id}",
        )
        return True
    elif status == 0:
        log_warn("relay", "relay.network", f"control: {_last_http_error or 'network unreachable'}")
    else:
        log_warn("relay", "relay.network", f"control: status={status}")
    return False


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
        except Exception:
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


# ==================== Wait Helper ====================


def relay_wait(timeout: float = 25.0) -> bool:
    """Drop-in replacement for sync_wait(). Returns True if new data imported.

    Used by cmd_events --wait and cmd_listen.
    """
    # If daemon is polling, just sleep to preserve timing behavior
    if is_relay_handled_by_daemon():
        time.sleep(min(timeout, 1.0))
        return False

    # Push first (rate-limited internally)
    push()

    # Pull with long-poll
    result, _ = pull(timeout=int(min(timeout, 25)))

    return bool(result.get("devices"))


def get_relay_status() -> dict[str, Any]:
    """Get relay status for TUI display.

    Returns dict with:
        configured: bool - relay URL is set
        enabled: bool - relay is enabled (config flag)
        status: 'ok' | 'error' | None - last operation result
        error: str | None - last error message
        last_push: float - timestamp of last successful push
    """
    config = get_config()
    return {
        "configured": bool(config.relay),
        "enabled": config.relay_enabled,
        "status": _safe_kv_get("relay_status"),
        "error": _safe_kv_get("relay_last_error"),
        "last_push": float(_safe_kv_get("relay_last_push") or 0),
    }


# ==================== Public API ====================

__all__ = [
    "push",
    "pull",
    "relay_wait",
    "build_state",
    "send_control",
    "get_relay_status",
    "is_relay_enabled",
    "is_relay_handled_by_daemon",
    "notify_relay_daemon",
    "notify_relay",
]
