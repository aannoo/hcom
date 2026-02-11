"""Relay commands for HCOM"""

import sys
import json
import time
from pathlib import Path
from .utils import format_error
from ..shared import format_age, CommandContext


REQUIRED_RELAY_VERSION = 1  # Bump when hcom needs new relay features


def cmd_relay(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Relay management: hcom relay [on|off|pull|poll|hf]

    Usage:
        hcom relay                Show relay status
        hcom relay on             Enable relay sync
        hcom relay off            Disable relay sync
        hcom relay pull           Manual sync (pull + push)
        hcom relay poll [sec]     Long-poll for changes
        hcom relay hf [token]     Setup HuggingFace Space relay
        hcom relay hf --update    Update relay to latest version

    Note: --name flag is not used by relay command (no identity needed).
    """
    from .utils import parse_name_flag

    # Relay doesn't use identity; direct calls may still pass --name.
    if ctx is None:
        _, argv = parse_name_flag(argv)

    if not argv:
        return _relay_status()
    elif argv[0] == "on":
        return _relay_toggle(True)
    elif argv[0] == "off":
        return _relay_toggle(False)
    elif argv[0] == "pull":
        return _relay_pull()
    elif argv[0] == "poll":
        return _relay_poll(argv[1:])
    elif argv[0] == "hf":
        return _relay_hf(argv[1:])
    else:
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

    # Check if relay URL is configured
    if not config.relay:
        print("No relay URL configured.", file=sys.stderr)
        print("Run: hcom relay hf <token>", file=sys.stderr)
        return 1

    # Update config
    snapshot = load_config_snapshot()
    snapshot.core.relay_enabled = enable
    save_config_snapshot(snapshot)
    reload_config()  # Invalidate cache so push/pull see new relay_enabled value

    if enable:
        print("Relay enabled\n")
        return _relay_status()
    else:
        print("Relay: disabled")
        print(f"URL still configured: {config.relay}")
        print("\nRun 'hcom relay on' to reconnect")

    return 0


def _relay_status() -> int:
    """Show relay status and configuration"""
    from ..core.device import get_device_short_id
    from ..core.config import get_config
    from ..core.db import kv_get
    from ..relay import push, pull, _http

    config = get_config()

    if not config.relay:
        print("Relay: not configured")
        print("Run: hcom relay hf <token>")
        return 0

    if not config.relay_enabled:
        print("Relay: disabled (URL configured)")
        print(f"URL: {config.relay}")
        print("\nRun: hcom relay on")
        return 0

    exit_code = 0

    # Push first (heartbeat - so this device shows as active)
    push(force=True)

    # Pull to catch up immediately
    _, pull_err = pull()
    if pull_err:
        print(f"Pull failed: {pull_err}", file=sys.stderr)
        exit_code = 1

    # Ping relay to check if online
    relay_online = False
    relay_version = None
    url = config.relay.rstrip("/") + "/version"
    status, content = _http("GET", url, timeout=5)
    if status == 200 and content:
        try:
            relay_version = json.loads(content).get("v")
            relay_online = True
        except json.JSONDecodeError:
            pass

    # Version warning first (if outdated)
    if relay_version is not None and relay_version != REQUIRED_RELAY_VERSION:
        print(f"⚠ Relay server outdated (v{relay_version}). Run: hcom relay hf --update\n")

    # Server status
    if relay_online:
        print("Status: online")
    else:
        print("Status: OFFLINE")
        print(f"URL: {config.relay}")
        print("\n⚠ Cannot reach relay server. Check URL or wait for Space to start.")
        return 1

    print(f"URL: {config.relay}")
    print(f"Device ID: {get_device_short_id()}")

    # Queued events (local only - remote events have : in instance name)
    from ..core.db import get_db

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

    # Live remote devices from server
    devices_url = config.relay.rstrip("/") + "/devices"
    dev_status, dev_content = _http("GET", devices_url, timeout=5)
    if dev_status == 200 and dev_content:
        try:
            remote_devices = json.loads(dev_content)
            my_short_id = get_device_short_id()
            active = [d for d in remote_devices if d.get("age", 9999) < 300 and d.get("short_id") != my_short_id]

            if active:
                print("\nActive devices:")
                for d in active:
                    print(f"  {d['short_id']}: {d['instances']} agents ({format_age(d['age'])} ago)")
            else:
                print("\nNo other active devices")
        except json.JSONDecodeError:
            print("\nNo other active devices")
    else:
        print("\nNo other active devices")

    return exit_code


def _format_time(timestamp: float) -> str:
    """Format timestamp for display (wrapper around format_age)"""
    if not timestamp:
        return "never"
    return f"{format_age(time.time() - timestamp)} ago"


def _check_relay_version() -> tuple[bool, str | None]:
    """Check if relay version matches required version."""
    from ..core.config import get_config
    from ..relay import _http

    config = get_config()
    if not config.relay:
        return (False, None)

    url = config.relay.rstrip("/") + "/version"
    status, content = _http("GET", url, timeout=5)
    if status == 200 and content:
        try:
            relay_version = json.loads(content).get("v", 0)
            if relay_version != REQUIRED_RELAY_VERSION:
                return (
                    True,
                    f"Relay v{relay_version}, need v{REQUIRED_RELAY_VERSION}. Run: hcom relay hf --update",
                )
        except json.JSONDecodeError:
            pass
    return (False, None)


def _relay_pull() -> int:
    """Manual sync trigger (pull + push)"""
    from ..relay import push, pull

    # Check for outdated relay
    outdated, msg = _check_relay_version()
    if outdated:
        print(f"⚠ {msg}", file=sys.stderr)

    ok, push_err = push(force=True)
    if push_err:
        print(f"Push failed: {push_err}", file=sys.stderr)

    result, pull_err = pull()
    if pull_err:
        print(f"Pull failed: {pull_err}", file=sys.stderr)
        return 1

    devices = result.get("devices", {})
    print(f"Synced with {len(devices)} remote devices")
    return 0


def _relay_poll(argv: list[str]) -> int:
    """Long-poll for changes, exit when data arrives or timeout.

    Used for manual long-polling or background workers.
    Returns 0 if new data arrived, 1 on timeout.
    """
    from ..relay import relay_wait

    timeout = 55
    if argv and argv[0].isdigit():
        timeout = int(argv[0])

    start_time = time.time()
    while time.time() - start_time < timeout:
        remaining = timeout - (time.time() - start_time)
        if remaining <= 0:
            break
        if relay_wait(min(remaining, 25)):
            return 0  # New data arrived
        time.sleep(1)  # Brief backoff
    return 1  # Timeout, no data


def _relay_hf(argv: list[str]) -> int:
    """Setup HF Space relay"""
    import urllib.request
    import urllib.error
    import os
    from ..core.config import load_config_snapshot, save_config_snapshot
    from .utils import validate_flags

    # Validate flags
    if error := validate_flags("relay", argv):
        print(format_error(error), file=sys.stderr)
        return 1

    SOURCE_SPACE = "aannoo/hcom-relay"

    def get_hf_token() -> str | None:
        """Get HF token from env or cached file."""
        if token := os.getenv("HF_TOKEN"):
            return token
        try:
            hf_home = Path(os.getenv("HF_HOME", Path.home() / ".cache" / "huggingface"))
            token_path = hf_home / "token"
            if token_path.exists():
                return token_path.read_text().strip()
        except (RuntimeError, OSError):
            pass  # Home dir inaccessible
        return None

    # Parse args: [token] [--space NAME] [--update]
    token = None
    space_name = "hcom-relay"
    do_update = False
    i = 0
    while i < len(argv):
        if argv[i] == "--space" and i + 1 < len(argv):
            space_name = argv[i + 1]
            i += 2
        elif argv[i] == "--update":
            do_update = True
            i += 1
        elif not token and not argv[i].startswith("-"):
            token = argv[i]
            i += 1
        else:
            i += 1

    if not token:
        token = get_hf_token()
    if not token:
        print("No HF token found.", file=sys.stderr)
        print("Usage: hcom relay hf <token>", file=sys.stderr)
        print("   or: huggingface-cli login", file=sys.stderr)
        return 1

    # Get username
    print("Getting HF username...")
    try:
        req = urllib.request.Request(
            "https://huggingface.co/api/whoami-v2",
            headers={"Authorization": f"Bearer {token}"},
        )
        with urllib.request.urlopen(req, timeout=10) as resp:
            data = json.loads(resp.read())
            username = data.get("name")
    except Exception as e:
        print(f"Failed to get username: {e}", file=sys.stderr)
        return 1

    target_space = f"{username}/{space_name}"
    space_url = f"https://{username}-{space_name}.hf.space/"
    created = False

    # Check if Space already exists
    space_exists = False
    try:
        req = urllib.request.Request(
            f"https://huggingface.co/api/spaces/{target_space}",
            headers={"Authorization": f"Bearer {token}"},
        )
        with urllib.request.urlopen(req, timeout=5) as resp:
            if resp.status == 200:
                space_exists = True
    except urllib.error.HTTPError as e:
        if e.code != 404:
            print(f"Check failed: {e}", file=sys.stderr)
            return 1

    # Handle --update: manual instructions (delete requires admin permission)
    if space_exists and do_update:
        print("Update manually:")
        print(f"  1. Edit: https://huggingface.co/spaces/{target_space}/edit/main/app.py")
        print(f"  2. Copy from: https://huggingface.co/spaces/{SOURCE_SPACE}/raw/main/app.py")
        return 0

    # Create Space if needed
    if space_exists:
        print(f"Space exists: {target_space}")
    else:
        print(f"Creating {target_space}...")
        try:
            req = urllib.request.Request(
                f"https://huggingface.co/api/spaces/{SOURCE_SPACE}/duplicate",
                data=json.dumps({"repository": target_space, "private": True}).encode(),
                method="POST",
                headers={
                    "Authorization": f"Bearer {token}",
                    "Content-Type": "application/json",
                },
            )
            with urllib.request.urlopen(req, timeout=10) as resp:
                if resp.status not in (200, 201):
                    print(f"Create failed: HTTP {resp.status}", file=sys.stderr)
                    return 1
                created = True
        except urllib.error.HTTPError as e2:
            print(f"Create failed: {e2}", file=sys.stderr)
            return 1

    # Update config (save token for private Space auth)
    config = load_config_snapshot()
    config.core.relay = space_url
    config.core.relay_token = token
    save_config_snapshot(config)

    # Clear version cache after update
    if created:
        from ..core.db import kv_set

        kv_set("relay_version_check", "0")
        kv_set("relay_version_outdated", "0")

    print(f"\n{space_url}")
    if created:
        print("\nSpace is building (~15 seconds). Check progress:")
        print(f"  https://huggingface.co/spaces/{target_space}")
        print("\nConfig updated. Relay will work once Space is running.")
        print("\nCheck status: hcom relay")
        print("See active agents from other devices: hcom list")
    else:
        print("\nConfig updated.")
    return 0
