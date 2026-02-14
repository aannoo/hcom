"""Relay commands for HCOM — cross-device sync via MQTT pub/sub."""

import base64
import sys
import time
import uuid
from ..shared import format_age, CommandContext


def cmd_relay(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Relay management: hcom relay [new|connect|disconnect|status]

    Usage:
        hcom relay                  Show relay status and ping broker
        hcom relay status           Same as above
        hcom relay new              Create new relay group
        hcom relay connect          Re-enable existing relay
        hcom relay connect <token>  Join relay from another device
        hcom relay off              Disable relay sync (alias: disconnect)
        hcom relay disconnect       Disable relay sync

    Private broker:
        hcom relay new --broker mqtts://host:port [--password secret]
        hcom relay connect <token> --broker mqtts://host:port [--password secret]

    Switching groups just overwrites — previous token is printed so you can switch back.

    Note: --name flag is not used by relay command (no identity needed).
    """
    from .utils import parse_name_flag

    # Relay doesn't use identity; direct calls may still pass --name.
    if ctx is None:
        _, argv = parse_name_flag(argv)

    if not argv:
        return _relay_status()
    elif argv[0] == "new":
        return _relay_new(argv[1:])
    elif argv[0] == "connect":
        return _relay_connect(argv[1:])
    elif argv[0] in ("off", "disconnect"):
        return _relay_toggle(False)
    elif argv[0] in ("on", "status"):
        return _relay_connect([]) if argv[0] == "on" else _relay_status()
    else:
        # Could be a token passed directly: hcom relay <token>
        # Tokens are base64url, typically 60+ chars, no dashes at start
        if len(argv[0]) > 20 and not argv[0].startswith("-"):
            return _relay_connect(argv)

        from .utils import get_command_help

        print(f"Unknown subcommand: {argv[0]}\n", file=sys.stderr)
        print(get_command_help("relay"), file=sys.stderr)
        return 1


def _relay_toggle(enable: bool) -> int:
    """Enable or disable relay sync."""
    from ..core.config import (
        load_config_snapshot,
        save_config_snapshot,
        get_config,
        reload_config,
    )

    config = get_config()

    # Check if relay_id is configured
    if not config.relay_id:
        print("No relay configured.", file=sys.stderr)
        print("Run: hcom relay new", file=sys.stderr)
        return 1

    # Clear retained MQTT state before disabling so remote devices stop seeing us
    if not enable and config.relay_enabled:
        from ..relay import clear_retained_state
        if clear_retained_state():
            print("Cleared remote state")

    # Update config
    snapshot = load_config_snapshot()
    snapshot.core.relay_enabled = enable
    save_config_snapshot(snapshot)
    reload_config()

    if enable:
        print("Relay enabled\n")
        return _relay_status()
    else:
        print("Relay: disabled")
        print("\nRun 'hcom relay connect' to reconnect")

    return 0


def _relay_status() -> int:
    """Show relay status and configuration."""
    from ..core.device import get_device_short_id
    from ..core.config import get_config
    from ..core.db import kv_get, get_db

    config = get_config()

    if not config.relay_id:
        print("Relay: not configured")
        print("Run: hcom relay new")
        return 0

    if not config.relay_enabled:
        print("Relay: disabled")
        print("\nRun: hcom relay connect")
        return 0

    # Show MQTT connection state from kv store
    relay_status = kv_get("relay_status")
    relay_error = kv_get("relay_last_error")

    if relay_status == "ok":
        print("Status: connected")
    elif relay_status == "error":
        print(f"Status: error — {relay_error or 'unknown'}")
        # Hint for auth errors on private brokers
        if relay_error and ("password" in relay_error or "auth" in relay_error or "not authorized" in relay_error):
            from ..relay import DEFAULT_BROKERS
            is_public = any(
                config.relay in (f"mqtts://{h}:{p}", f"mqtt://{h}:{p}")
                for h, p in DEFAULT_BROKERS
            )
            if not is_public and not config.relay_token:
                print("  Hint: use --password when connecting to private brokers")
    else:
        print("Status: waiting (daemon may not be running)")

    # Live broker ping
    if config.relay:
        import urllib.parse
        parsed = urllib.parse.urlparse(config.relay)
        host = parsed.hostname or config.relay
        port = parsed.port or (8883 if parsed.scheme == "mqtts" else 1883)
        ping_ms = _ping_broker(host, port)
        if ping_ms is not None:
            print(f"Broker: {config.relay} ({ping_ms}ms)")
        else:
            print(f"Broker: {config.relay} (unreachable)")
    else:
        print("Broker: auto (public fallback)")
    print(f"Device ID: {get_device_short_id()}")

    # Queued events (local only — remote events have : in instance name)
    conn = get_db()
    last_push_id = int(kv_get("relay_last_push_id") or 0)
    queued = conn.execute(
        "SELECT COUNT(*) FROM events WHERE id > ? AND instance NOT LIKE '%:%'",
        (last_push_id,),
    ).fetchone()[0]
    print(f"Queued: {queued} events pending" if queued > 0 else "Queued: up to date")

    # Last push
    last_push = float(kv_get("relay_last_push") or 0)
    print(f"Last push: {_format_time(last_push)}" if last_push else "Last push: never")

    # Remote devices — derived from KV (works even with zero agents on remote)
    from ..relay import get_remote_devices

    remote_devices = get_remote_devices()
    # Count agents per device
    agent_rows = conn.execute(
        "SELECT origin_device_id, COUNT(*) as cnt FROM instances "
        "WHERE origin_device_id IS NOT NULL AND origin_device_id != '' "
        "GROUP BY origin_device_id"
    ).fetchall()
    agent_counts = {row["origin_device_id"]: row["cnt"] for row in agent_rows}

    if remote_devices:
        remote_parts = []
        for device_id, info in sorted(remote_devices.items(), key=lambda x: x[1]["short_id"]):
            short_id = info["short_id"]
            sync_ts = info["sync_time"]
            agents = agent_counts.get(device_id, 0)
            parts = [short_id]
            if sync_ts:
                parts.append(_format_time(sync_ts))
            if agents == 0:
                parts.append("no agents")
            remote_parts.append(f"{parts[0]} ({', '.join(parts[1:])})" if len(parts) > 1 else parts[0])
        print(f"\nRemote devices: {', '.join(remote_parts)}")
    else:
        print("\nNo other devices")

    # Show token for adding more devices
    if config.relay and config.relay_id:
        token = _encode_join_token(config.relay_id, config.relay)
        print(f"\nAdd devices: hcom relay connect {token}")

    return 0


def _format_time(timestamp: float) -> str:
    """Format timestamp for display."""
    if not timestamp:
        return "never"
    return f"{format_age(time.time() - timestamp)} ago"


def _parse_broker_flags(argv: list[str]) -> tuple[str | None, str | None, list[str]]:
    """Parse --broker, --password from argv.

    Returns (broker_url, auth_token, remaining_argv).
    """
    broker = None
    auth_token = None
    remaining = []
    i = 0
    while i < len(argv):
        if argv[i] == "--broker" and i + 1 < len(argv):
            broker = argv[i + 1]
            i += 2
        elif argv[i] == "--password" and i + 1 < len(argv):
            auth_token = argv[i + 1]
            i += 2
        else:
            remaining.append(argv[i])
            i += 1
    return broker, auth_token, remaining


def _ping_broker(host: str, port: int) -> int | None:
    """TLS connect to broker, return round-trip ms or None on failure."""
    import ssl
    import socket
    try:
        t0 = time.time()
        ctx = ssl.create_default_context()
        sock = socket.create_connection((host, port), timeout=5)
        sock = ctx.wrap_socket(sock, server_hostname=host)
        sock.close()
        return int((time.time() - t0) * 1000)
    except Exception:
        return None


def _test_brokers_parallel(brokers: list[tuple[str, int]]) -> list[tuple[str, int, int | None]]:
    """Test all brokers in parallel. Returns [(host, port, ping_ms|None)] in input order."""
    import concurrent.futures
    results: list[tuple[str, int, int | None]] = [(h, p, None) for h, p in brokers]

    def _test(idx: int, host: str, port: int) -> tuple[int, int | None]:
        return idx, _ping_broker(host, port)

    with concurrent.futures.ThreadPoolExecutor(max_workers=len(brokers)) as pool:
        futures = [pool.submit(_test, i, h, p) for i, (h, p) in enumerate(brokers)]
        for f in concurrent.futures.as_completed(futures):
            idx, ping_ms = f.result()
            results[idx] = (brokers[idx][0], brokers[idx][1], ping_ms)

    return results


def _relay_new(argv: list[str]) -> int:
    """Create a new relay group — generate relay_id, find/use broker, enable.

    Usage:
        hcom relay new                                    Public broker
        hcom relay new --broker mqtts://host:port         Private broker
        hcom relay new --broker mqtts://host:port --password secret
    """
    from ..core.config import load_config_snapshot, save_config_snapshot, reload_config
    from ..relay import DEFAULT_BROKERS

    broker_url, auth_token, _ = _parse_broker_flags(argv)

    snapshot = load_config_snapshot()

    # Show previous group FIRST so user can copy it before it's replaced
    if snapshot.core.relay_id and snapshot.core.relay:
        old_token = _encode_join_token(snapshot.core.relay_id, snapshot.core.relay)
        print(f"Current group: hcom relay connect {old_token}\n")

    # Generate relay_id
    relay_id = str(uuid.uuid4())

    if broker_url:
        # Private broker — use as-is, test connectivity
        import urllib.parse
        parsed = urllib.parse.urlparse(broker_url)
        host = parsed.hostname or broker_url
        port = parsed.port or (8883 if parsed.scheme == "mqtts" else 1883)

        print(f"Testing {host}:{port}...")
        ping_ms = _ping_broker(host, port)
        if ping_ms is None:
            print(f"  {host}:{port} — failed", file=sys.stderr)
            print("\nBroker unreachable. Check host, port, and network.", file=sys.stderr)
            return 1
        print(f"  {host}:{port} — {ping_ms}ms")
        pinned_broker = broker_url
    else:
        # Public broker — test all in parallel, pick fastest
        print("Testing brokers...")
        pinned_broker = None
        results = _test_brokers_parallel(DEFAULT_BROKERS)
        for host, port, ping_ms in results:
            if ping_ms is not None:
                print(f"  {host}:{port} — {ping_ms}ms")
                if pinned_broker is None:
                    pinned_broker = f"mqtts://{host}:{port}"
            else:
                print(f"  {host}:{port} — failed")

        if not pinned_broker:
            print("\nNo broker reachable. Check your network.", file=sys.stderr)
            print("Or use a private broker: hcom relay new --broker mqtts://host:port", file=sys.stderr)
            return 1

    # Save config (pinned_broker is guaranteed non-None — we return 1 above if not)
    assert pinned_broker is not None
    snapshot.core.relay_id = relay_id
    snapshot.core.relay = pinned_broker
    snapshot.core.relay_enabled = True
    if auth_token is not None:
        snapshot.core.relay_token = auth_token
    save_config_snapshot(snapshot)
    reload_config()

    # Generate join token
    token = _encode_join_token(relay_id, pinned_broker)

    print(f"\nBroker: {pinned_broker}")
    if auth_token:
        print("Password: set")
    print(f"\nOn other devices: hcom relay connect {token}")
    if auth_token:
        print("  (they will also need: --password <secret>)")

    from ..relay import is_relay_handled_by_daemon
    if is_relay_handled_by_daemon():
        print("\nConnected.")
    else:
        print("\nStart daemon to connect: hcom daemon start")
    return 0


def _relay_connect(argv: list[str]) -> int:
    """Connect to relay — re-enable existing config or join with token.

    Usage:
        hcom relay connect                               Re-enable existing
        hcom relay connect <token>                       Join relay group
        hcom relay connect <token> --broker ...          Override broker from token
    """
    from ..core.config import load_config_snapshot, save_config_snapshot, get_config, reload_config

    broker_url, auth_token, remaining = _parse_broker_flags(argv)

    # No token = re-enable existing config
    token_str = remaining[0] if remaining and not remaining[0].startswith("-") else None

    if not token_str:
        # Re-enable mode
        config = get_config()
        if not config.relay_id:
            print("No relay configured.", file=sys.stderr)
            print("Run: hcom relay new", file=sys.stderr)
            return 1

        if config.relay_enabled:
            print("Relay already enabled.\n")
            return _relay_status()

        snapshot = load_config_snapshot()
        snapshot.core.relay_enabled = True
        save_config_snapshot(snapshot)
        reload_config()
        print("Relay enabled\n")
        return _relay_status()

    # Token mode — join existing relay
    result = _decode_join_token(token_str)
    if result is None:
        print("Invalid token.", file=sys.stderr)
        return 1

    relay_id, token_broker = result

    # --broker overrides broker from token
    effective_broker = broker_url or token_broker

    # Test broker connectivity (non-blocking warning)
    import urllib.parse
    parsed = urllib.parse.urlparse(effective_broker)
    host = parsed.hostname or effective_broker
    port = parsed.port or (8883 if parsed.scheme == "mqtts" else 1883)
    ping_ms = _ping_broker(host, port)

    snapshot = load_config_snapshot()

    # Show previous group FIRST if switching
    if snapshot.core.relay_id and snapshot.core.relay and snapshot.core.relay_id != relay_id:
        old_token = _encode_join_token(snapshot.core.relay_id, snapshot.core.relay)
        print(f"Current group: hcom relay connect {old_token}\n")

    snapshot.core.relay_id = relay_id
    snapshot.core.relay = effective_broker
    snapshot.core.relay_enabled = True
    if auth_token is not None:
        snapshot.core.relay_token = auth_token
    save_config_snapshot(snapshot)
    reload_config()

    if ping_ms is not None:
        print(f"Broker: {effective_broker} ({ping_ms}ms)")
    else:
        print(f"Broker: {effective_broker}")
        print("  Warning: broker unreachable — check network or token", file=sys.stderr)

    # Password feedback
    if auth_token:
        print("Password: set")
    else:
        # Warn if private broker (not in DEFAULT_BROKERS) without password
        from ..relay import DEFAULT_BROKERS
        is_public = any(
            effective_broker in (f"mqtts://{h}:{p}", f"mqtt://{h}:{p}")
            for h, p in DEFAULT_BROKERS
        )
        if not is_public:
            print("Password: not set (use --password if broker requires auth)")

    from ..relay import is_relay_handled_by_daemon
    if is_relay_handled_by_daemon():
        print("\nConnected.")
    else:
        print("\nStart daemon to connect: hcom daemon start")
    return 0


def _encode_join_token(relay_id: str, broker_url: str) -> str:
    """Encode relay_id and broker URL into a compact join token.

    Format (version-prefixed):
      0x01 + 16 UUID bytes + 1 broker index  →  18 bytes → 24 char base64url (public)
      0x02 + 16 UUID bytes + URL bytes        →  variable (private)
    """
    from ..relay import DEFAULT_BROKERS

    uuid_bytes = uuid.UUID(relay_id).bytes

    for i, (host, port) in enumerate(DEFAULT_BROKERS):
        if broker_url in (f"mqtts://{host}:{port}", f"mqtt://{host}:{port}"):
            return base64.urlsafe_b64encode(b"\x01" + uuid_bytes + bytes([i])).decode().rstrip("=")

    return base64.urlsafe_b64encode(b"\x02" + uuid_bytes + broker_url.encode()).decode().rstrip("=")


def _decode_join_token(token: str) -> tuple[str, str] | None:
    """Decode a join token back to (relay_id, broker_url)."""
    from ..relay import DEFAULT_BROKERS

    padding = 4 - len(token) % 4
    if padding != 4:
        token += "=" * padding
    try:
        raw = base64.urlsafe_b64decode(token.encode())
    except Exception:
        return None

    if len(raw) < 18:
        return None

    version = raw[0]
    try:
        relay_id = str(uuid.UUID(bytes=raw[1:17]))
    except Exception:
        return None

    if version == 0x01:
        # Public broker index
        idx = raw[17]
        if idx >= len(DEFAULT_BROKERS):
            return None
        host, port = DEFAULT_BROKERS[idx]
        return (relay_id, f"mqtts://{host}:{port}")
    elif version == 0x02:
        # Private broker URL
        try:
            broker_url = raw[17:].decode()
        except Exception:
            return None
        return (relay_id, broker_url)
    else:
        return None
