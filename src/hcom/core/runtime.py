"""Runtime utilities for environment building and TCP notifications.

This module provides shared infrastructure used by both hooks and CLI commands:

Environment Building
--------------------
build_claude_env() merges HCOM settings (from config.toml via HcomConfig) with
passthrough env vars (from env file). The caller layers the current shell
environment on top, so env vars > config.toml > defaults.

TCP Notifications
-----------------
notify_instance() and notify_all_instances() implement the instant message wake
system. When a message is sent, the sender pings all listening instances via TCP
so they don't have to wait for their polling interval.

Architecture:
1. Listeners register ports in the notify_endpoints table
2. Senders call notify_*() after logging messages
3. TCP ping wakes blocked listeners instantly
4. Polling provides fallback if TCP fails

NOTE: bootstrap/launch context text is re-exported from bootstrap.py for
backward compatibility. That content is injected into Claude's (any agent - claude specific info here is not helpful) context via
hooks - the human user never sees it directly.
"""

from __future__ import annotations
import socket
import sqlite3

from .paths import hcom_path, ENV_FILE
from .instances import load_instance_position

from .bootstrap import get_bootstrap  # noqa: F401


def build_claude_env() -> dict[str, str]:
    """Build environment dict for launched agents.

    Merges two sources:
    - HCOM_* settings from config.toml (via get_config → hcom_config_to_dict)
    - Passthrough env vars from env file (ANTHROPIC_MODEL, etc.)

    The caller (typically launch_terminal) layers the current shell environment
    on top, so env vars > config.toml/env > defaults.

    Returns:
        Dict of environment variable names to string values.
        Blank values are skipped.
    """
    from .config import get_config, hcom_config_to_dict, load_env_extras

    env: dict[str, str] = {}

    # 1. HCOM_* settings from config.toml
    config = get_config()
    for key, value in hcom_config_to_dict(config).items():
        if value:
            env[key] = value

    # 2. Passthrough vars from env file
    env_path = hcom_path(ENV_FILE)
    for key, value in load_env_extras(env_path).items():
        if value:
            env[key] = value

    return env


def create_notify_server() -> tuple[socket.socket | None, int | None]:
    """Create TCP notify server for instant wake on messages.

    Used by listen loops to receive instant notifications when messages arrive,
    avoiding polling delays.

    Returns:
        (server, port): Server socket and port, or (None, None) on failure.
    """
    try:
        server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        server.bind(("127.0.0.1", 0))
        server.listen(128)
        server.setblocking(False)
        return server, server.getsockname()[1]
    except OSError:
        return None, None


def notify_instance(instance_name: str, timeout: float = 0.05) -> None:
    """Send TCP notification to wake a specific instance.

    Looks up all registered notify ports for the instance and sends a single
    newline byte to each. This wakes any blocked listeners immediately instead
    of waiting for their polling interval.

    Dead ports (connection refused) are automatically pruned from the table.

    Args:
        instance_name: Target instance to notify (e.g., 'luna')
        timeout: TCP connection timeout in seconds (default 50ms)
    """
    instance_data = load_instance_position(instance_name)
    if not instance_data:
        return

    ports: list[int] = []
    try:
        from .db import list_notify_ports

        ports.extend(list_notify_ports(instance_name))
    except sqlite3.Error:
        pass

    if not ports:
        return

    # Dedup while preserving order
    seen = set()
    deduped: list[int] = []
    for p in ports:
        if p and p not in seen:
            deduped.append(p)
            seen.add(p)

    from .db import delete_notify_endpoint

    for port in deduped:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=timeout) as sock:
                sock.send(b"\n")
        except OSError:
            # Best effort prune: if a port is dead, remove from notify_endpoints.
            try:
                delete_notify_endpoint(instance_name, port=port)
            except sqlite3.Error:
                pass


def _send_notify_to_ports(ports: list[int], timeout: float = 0.05) -> None:
    """Send TCP notifications to specific ports (no DB lookup).

    Used by stop_instance after row is deleted but before notifying listeners.

    Args:
        ports: List of TCP ports to notify
        timeout: TCP connection timeout in seconds (default 50ms)
    """
    # Dedup while preserving order
    seen = set()
    deduped: list[int] = []
    for p in ports:
        if p and p not in seen:
            deduped.append(p)
            seen.add(p)

    for port in deduped:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=timeout) as sock:
                sock.send(b"\n")
        except OSError:
            pass  # Best effort - no pruning since row is already deleted


def notify_all_instances(timeout: float = 0.05) -> None:
    """Send TCP wake notifications to all instance notify ports.

    Best effort - connection failures ignored. Polling fallback ensures
    message delivery even if all notifications fail.

    Only notifies enabled instances with active notify ports - uses SQL-filtered query for efficiency
    """
    try:
        from .db import get_db, delete_notify_endpoint

        conn = get_db()

        # Prefer notify_endpoints (supports multiple concurrent listeners per instance).
        # Row exists = participating (no enabled filter needed)
        rows = conn.execute(
            """
            SELECT ne.instance AS name, ne.port AS port
            FROM notify_endpoints ne
            JOIN instances i ON i.name = ne.instance
            WHERE ne.port > 0
            """
        ).fetchall()

        # Dedup (name, port)
        seen: set[tuple[str, int]] = set()
        targets: list[tuple[str, int]] = []
        for row in rows:
            try:
                k = (row["name"], int(row["port"]))
            except (ValueError, TypeError, KeyError):
                continue
            if k in seen:
                continue
            seen.add(k)
            targets.append(k)

        for name, port in targets:
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=timeout) as sock:
                    sock.send(b"\n")
            except OSError:
                # Best-effort prune for notify_endpoints rows.
                try:
                    delete_notify_endpoint(name, port=port)
                except sqlite3.Error:
                    pass

    except (sqlite3.Error, OSError):
        return
