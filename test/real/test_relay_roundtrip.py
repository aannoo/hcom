#!/usr/bin/env python3
"""Relay MQTT roundtrip integration test.

Two real hcom instances (separate HCOM_DIR), each with their own
daemon, talking through a real public MQTT broker.
Zero mocking, zero fake payloads.

Phases:
1. Device A: hcom relay new → daemon connects to broker
2. Device A: hcom send → event pushed to broker
3. Device B: hcom relay connect <token> → daemon connects, pulls
4. Verify: Device B sees Device A's event in hcom events (namespaced)
5. Verify: Device A sees Device B as remote device in relay status
6. Cleanup: relay off, daemon stop, remove temp dirs

Requires:
- hcom installed (editable or pip)
- paho-mqtt installed
- Network access to public MQTT brokers

Usage:
    python test/public/real/test_relay_roundtrip.py
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import uuid  # for unique test marker


# ── Helpers ──────────────────────────────────────────────────────────

_dir_a: str | None = None
_dir_b: str | None = None


def fail(msg: str):
    print(f"\n  FAIL: {msg}", file=sys.stderr)
    cleanup()
    sys.exit(1)


def ok(msg: str):
    print(f"  OK: {msg}")


def _hcom(cmd: str, hcom_dir: str, timeout: int = 15) -> subprocess.CompletedProcess:
    env = {**os.environ, "HCOM_DIR": hcom_dir}
    return subprocess.run(
        f"hcom {cmd}", shell=True, capture_output=True, text=True,
        timeout=timeout, env=env,
    )


def hcom_a(cmd: str, timeout: int = 15) -> subprocess.CompletedProcess:
    return _hcom(cmd, _dir_a, timeout)


def hcom_b(cmd: str, timeout: int = 15) -> subprocess.CompletedProcess:
    return _hcom(cmd, _dir_b, timeout)


def check_a(cmd: str, timeout: int = 15) -> str:
    r = hcom_a(cmd, timeout)
    if r.returncode != 0:
        fail(f"Device A: hcom {cmd}\nstdout: {r.stdout}\nstderr: {r.stderr}")
    return r.stdout


def check_b(cmd: str, timeout: int = 15) -> str:
    r = hcom_b(cmd, timeout)
    if r.returncode != 0:
        fail(f"Device B: hcom {cmd}\nstdout: {r.stdout}\nstderr: {r.stderr}")
    return r.stdout


def poll_until(fn, desc: str, timeout: float = 30, interval: float = 1.0):
    t0 = time.time()
    last = None
    while time.time() - t0 < timeout:
        last = fn()
        if last:
            return last
        time.sleep(interval)
    fail(f"Timeout ({timeout}s): {desc} (last: {last})")


def parse_token(output: str) -> str | None:
    for line in output.splitlines():
        if "hcom relay connect " in line:
            return line.split("hcom relay connect ")[1].strip()
    return None


def parse_device_id(status_output: str) -> str | None:
    for line in status_output.splitlines():
        if "Device ID:" in line:
            return line.split("Device ID:")[1].strip()
    return None


def read_device_uuid(hcom_dir: str) -> str | None:
    """Read the actual device UUID from HCOM_DIR/.tmp/device_id."""
    p = os.path.join(hcom_dir, ".tmp", "device_id")
    try:
        return open(p).read().strip() or None
    except OSError:
        return None


def _kill_daemon(hcom_dir: str):
    """Kill daemon by PID file. Fallback for when `hcom daemon stop` can't reach it."""
    import signal
    pid_path = os.path.join(hcom_dir, "hcomd.pid")
    try:
        pid = int(open(pid_path).read().strip())
        os.kill(pid, signal.SIGTERM)
        # Wait up to 3s for exit
        for _ in range(30):
            time.sleep(0.1)
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return  # Dead
        # Still alive — SIGKILL
        os.kill(pid, signal.SIGKILL)
    except (OSError, ValueError):
        pass


def cleanup():
    for d in [_dir_a, _dir_b]:
        if not d:
            continue
        # Try graceful stop first, then direct PID kill, then nuke the dir
        try:
            _hcom("relay off", d, timeout=5)
        except Exception:
            pass
        try:
            _hcom("daemon stop", d, timeout=5)
        except Exception:
            pass
        _kill_daemon(d)
        shutil.rmtree(d, ignore_errors=True)


# ── Main test ────────────────────────────────────────────────────────

def run_test():
    global _dir_a, _dir_b

    _dir_a = tempfile.mkdtemp(prefix="hcom_relay_a_")
    _dir_b = tempfile.mkdtemp(prefix="hcom_relay_b_")

    print("=" * 60)
    print("Relay Roundtrip: two real hcom instances via MQTT")
    print("=" * 60)
    print(f"\n  Device A: {_dir_a}")
    print(f"  Device B: {_dir_b}")

    # ── Phase 1: Device A creates relay group ────────────────────
    print("\n[Phase 1] Device A: relay new...")

    output = check_a("relay new", timeout=30)
    print(output.rstrip())

    token = parse_token(output)
    if not token:
        fail("Could not parse token from relay new output")
    ok(f"Token: {token[:24]}...")

    # Wait for connected
    def a_connected():
        r = hcom_a("relay status")
        return r.returncode == 0 and "connected" in r.stdout.lower()

    poll_until(a_connected, "Device A relay connected", timeout=20)
    ok("Device A connected to broker")

    short_a = parse_device_id(hcom_a("relay status").stdout)
    if not short_a:
        fail("Could not parse Device A short ID")
    ok(f"Device A short ID: {short_a}")

    # ── Phase 2: Device A sends test message ─────────────────────
    print("\n[Phase 2] Device A: sending test message...")

    marker = f"relay-rt-{uuid.uuid4().hex[:8]}"
    check_a(f'send --from relaytest -- "{marker}"')
    ok(f"Sent: {marker}")

    def a_pushed():
        r = hcom_a("relay status")
        return r.returncode == 0 and "up to date" in r.stdout.lower()

    poll_until(a_pushed, "Device A push queue drained", timeout=20)
    ok("Device A: pushed to broker")

    # ── Phase 3: Device B joins ──────────────────────────────────
    print("\n[Phase 3] Device B: relay connect...")

    output = check_b(f"relay connect {token}", timeout=30)
    print(output.rstrip())

    def b_connected():
        r = hcom_b("relay status")
        return r.returncode == 0 and "connected" in r.stdout.lower()

    poll_until(b_connected, "Device B relay connected", timeout=20)
    ok("Device B connected to broker")

    # ── Phase 4: Device B sees relayed event ─────────────────────
    print("\n[Phase 4] Device B: checking for relayed event...")

    def b_has_event():
        r = hcom_b("events --last 50")
        if r.returncode != 0:
            return None
        for line in r.stdout.strip().splitlines():
            try:
                ev = json.loads(line.strip())
                data = ev.get("data", {})
                if isinstance(data, str):
                    data = json.loads(data)
                if marker in data.get("text", ""):
                    return (ev, data)
            except (json.JSONDecodeError, KeyError, TypeError):
                continue
        return None

    result = poll_until(b_has_event, f"Device B sees '{marker}'", timeout=30, interval=2.0)
    ev, data = result
    ok(f"Event received: type={ev.get('type')}")

    # Verify sender namespaced with Device A's short ID
    expected_from = f"relaytest:{short_a}"
    actual_from = data.get("from", "")
    if actual_from == expected_from:
        ok(f"from namespaced: {actual_from}")
    else:
        fail(f"from={actual_from}, expected {expected_from}")

    # Verify _relay marker points back to Device A
    actual_uuid_a = read_device_uuid(_dir_a)
    relay_marker = data.get("_relay", {})
    if actual_uuid_a and relay_marker.get("device") == actual_uuid_a:
        ok(f"_relay.device = Device A ({actual_uuid_a[:8]}...)")
    else:
        fail(f"_relay.device={relay_marker.get('device')}, expected {actual_uuid_a}")

    if relay_marker.get("short") == short_a:
        ok(f"_relay.short = {short_a}")
    else:
        fail(f"_relay.short={relay_marker.get('short')}, expected {short_a}")

    # ── Phase 5: Device A sees Device B as remote ────────────────
    print("\n[Phase 5] Device A: checking for Device B as remote...")

    short_b = parse_device_id(hcom_b("relay status").stdout)
    if not short_b:
        fail("Could not parse Device B short ID")
    ok(f"Device B short ID: {short_b}")

    def a_sees_b():
        r = hcom_a("relay status")
        if r.returncode != 0:
            return None
        for line in r.stdout.splitlines():
            if "Remote devices:" in line and short_b in line:
                return line.strip()
        return None

    remote_line = poll_until(a_sees_b, f"Device A sees {short_b} in remote devices", timeout=30, interval=2.0)
    ok(remote_line)

    # ── Cleanup ──────────────────────────────────────────────────
    print("\n[Cleanup]")
    cleanup()
    _dir_a = _dir_b = None

    print("\n" + "=" * 60)
    print("ALL PHASES PASSED")
    print("=" * 60)


if __name__ == "__main__":
    try:
        run_test()
    except KeyboardInterrupt:
        print("\nInterrupted")
        cleanup()
        sys.exit(1)
    except SystemExit:
        raise
    except Exception as e:
        print(f"\nUnexpected error: {e}", file=sys.stderr)
        import traceback
        traceback.print_exc()
        cleanup()
        sys.exit(1)