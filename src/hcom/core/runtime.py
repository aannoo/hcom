"""Runtime utilities for environment building and TCP notifications.

This module provides shared infrastructure used by both hooks and CLI commands:

Environment Building
--------------------
build_claude_env() loads config.env as environment variable defaults that get
layered under shell environment (shell vars take precedence). This enables
configuration via both file and environment.

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

from .paths import hcom_path, CONFIG_FILE
from ..shared import parse_env_file
from .instances import load_instance_position

# Re-export from bootstrap module for backward compatibility
from .bootstrap import build_hcom_bootstrap_text, get_bootstrap  # noqa: F401


def build_claude_env() -> dict[str, str]:
    """Load config.env as environment variable defaults.

    Reads ~/.hcom/config.env and returns all HCOM_* and other configured
    variables as a dict. The caller (typically launch_terminal) layers the
    current shell environment on top, so env vars > config.env > defaults.

    Returns:
        Dict of environment variable names to string values.
        Blank values in config.env are skipped.
    """
    env = {}

    # Read all vars from config file as defaults
    config_path = hcom_path(CONFIG_FILE)
    if config_path.exists():
        file_config = parse_env_file(config_path)
        for key, value in file_config.items():
            if value == "":
                continue  # Skip blank values
            env[key] = str(value)

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
    except Exception:
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
    except Exception:
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
        except Exception:
            # Best effort prune: if a port is dead, remove from notify_endpoints.
            try:
                delete_notify_endpoint(instance_name, port=port)
            except Exception:
                pass


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
            except Exception:
                continue
            if k in seen:
                continue
            seen.add(k)
            targets.append(k)

        for name, port in targets:
            try:
                with socket.create_connection(
                    ("127.0.0.1", port), timeout=timeout
                ) as sock:
                    sock.send(b"\n")
            except Exception:
                # Best-effort prune for notify_endpoints rows.
                try:
                    delete_notify_endpoint(name, port=port)
                except Exception:
                    pass

    except Exception:
        return
