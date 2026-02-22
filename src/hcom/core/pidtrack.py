"""Track hcom-launched process PIDs for orphan detection.

Persists PIDs to ~/.hcom/.tmp/launched_pids.json so they survive DB resets.
Auto-prunes dead PIDs on every read. Used by TUI and CLI to show running
processes that are no longer participating in hcom.
"""

from __future__ import annotations

import json
import os
import threading
import time
from pathlib import Path

from .paths import hcom_path

PIDFILE = ".tmp/launched_pids.json"
_cache: list[dict] | None = None
_cache_time: float = 0.0
_cache_lock = threading.Lock()
CACHE_TTL = 5.0  # seconds


def _pidfile_path() -> Path:
    return hcom_path(PIDFILE, ensure_parent=True)


def _is_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except PermissionError:
        return True  # Process exists but owned by another user
    except ProcessLookupError:
        return False


def _read_raw() -> dict[str, dict]:
    """Read pidfile. Returns {pid_str: {tool, names, launched_at, directory, ...}}."""
    path = _pidfile_path()
    if not path.exists():
        return {}
    try:
        data = json.loads(path.read_text())
        return data if isinstance(data, dict) else {}
    except (json.JSONDecodeError, OSError):
        return {}


def _write_raw(data: dict[str, dict]) -> None:
    from .paths import atomic_write

    atomic_write(_pidfile_path(), json.dumps(data))


def record_pid(pid: int, tool: str, name: str, directory: str = "", process_id: str = "",
               terminal_preset: str = "", pane_id: str = "", terminal_id: str = "",
               kitty_listen_on: str = "", session_id: str = "",
               notify_port: int = 0, inject_port: int = 0) -> None:
    """Record a launched process PID."""
    data = _read_raw()
    key = str(pid)
    if key in data:
        # Append name if not already present
        if name not in data[key].get("names", []):
            data[key].setdefault("names", []).append(name)
        if process_id and not data[key].get("process_id"):
            data[key]["process_id"] = process_id
        if terminal_preset and not data[key].get("terminal_preset"):
            data[key]["terminal_preset"] = terminal_preset
        if pane_id and not data[key].get("pane_id"):
            data[key]["pane_id"] = pane_id
        if terminal_id and not data[key].get("terminal_id"):
            data[key]["terminal_id"] = terminal_id
        if kitty_listen_on and not data[key].get("kitty_listen_on"):
            data[key]["kitty_listen_on"] = kitty_listen_on
        if session_id and not data[key].get("session_id"):
            data[key]["session_id"] = session_id
        if notify_port and not data[key].get("notify_port"):
            data[key]["notify_port"] = notify_port
        if inject_port and not data[key].get("inject_port"):
            data[key]["inject_port"] = inject_port
    else:
        entry: dict = {
            "tool": tool,
            "names": [name],
            "launched_at": time.time(),
            "directory": directory,
        }
        if process_id:
            entry["process_id"] = process_id
        if terminal_preset:
            entry["terminal_preset"] = terminal_preset
        if pane_id:
            entry["pane_id"] = pane_id
        if terminal_id:
            entry["terminal_id"] = terminal_id
        if kitty_listen_on:
            entry["kitty_listen_on"] = kitty_listen_on
        if session_id:
            entry["session_id"] = session_id
        if notify_port:
            entry["notify_port"] = notify_port
        if inject_port:
            entry["inject_port"] = inject_port
        data[key] = entry
    _write_raw(data)
    _invalidate_cache()



def get_orphan_processes(active_pids: set[int] | None = None) -> list[dict]:
    """Get running hcom processes not accounted for by active instances.

    Returns list of {pid, tool, names, launched_at, directory} for processes
    that are alive but whose PID doesn't match any active instance.
    Auto-prunes dead PIDs from the file.

    Uses a 5s cache in TUI context to avoid excessive IO.
    """
    global _cache, _cache_time
    now = time.time()
    with _cache_lock:
        if _cache is not None and (now - _cache_time) < CACHE_TTL:
            if active_pids is not None:
                return [p for p in _cache if p["pid"] not in active_pids]
            return list(_cache)

    data = _read_raw()
    alive = {}
    for pid_str, info in data.items():
        pid = int(pid_str)
        if _is_alive(pid):
            alive[pid_str] = info
        # Dead PIDs are simply not carried forward (auto-prune)

    # Write back pruned data if anything was removed
    if len(alive) != len(data):
        _write_raw(alive)

    result = []
    for pid_str, info in alive.items():
        result.append({
            "pid": int(pid_str),
            "tool": info.get("tool", "unknown"),
            "names": info.get("names", []),
            "launched_at": info.get("launched_at", 0),
            "directory": info.get("directory", ""),
            "process_id": info.get("process_id", ""),
            "terminal_preset": info.get("terminal_preset", ""),
            "pane_id": info.get("pane_id", ""),
            "terminal_id": info.get("terminal_id", ""),
            "kitty_listen_on": info.get("kitty_listen_on", ""),
            "session_id": info.get("session_id", ""),
            "notify_port": info.get("notify_port", 0),
            "inject_port": info.get("inject_port", 0),
        })

    with _cache_lock:
        _cache = result
        _cache_time = now

    if active_pids is not None:
        # Prune PIDs that are now active from the file (not just filter display)
        active_in_file = {str(p["pid"]) for p in result if p["pid"] in active_pids}
        if active_in_file:
            pruned = {k: v for k, v in alive.items() if k not in active_in_file}
            _write_raw(pruned)
            _invalidate_cache()
        return [p for p in result if p["pid"] not in active_pids]
    return list(result)


def recover_single_orphan_to_db(orphan: dict, instance_name: str) -> None:
    """Re-register a single orphan into the DB (shared by auto-recovery and CLI).

    Creates instance row, sets PID/directory, creates process/session bindings,
    and sets status to listening so the PTY delivery gate can inject messages.
    Does NOT log events, print output, or remove from pidtrack — caller handles those.
    """
    from .instances import initialize_instance_in_position_file, update_instance_position, set_status
    from .db import set_process_binding, set_session_binding, upsert_notify_endpoint

    pid = int(orphan["pid"])
    tool = str(orphan.get("tool") or "claude")
    directory = orphan.get("directory", "")
    process_id = orphan.get("process_id", "")
    session_id = (orphan.get("session_id") or "").strip()
    notify_port = orphan.get("notify_port", 0)
    inject_port = orphan.get("inject_port", 0)

    initialize_instance_in_position_file(instance_name, session_id=None, tool=tool)

    updates: dict[str, object] = {"pid": pid}
    if directory:
        updates["directory"] = directory
    update_instance_position(instance_name, updates)

    if process_id:
        set_process_binding(process_id, session_id or None, instance_name)
    if session_id:
        set_session_binding(session_id, instance_name)
        update_instance_position(instance_name, {"session_id": session_id})

    # Restore notify endpoints so PTY delivery can be woken immediately
    if notify_port:
        upsert_notify_endpoint(instance_name, "pty", int(notify_port))
    if inject_port:
        upsert_notify_endpoint(instance_name, "inject", int(inject_port))

    # Set listening so PTY delivery gate (is_idle check) allows message injection
    set_status(instance_name, "listening", "recovered")


def recover_orphans_to_db() -> int:
    """Re-register live pidtrack entries into fresh DB after reset/schema bump.

    Called after init_db() on a fresh DB. Returns count of recovered instances.
    """
    from .instances import load_instance_position
    from .identity import is_valid_base_name

    orphans = get_orphan_processes()
    recovered = 0
    for orphan in orphans:
        names = orphan.get("names") or []
        name = names[-1] if names else None
        if not name:
            continue

        # Skip if name already exists (concurrent recovery)
        if load_instance_position(name):
            continue

        # Validate name before reuse
        if not is_valid_base_name(name):
            continue

        recover_single_orphan_to_db(orphan, name)
        remove_pid(orphan["pid"])
        recovered += 1

    # Wake all recovered PTY wrappers so they reconnect to the new DB immediately
    # (instead of waiting up to 30s for the next heartbeat)
    if recovered:
        try:
            from .runtime import notify_all_instances
            notify_all_instances()
        except Exception:
            pass  # Best effort — they'll reconnect on next heartbeat anyway

    return recovered


def recover_orphan_pid(instance_name: str, process_id: str | None) -> None:
    """Recover PID from orphan tracking and set it on the new instance.

    Called during hcom start / start --as when a PTY process survives stop
    and rebinds to a new identity. Matches by process_id, sets PID on the
    instance, and removes the orphan entry.
    """
    if not process_id:
        return
    try:
        from .instances import update_instance_position

        for orphan in get_orphan_processes():
            if orphan.get("process_id") == process_id:
                update_instance_position(instance_name, {"pid": orphan["pid"]})
                remove_pid(orphan["pid"])
                return
    except Exception:
        pass


def remove_pid(pid: int) -> None:
    """Remove a PID from tracking (after kill)."""
    data = _read_raw()
    key = str(pid)
    if key in data:
        del data[key]
        _write_raw(data)
        _invalidate_cache()


def get_preset_for_pid(pid: int) -> str | None:
    """Get terminal preset name for a tracked PID."""
    data = _read_raw()
    entry = data.get(str(pid), {})
    preset = entry.get("terminal_preset", "")
    return preset if preset else None


def get_pane_id_for_pid(pid: int) -> str:
    """Get terminal pane ID for a tracked PID."""
    data = _read_raw()
    return data.get(str(pid), {}).get("pane_id", "")


def get_process_id_for_pid(pid: int) -> str:
    """Get HCOM process ID for a tracked PID."""
    data = _read_raw()
    return data.get(str(pid), {}).get("process_id", "")


def get_terminal_id_for_pid(pid: int) -> str:
    """Get captured terminal ID (stdout from open command) for a tracked PID."""
    data = _read_raw()
    return data.get(str(pid), {}).get("terminal_id", "")


def get_kitty_listen_on_for_pid(pid: int) -> str:
    """Get KITTY_LISTEN_ON socket path for a tracked PID."""
    data = _read_raw()
    return data.get(str(pid), {}).get("kitty_listen_on", "")


def _invalidate_cache() -> None:
    global _cache, _cache_time
    with _cache_lock:
        _cache = None
        _cache_time = 0.0
