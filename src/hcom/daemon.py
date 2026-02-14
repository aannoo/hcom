"""Threaded daemon for fast hook/CLI handling.

Reduces hcom latency from ~200ms to <20ms by:
1. Pre-loading all Python modules at daemon startup
2. Rust client connects via Unix socket (fast startup)
3. Daemon handles requests with already-loaded code

Protocol: JSON-lines over Unix socket at ~/.hcom/hcomd.sock

Request format:
    {"version": 1, "request_id": "...", "kind": "hook"|"cli", ...}

Response format:
    {"exit_code": 0, "stdout": "...", "stderr": "...", "request_id": "..."}
"""

from __future__ import annotations

import fcntl
import json
import os
import signal
import socket
import socketserver
import sqlite3
import threading
import time
import traceback
import uuid
from pathlib import Path
from typing import Any, TYPE_CHECKING

if TYPE_CHECKING:
    from .core.hcom_context import HcomContext
    from .core.hook_payload import HookPayload
    from .core.hook_result import HookResult

# Thread-local I/O capture (enables concurrent CLI requests)
from .core.io_capture import (
    _thread_streams,
    CaptureBuffer,
    MockStdin,
    install_thread_local_streams,
)

PROTOCOL_VERSION = 1
IDLE_TIMEOUT = 1800  # 30 minutes
PID_FILE_NAME = "hcomd.pid"
SOCKET_FILE_NAME = "hcomd.sock"
VERSION_FILE_NAME = "daemon.version"  # In .tmp/ subdir
MAX_REQUEST_SIZE = 16 * 1024 * 1024  # 16MB limit (matches previous asyncio limit)

_last_request_time: float | None = None
_shutdown_event: threading.Event | None = None
_server: "ThreadingUnixServer | None" = None
_active_requests: int = 0
_active_lock: threading.Lock = threading.Lock()
_cleanup_done: threading.Event = threading.Event()
_relay_manager: "RelayManager | None" = None


def get_socket_path() -> Path:
    """Get socket path, respecting HCOM_DIR for sandbox mode."""
    hcom_dir = os.environ.get("HCOM_DIR")
    if hcom_dir:
        return Path(hcom_dir).expanduser() / SOCKET_FILE_NAME
    return Path.home() / ".hcom" / SOCKET_FILE_NAME


def get_pid_path() -> Path:
    """Get PID file path for single-instance guarantee."""
    hcom_dir = os.environ.get("HCOM_DIR")
    if hcom_dir:
        return Path(hcom_dir).expanduser() / PID_FILE_NAME
    return Path.home() / ".hcom" / PID_FILE_NAME


def get_version_path() -> Path:
    """Get version file path for client version checking.

    Written on daemon startup, read by Rust client to detect version mismatch.
    Stored in .tmp/ as ephemeral runtime state.
    """
    hcom_dir = os.environ.get("HCOM_DIR")
    if hcom_dir:
        return Path(hcom_dir).expanduser() / ".tmp" / VERSION_FILE_NAME
    return Path.home() / ".hcom" / ".tmp" / VERSION_FILE_NAME


def get_daemon_info(include_stats: bool = False) -> dict:
    """Get daemon status information.

    Args:
        include_stats: If True, parse logs for performance stats (slower)

    Returns dict with:
        running: bool - True if daemon process is running
        pid: int | None - PID if running
        socket_path: str - Path to Unix socket
        socket_exists: bool - True if socket file exists
        responsive: bool - True if socket accepts connections
        stats: dict (if include_stats) - request_count, avg_ms, max_ms
    """
    import socket as sock_module

    socket_path = get_socket_path()
    pid_path = get_pid_path()

    info: dict[str, Any] = {
        "running": False,
        "pid": None,
        "socket_path": str(socket_path),
        "socket_exists": socket_path.exists(),
        "responsive": False,
    }

    if not pid_path.exists():
        return info

    try:
        pid = int(pid_path.read_text().strip())
        # Check if process is alive
        os.kill(pid, 0)
        info["running"] = True
        info["pid"] = pid

        # Check if socket is responsive
        if info["socket_exists"]:
            try:
                s = sock_module.socket(sock_module.AF_UNIX, sock_module.SOCK_STREAM)
                s.settimeout(1.0)
                s.connect(str(socket_path))
                s.close()
                info["responsive"] = True
            except Exception:
                pass
    except (ValueError, OSError, ProcessLookupError):
        pass

    if include_stats:
        info["stats"] = _get_daemon_stats()

    return info


def _get_daemon_stats(hours: float = 1.0) -> dict:
    """Parse logs for daemon request stats."""
    from datetime import datetime, timezone, timedelta
    from .core.log import get_log_path

    path = get_log_path()
    if not path.exists():
        return {"request_count": 0, "avg_ms": 0, "max_ms": 0}

    cutoff = datetime.now(timezone.utc) - timedelta(hours=hours)
    latencies: list[float] = []
    max_ms = 0.0

    try:
        with open(path, "r") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    entry = json.loads(line)
                    if entry.get("event") != "request.done":
                        continue
                    ts = _parse_log_timestamp(entry.get("ts"))
                    if ts and ts < cutoff:
                        continue
                    duration = entry.get("duration_ms", 0)
                    latencies.append(duration)
                    if duration > max_ms:
                        max_ms = duration
                except (json.JSONDecodeError, ValueError):
                    continue
    except Exception:
        pass

    return {
        "request_count": len(latencies),
        "avg_ms": round(sum(latencies) / len(latencies), 1) if latencies else 0,
        "max_ms": round(max_ms, 1),
    }


def _parse_log_timestamp(ts_value):
    """Parse log timestamp - handles both ISO string and Unix epoch float."""
    from datetime import datetime, timezone

    if isinstance(ts_value, (int, float)):
        # Unix epoch (old Rust format)
        return datetime.fromtimestamp(ts_value, tz=timezone.utc)
    elif isinstance(ts_value, str):
        # ISO format (new format)
        try:
            return datetime.fromisoformat(ts_value.replace("Z", "+00:00"))
        except ValueError:
            return None
    return None


def get_delivery_stats(hours: float = 1.0) -> dict:
    """Get message delivery stats from logs."""
    from datetime import datetime, timezone, timedelta
    from .core.log import get_log_path

    path = get_log_path()
    if not path.exists():
        return {"delivered": 0, "failed": 0, "failure_reasons": {}}

    cutoff = datetime.now(timezone.utc) - timedelta(hours=hours)
    delivered = 0
    failed = 0
    failure_reasons: dict[str, int] = {}

    try:
        with open(path, "r") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    entry = json.loads(line)
                    event = entry.get("event", "")
                    if not event.startswith("delivery."):
                        continue
                    ts = _parse_log_timestamp(entry.get("ts"))
                    if ts and ts < cutoff:
                        continue
                    if event in ("delivery.success", "delivery.success_no_cursor"):
                        delivered += 1
                    elif event in ("delivery.failed", "delivery.verify_timeout"):
                        failed += 1
                        reason = event.replace("delivery.", "")
                        failure_reasons[reason] = failure_reasons.get(reason, 0) + 1
                except (json.JSONDecodeError, ValueError):
                    continue
    except Exception:
        pass

    return {
        "delivered": delivered,
        "failed": failed,
        "failure_reasons": failure_reasons,
    }


def _log_info(event: str, msg: str = "", **fields) -> None:
    """Log INFO to unified hcom.log (JSONL)."""
    from .core.log import log_info
    log_info("daemon", event, msg, **fields)


def _log_warn(event: str, msg: str = "", **fields) -> None:
    """Log WARN to unified hcom.log (JSONL)."""
    from .core.log import log_warn
    log_warn("daemon", event, msg, **fields)


def _log_error(event: str, error: Exception | str, msg: str = "", **fields) -> None:
    """Log ERROR to unified hcom.log (JSONL)."""
    from .core.log import log_error
    log_error("daemon", event, error, msg, **fields)


def _mqtt_reason(rc: object) -> str:
    """Translate MQTT reason code to human-readable string.

    Handles both int (paho v1) and ReasonCode object (paho v2).
    """
    # paho-mqtt v2 passes ReasonCode object - convert to int for dict lookup
    rc_int = getattr(rc, "value", rc)
    if not isinstance(rc_int, int):
        return f"unknown ({rc})"
    reasons = {
        1: "unsupported protocol version",
        2: "invalid client ID",
        3: "server unavailable",
        4: "bad username or password",
        5: "not authorized",
        128: "unspecified error",
        132: "bad authentication method",
        133: "topic name invalid",
        135: "not authorized",
        137: "server busy",
        138: "banned",
        140: "bad authentication method",
        144: "topic filter invalid",
        149: "packet too large",
        153: "quota exceeded",
        160: "connection rate exceeded",
    }
    return reasons.get(rc_int, f"unknown ({rc})")


class RelayManager:
    """Background MQTT relay worker for daemon.

    Single paho-mqtt Client with loop_start() handles both receiving (on_message)
    and publishing (push via client.publish()). A TCP notify server lets CLI
    processes wake the push loop.
    """

    def __init__(self, shutdown_event: threading.Event) -> None:
        self.shutdown_event = shutdown_event
        self.push_event = threading.Event()
        self.notify_server: socket.socket | None = None
        self.push_thread: threading.Thread | None = None
        self.mqtt_client: Any = None

    def start(self) -> None:
        self._setup_notify_server()
        self._connect_mqtt()
        self.push_thread = threading.Thread(target=self._push_loop, daemon=True)
        self.push_thread.start()

    def stop(self) -> None:
        try:
            from .core.db import kv_set

            kv_set("relay_daemon_port", None)
            kv_set("relay_daemon_fail_count", None)
        except Exception:
            pass
        if self.notify_server:
            try:
                self.notify_server.close()
            except Exception:
                pass
            self.notify_server = None
        # Graceful MQTT disconnect: publish empty retained to own topic, then disconnect
        if self.mqtt_client:
            try:
                from .relay import _get_relay_id, _state_topic
                from .core.device import get_device_uuid

                relay_id = _get_relay_id()
                if relay_id:
                    topic = _state_topic(relay_id, get_device_uuid())
                    result = self.mqtt_client.publish(topic, b"", qos=1, retain=True)
                    result.wait_for_publish(timeout=5)
            except Exception:
                pass
            try:
                self.mqtt_client.loop_stop()
                self.mqtt_client.disconnect()
            except Exception:
                pass
            self.mqtt_client = None
        self.push_event.set()

    def reconnect(self) -> None:
        """Tear down MQTT client so push loop reconnects with fresh config."""
        if self.mqtt_client:
            try:
                self.mqtt_client.loop_stop()
                self.mqtt_client.disconnect()
            except Exception:
                pass
            self.mqtt_client = None
        from .core.config import reload_config
        reload_config()
        # Push loop will see mqtt_client is None and call _connect_mqtt()
        self.push_event.set()

    def _connect_mqtt(self) -> None:
        """Create and connect the MQTT client."""
        from .relay import (
            is_relay_enabled, _get_relay_id, _get_broker_address, _use_tls,
            _state_topic, _wildcard_topic, _mark_as_relay_worker,
            _set_relay_status, handle_mqtt_message,
        )
        from .core.device import get_device_uuid

        if not is_relay_enabled():
            return

        relay_id = _get_relay_id()
        if not relay_id:
            return

        try:
            import paho.mqtt.client as mqtt
            from paho.mqtt.enums import CallbackAPIVersion, MQTTProtocolVersion
        except ImportError:
            _log_error("relay.mqtt", "paho-mqtt not installed", "relay disabled")
            return

        device_uuid = get_device_uuid()
        client_id = f"hcom-{device_uuid[:8]}"

        # Try MQTT v5 first (needed for message_expiry_interval)
        client = mqtt.Client(
            CallbackAPIVersion.VERSION2,
            client_id=client_id,
            protocol=MQTTProtocolVersion.MQTTv5,
        )

        # Set LWT: empty retained payload on our state topic (clears state on ungraceful disconnect)
        will_topic = _state_topic(relay_id, device_uuid)
        client.will_set(will_topic, payload=b"", qos=1, retain=True)

        # Callbacks
        connected_event = threading.Event()

        def on_connect(client, userdata, flags, reason_code, properties=None):
            _mark_as_relay_worker()  # Mark paho's loop thread as relay worker
            if reason_code != 0:
                msg = f"connect refused: {_mqtt_reason(reason_code)}"
                _set_relay_status("error", msg)
                _log_error("relay.mqtt", msg)
                connected_event.set()  # Unblock wait, but connection failed
                return
            client.subscribe(_wildcard_topic(relay_id), qos=1)
            _set_relay_status("ok")
            _log_info("relay.mqtt", "connected to broker")
            connected_event.set()  # Unblock wait

        def on_disconnect(client, userdata, flags, reason_code, properties=None):
            # Paho's reconnect_delay_set(1, 60) handles auto-reconnect with backoff.
            # No broker re-pinning — token must stay stable across devices.
            _log_warn("relay.mqtt", f"on_disconnect: reason={_mqtt_reason(reason_code)}")
            if reason_code != 0:
                _set_relay_status("error", f"disconnected: {_mqtt_reason(reason_code)}")

        def on_message(client, userdata, msg):
            try:
                handle_mqtt_message(msg.topic, msg.payload, relay_id)
            except Exception as e:
                _log_error("relay.on_message", e, f"topic={msg.topic}")

        client.on_connect = on_connect
        client.on_disconnect = on_disconnect
        client.on_message = on_message

        # Limit paho's internal queue to prevent unbounded memory during outages
        client.max_queued_messages_set(100)
        client.max_inflight_messages_set(20)
        # Auto-reconnect with backoff
        client.reconnect_delay_set(min_delay=1, max_delay=60)

        # Determine broker
        broker_addr = _get_broker_address()
        if not broker_addr:
            # No broker pinned — try fallback list
            broker_addr = self._find_working_broker()
            if not broker_addr:
                _set_relay_status("error", "all brokers unreachable")
                return

        host, port = broker_addr

        # TLS (auto-negotiates highest version)
        if _use_tls(port):
            client.tls_set()

        # Auth (optional, for custom brokers)
        from .core.config import get_config
        config = get_config()
        if config.relay_token:
            client.username_pw_set(username="hcom", password=config.relay_token)

        try:
            client.connect(host, port, keepalive=60)
            client.loop_start()
            # Wait for CONNACK before returning - prevents push_loop race
            if not connected_event.wait(timeout=5.0):
                _log_warn("relay.mqtt", f"connect timeout: {host}:{port}")
                _set_relay_status("error", "connect timeout")
                try:
                    client.loop_stop()
                    client.disconnect()
                except Exception:
                    pass
                return
            # Check if on_connect reported failure
            if not client.is_connected():
                try:
                    client.loop_stop()
                    client.disconnect()
                except Exception:
                    pass
                return
            self.mqtt_client = client
        except Exception as e:
            _log_error("relay.mqtt", e, f"connect failed: {host}:{port}")
            _set_relay_status("error", f"connect failed: {e}")

    def _find_working_broker(self) -> tuple[str, int] | None:
        """Try each public broker, pin the first that works. Returns (host, port) or None."""
        from .relay import DEFAULT_BROKERS

        for host, port in DEFAULT_BROKERS:
            try:
                # Brief test connection
                import ssl as _ssl
                test_sock = socket.create_connection((host, port), timeout=5)
                ctx = _ssl.create_default_context()
                test_sock = ctx.wrap_socket(test_sock, server_hostname=host)
                test_sock.close()

                # Pin this broker to config
                from .core.config import load_config_snapshot, save_config_snapshot
                snapshot = load_config_snapshot()
                snapshot.core.relay = f"mqtts://{host}:{port}"
                save_config_snapshot(snapshot)
                _log_info("relay.mqtt", f"pinned broker: {host}:{port}")
                return host, port
            except Exception:
                continue

        return None

    def _setup_notify_server(self) -> None:
        """TCP server for CLI→daemon push wake notifications."""
        try:
            from .core.db import kv_set

            self.notify_server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            self.notify_server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            self.notify_server.bind(("127.0.0.1", 0))
            self.notify_server.listen(16)
            self.notify_server.setblocking(False)
            relay_port = self.notify_server.getsockname()[1]
            kv_set("relay_daemon_port", str(relay_port))
        except OSError as e:
            _log_error("relay.notify_server", e, "Failed to setup relay notification server")
            self.notify_server = None

    def _check_notify_server(self) -> None:
        """Accept pending TCP notifications (non-blocking) and trigger push."""
        if not self.notify_server:
            return
        while True:
            try:
                conn, _ = self.notify_server.accept()
                conn.close()
                self.push_event.set()
            except BlockingIOError:
                break
            except Exception:
                break

    def _wait_for_notify_or_timeout(self, timeout: float) -> bool:
        """Wait for notify socket activity, push_event, or timeout.

        Uses select() on the notify socket so we wake immediately when
        a client connects (e.g. hcom send triggers notify_relay()).
        Returns True if woken by notify/event, False on timeout.
        """
        import select as _select

        deadline = time.time() + timeout
        while not self.shutdown_event.is_set():
            remaining = deadline - time.time()
            if remaining <= 0:
                return False
            if self.push_event.is_set():
                return True
            # select() on notify socket — wakes on incoming TCP connection
            wait_fds = [self.notify_server] if self.notify_server else []
            ready, _, _ = _select.select(wait_fds, [], [], min(remaining, 1.0))
            if ready:
                self._check_notify_server()
                return True
        return False

    def _push_loop(self) -> None:
        """Push state to MQTT when notified or periodically.

        Drains backlog in a loop with a 10s time budget per wake cycle.
        Re-queues push_event if budget exhausted with events remaining.
        """
        from .relay import is_relay_enabled, push, _mark_as_relay_worker

        _mark_as_relay_worker()
        _retry_backoff = 10.0  # seconds between reconnect attempts
        while not self.shutdown_event.is_set():
            if not is_relay_enabled():
                self.push_event.clear()
                self.shutdown_event.wait(1.0)
                continue
            if not self.mqtt_client:
                # Initial connect failed or client lost — retry
                _log_info("relay.push_loop", f"connecting (backoff={_retry_backoff:.0f}s)")
                self._connect_mqtt()
                if not self.mqtt_client:
                    self.shutdown_event.wait(_retry_backoff)
                    _retry_backoff = min(_retry_backoff * 2, 120.0)
                    continue
                _retry_backoff = 10.0  # reset on success
                self.push_event.set()  # Drain backlog on fresh connect
            self._check_notify_server()
            # Wait for notify socket or periodic heartbeat (30s fallback)
            if not self.push_event.is_set():
                self._wait_for_notify_or_timeout(30.0)
            self.push_event.clear()
            # Detect stale connection (e.g. after system sleep) — paho may not
            # fire on_disconnect if the socket died silently.  Tear down so the
            # next iteration rebuilds from scratch.
            if self.mqtt_client and not self.mqtt_client.is_connected():
                _log_warn("relay.push_loop", "is_connected()=False — forcing reconnect")
                try:
                    self.mqtt_client.loop_stop()
                    self.mqtt_client.disconnect()
                except Exception:
                    pass
                self.mqtt_client = None
                continue
            # Drain: push batches until empty, error, shutdown, or 10s budget
            deadline = time.time() + 10.0
            while not self.shutdown_event.is_set():
                try:
                    success, err, has_more = push(mqtt_client=self.mqtt_client)
                    if not success:
                        # Connection error — paho's is_connected() can lag behind
                        # reality (e.g. after system sleep where TCP socket died
                        # silently).  Force teardown so next iteration reconnects.
                        if err and ("not connected" in err.lower() or "not currently" in err.lower()):
                            _log_warn("relay.push_loop", f"publish failed ({err}) — forcing reconnect")
                            try:
                                self.mqtt_client.loop_stop()
                                self.mqtt_client.disconnect()
                            except Exception:
                                pass
                            self.mqtt_client = None
                        break
                    if not has_more:
                        break
                    if time.time() > deadline:
                        self.push_event.set()  # Re-queue — more to drain
                        break
                except (OSError, sqlite3.Error) as e:
                    _log_error("relay.push_loop", e, "Error in relay push loop")
                    break


class DaemonHandler(socketserver.StreamRequestHandler):
    """Handle single client request."""

    # Timeout for reading request (prevents malicious clients from blocking indefinitely)
    timeout = 60  # seconds

    def handle(self) -> None:
        global _last_request_time, _active_requests
        # Set timestamp before shutdown check — timer sees fresh time even if
        # request is preempted between here and the lock acquisition below.
        _last_request_time = time.time()

        # Reject new requests during shutdown drain
        if _shutdown_event and _shutdown_event.is_set():
            self._send_response(1, "", "Daemon shutting down")
            return

        with _active_lock:
            _active_requests += 1

        request_id = ""
        try:
            # Set socket timeout for this connection
            self.connection.settimeout(self.timeout)

            # Enforce 16MB limit (readline stops at newline OR size limit)
            line = self.rfile.readline(MAX_REQUEST_SIZE + 1)
            if not line:
                return
            if len(line) > MAX_REQUEST_SIZE:
                self._send_response(1, "", "Request exceeds 16MB limit")
                return

            try:
                req = json.loads(line)
            except json.JSONDecodeError as e:
                self._send_response(1, "", f"Invalid JSON: {e}")
                return

            request_id = req.get("request_id") or str(uuid.uuid4())
            kind = req.get("kind", "unknown")
            hook_type = req.get("hook_type", "")
            cmd = req.get("argv", [""])[0] if req.get("argv") else ""
            instance = req.get("env", {}).get("HCOM_INSTANCE_NAME", "")

            _log_info(
                "request.start",
                request_id=request_id,
                kind=kind,
                hook_type=hook_type,
                command=cmd,
                instance=instance,
            )

            # Version check
            if req.get("version", 1) != PROTOCOL_VERSION:
                _log_warn(
                    "request.version_mismatch",
                    request_id=request_id,
                    got=req.get("version"),
                    expected=PROTOCOL_VERSION,
                )

            start_time = time.perf_counter()
            result = dispatch_request(req, request_id)
            duration_ms = (time.perf_counter() - start_time) * 1000
            result["request_id"] = request_id

            self.wfile.write(json.dumps(result).encode() + b"\n")
            self.wfile.flush()
            _log_info(
                "request.done",
                request_id=request_id,
                exit_code=result.get("exit_code"),
                duration_ms=round(duration_ms, 1),
            )

        except Exception as e:
            _log_error("request.error", e, request_id=request_id or "unknown")
            try:
                self._send_response(1, "", str(e), request_id)
            except Exception:
                pass
        finally:
            with _active_lock:
                _active_requests -= 1

    def _send_response(
        self,
        exit_code: int,
        stdout: str,
        stderr: str,
        request_id: str | None = None,
    ) -> None:
        """Send JSON response to client."""
        result = {
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
            "request_id": request_id,
        }
        self.wfile.write(json.dumps(result).encode() + b"\n")
        self.wfile.flush()


class ThreadingUnixServer(socketserver.ThreadingUnixStreamServer):
    """Unix socket server with threading and clean shutdown."""

    allow_reuse_address = True
    daemon_threads = False  # Wait for in-flight handler threads on shutdown


def dispatch_request(req: dict, request_id: str) -> dict:
    """Route request to handler using typed context objects."""
    from .core.hcom_context import HcomContext
    from .core.hook_payload import HookPayload
    from .core.config import reload_config
    from .core.paths import clear_path_cache

    # Re-read config and paths each request so config set takes effect without daemon restart
    clear_path_cache()
    reload_config()

    # Build typed context from request
    env = req.get("env", {})
    cwd = req.get("cwd", "/")
    stdin_is_tty = req.get("stdin_is_tty", True)
    stdout_is_tty = req.get("stdout_is_tty", True)
    ctx = HcomContext.from_env(env, cwd, stdin_is_tty=stdin_is_tty, stdout_is_tty=stdout_is_tty)

    if req["kind"] == "hook":
        hook_type = req["hook_type"]

        # Build typed payload based on tool
        try:
            if hook_type == "codex-notify":
                # Rust client sends argv = ["codex-notify", "<payload>"] (no leading 'hcom')
                argv = req.get("argv", ["codex-notify", "{}"])
                if not isinstance(argv, list):
                    return {"exit_code": 1, "stdout": "", "stderr": "Invalid argv: expected list"}
                raw_payload = json.loads(argv[1]) if len(argv) > 1 else {}
                payload = HookPayload.from_codex(raw_payload, hook_type)
            elif hook_type.startswith("gemini-"):
                raw_payload = json.loads(req.get("stdin") or "{}")
                payload = HookPayload.from_gemini(raw_payload, hook_type)
            else:
                raw_payload = json.loads(req.get("stdin") or "{}")
                payload = HookPayload.from_claude(raw_payload, hook_type)
        except json.JSONDecodeError as e:
            return {"exit_code": 1, "stdout": "", "stderr": f"Invalid payload JSON: {e}"}

        result = handle_hook_request(hook_type, ctx, payload, request_id)
        return result_to_dict(result)

    elif req["kind"] == "cli":
        argv = req.get("argv", [])
        stdin_content = req.get("stdin")  # May be None or string
        stdin_is_tty = req.get("stdin_is_tty", False)
        stdout_is_tty = req.get("stdout_is_tty", False)
        result = handle_cli_request(argv, ctx, request_id, stdin_content, stdin_is_tty, stdout_is_tty)
        return result_to_dict(result)

    return {"exit_code": 1, "stdout": "", "stderr": "unknown request kind"}


def result_to_dict(result: "HookResult") -> dict:
    """Convert HookResult to response dict."""
    return {
        "exit_code": result.exit_code,
        "stdout": result.stdout,
        "stderr": result.stderr,
    }


def handle_hook_request(
    hook_type: str,
    ctx: "HcomContext",
    payload: "HookPayload",
    request_id: str,
) -> "HookResult":
    """Handle hook request with typed context objects.

    All tool detection uses contextvars populated from ctx, not os.environ.

    NOTE: No stdout/stderr redirect - hooks MUST return output via HookResult.
    Using redirect_stdout is NOT thread-safe (corrupts concurrent requests).
    Any print() calls in hooks are bugs that corrupt JSON output.
    """
    from .core.thread_context import with_context
    from .core.hook_result import HookResult

    try:
        # Set contextvars for thread-safe accessors (get_process_id, get_cwd, etc.)
        with with_context(ctx):
            if hook_type == "codex-notify":
                from .tools.codex.hooks import handle_codex_hook_with_context

                result = handle_codex_hook_with_context(hook_type, ctx, payload)
            elif hook_type.startswith("gemini-"):
                from .tools.gemini.hooks import handle_gemini_hook_with_context

                result = handle_gemini_hook_with_context(hook_type, ctx, payload)
            else:
                from .tools.claude.dispatcher import handle_hook_with_context

                result = handle_hook_with_context(hook_type, ctx, payload)

            return result

    except SystemExit as e:
        exit_code = e.code if isinstance(e.code, int) else (1 if e.code else 0)
        _log_info("hook.systemexit", request_id=request_id, exit_code=exit_code)
        return HookResult(exit_code, "", "")

    except Exception as e:
        _log_error("hook.error", e, request_id=request_id)
        return HookResult.error(traceback.format_exc())


def handle_cli_request(
    argv: list[str],
    ctx: "HcomContext",
    request_id: str,
    stdin_content: str | None,
    stdin_is_tty: bool = False,
    stdout_is_tty: bool = False,
) -> "HookResult":
    """Handle CLI command with typed context.

    Uses thread-local streams for output capture - no global lock needed.
    Each concurrent request gets isolated stdout/stderr capture.
    """
    from .cli import main_with_context
    from .core.hook_result import HookResult

    # Snapshot relay config before relay commands so we can detect changes
    relay_config_before = None
    is_relay_cmd = len(argv) >= 1 and argv[0] == "relay"
    if is_relay_cmd and _relay_manager:
        try:
            from .core.config import get_config
            cfg = get_config()
            relay_config_before = (cfg.relay_id, cfg.relay, cfg.relay_enabled)
        except Exception:
            pass

    # Set up thread-local capture buffers (stdin, stdout, stderr)
    stdout_capture = CaptureBuffer(is_tty=stdout_is_tty)
    stderr_capture = CaptureBuffer(is_tty=False)
    _thread_streams.stdout = stdout_capture
    _thread_streams.stderr = stderr_capture
    _thread_streams.stdin = MockStdin(stdin_content or "", stdin_is_tty)

    try:
        result = main_with_context(argv, ctx)
        # Hot-reload relay if config changed (e.g. after relay new/connect/off)
        if relay_config_before is not None and _relay_manager:
            try:
                from .core.config import get_config
                cfg = get_config()
                relay_config_after = (cfg.relay_id, cfg.relay, cfg.relay_enabled)
                if relay_config_after != relay_config_before:
                    _relay_manager.reconnect()
            except Exception:
                pass
        # Build HookResult from thread-local captured output
        return HookResult(
            exit_code=result.exit_code,
            stdout=stdout_capture.getvalue(),
            stderr=stderr_capture.getvalue(),
        )
    except Exception as e:
        _log_error("cli.error", e, request_id=request_id)
        return HookResult.error(traceback.format_exc())
    finally:
        _thread_streams.stdout = None
        _thread_streams.stderr = None
        _thread_streams.stdin = None


def idle_shutdown_timer() -> None:
    """Shutdown daemon after IDLE_TIMEOUT seconds of inactivity."""
    global _last_request_time, _shutdown_event, _server
    while _shutdown_event and not _shutdown_event.is_set():
        time.sleep(60)
        try:
            from .relay import is_relay_enabled

            if is_relay_enabled():
                continue
        except Exception:
            pass
        # Don't shut down while requests are in-flight or recently active
        should_shutdown = False
        with _active_lock:
            if _active_requests > 0:
                continue
            if _last_request_time is not None:
                elapsed = time.time() - _last_request_time
                if elapsed <= IDLE_TIMEOUT:
                    continue
            _log_info("daemon.idle_timeout", idle_seconds=IDLE_TIMEOUT)
            _shutdown_event.set()
            should_shutdown = True
        if should_shutdown and _server:
            _server.shutdown()
            break


def setup_signal_handlers() -> None:
    """Setup graceful shutdown on SIGTERM/SIGINT."""

    def handle_signal(signum: int, _frame) -> None:
        _log_info("daemon.signal", signal=signum)
        if _shutdown_event:
            _shutdown_event.set()
        if _server:
            def _shutdown_with_drain():
                # Wait up to 10s for in-flight requests to finish
                deadline = time.time() + 10
                while time.time() < deadline:
                    with _active_lock:
                        if _active_requests == 0:
                            break
                    time.sleep(0.1)
                _server.shutdown()
                # Wait for main thread's finally block to finish cleanup (socket/pid/version).
                # Hard exit after that in case non-daemon handler threads prevent process exit.
                _cleanup_done.wait(timeout=5)
                os._exit(0)
            threading.Thread(target=_shutdown_with_drain, daemon=True).start()

    signal.signal(signal.SIGTERM, handle_signal)
    signal.signal(signal.SIGINT, handle_signal)


def _acquire_pidfile_lock(pid_path: Path, socket_path: Path):
    """Acquire exclusive flock on pidfile. Kills zombie daemons (lock held, no socket).

    Returns the open file descriptor (caller must keep it alive), or None on failure.
    """
    def _try_lock():
        fd = open(pid_path, "a+")
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fd.seek(0)
        fd.truncate()
        fd.write(str(os.getpid()))
        fd.flush()
        return fd

    try:
        return _try_lock()
    except BlockingIOError:
        pass
    except OSError as e:
        _log_error("daemon.lock_failed", e)
        return None

    # Lock held by another process. If socket exists, it's a healthy daemon.
    if socket_path.exists():
        _log_error("daemon.lock_failed", "Daemon already running (PID file locked)")
        return None

    # No socket — but is the incumbent still starting up, or is it a zombie?
    # Normal startup creates the socket within ~2s. Give a 5s grace period.
    import time
    try:
        pid_age = time.time() - pid_path.stat().st_mtime
    except OSError:
        pid_age = float("inf")

    if pid_age < 5:
        # Incumbent is likely still starting. Don't kill it.
        _log_info("daemon.lock_deferred", pid_age=f"{pid_age:.1f}s")
        return None

    # PID file is old and no socket — zombie. Kill it and retry once.
    try:
        stale_pid = int(pid_path.read_text().strip())
        _log_info("daemon.kill_zombie", pid=stale_pid)
        os.kill(stale_pid, signal.SIGKILL)
    except (ValueError, OSError):
        pass
    time.sleep(0.2)

    try:
        return _try_lock()
    except OSError as e:
        _log_error("daemon.lock_failed", f"Zombie recovery failed: {e}")
        return None


def run_daemon() -> None:
    """Main daemon entry point."""
    global _shutdown_event, _server, _relay_manager

    # Clean inherited per-request env vars from daemon process.
    # The daemon may be spawned from an hcom-launched session (e.g., luba's PTY wrapper),
    # inheriting identity/tool vars. Without cleanup, thread_context accessors fall back
    # to os.environ when a request's context var is None, returning the DAEMON's stale
    # identity instead of the client's. This causes bare-terminal "hcom start" to resolve
    # to the wrong instance.
    from .shared import HCOM_IDENTITY_VARS, TOOL_MARKER_VARS

    for var in HCOM_IDENTITY_VARS + TOOL_MARKER_VARS:
        os.environ.pop(var, None)
    # Also clean vars checked by accessors/commands but not in the above tuples
    os.environ.pop("CLAUDE_ENV_FILE", None)
    os.environ.pop("HCOM_GO", None)
    os.environ.pop("HCOM_CLAUDE_UNIX_SESSION_ID", None)

    _shutdown_event = threading.Event()

    socket_path = get_socket_path()
    pid_path = get_pid_path()

    # Ensure directories exist
    socket_path.parent.mkdir(parents=True, exist_ok=True)

    # Acquire exclusive lock on PID file for singleton enforcement.
    # Uses "a+" so all openers get the same inode (flock is per-inode).
    lock_fd = _acquire_pidfile_lock(pid_path, socket_path)
    if lock_fd is None:
        return

    # Install thread-local stream wrappers for concurrent output capture
    install_thread_local_streams()

    # Clean up stale socket
    socket_path.unlink(missing_ok=True)

    # Write version file for client version checking (atomically to prevent partial reads)
    # Allows Rust client to detect version mismatch and auto-restart daemon
    version_path = get_version_path()
    from .shared import __version__
    from .core.paths import atomic_write
    atomic_write(version_path, __version__)

    setup_signal_handlers()

    _log_info("daemon.start", socket=str(socket_path), pid=os.getpid(), version=__version__)

    _server = ThreadingUnixServer(str(socket_path), DaemonHandler)
    socket_path.chmod(0o600)

    # Start idle timeout checker
    idle_thread = threading.Thread(target=idle_shutdown_timer, daemon=True)
    idle_thread.start()
    _relay_manager = RelayManager(_shutdown_event)
    _relay_manager.start()

    try:
        _server.serve_forever()
    finally:
        _log_info("daemon.shutdown")
        if _relay_manager:
            _relay_manager.stop()
            _relay_manager = None
        # Delete socket FIRST to stop new connections
        socket_path.unlink(missing_ok=True)
        _server.server_close()
        # Release PID file lock (don't unlink — keep same inode for flock consistency)
        if lock_fd:
            lock_fd.close()
        version_path.unlink(missing_ok=True)
        _log_info("daemon.stop")
        _cleanup_done.set()


def main() -> None:
    """CLI entry point for daemon."""
    run_daemon()


if __name__ == "__main__":
    main()
