"""Structured logging for hcom. JSONL format, single file, with rotation."""

from __future__ import annotations

import json
import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from .paths import hcom_path, LOGS_DIR

_LOG_FILE = "hcom.log"
_MAX_BYTES = 8_000_000  # 8MB
_BACKUPS = int(os.environ.get("HCOM_LOG_BACKUPS", 3))


def _ts() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _log_path() -> Path:
    return hcom_path(LOGS_DIR) / _LOG_FILE


def get_log_path() -> Path:
    """Return the path to the hcom log file."""
    return _log_path()


def _rotate_if_needed(path: Path) -> None:
    """Rotate log file if over size limit. hcom.log -> hcom.log.1 -> hcom.log.2 -> hcom.log.3"""
    try:
        if not path.exists() or path.stat().st_size <= _MAX_BYTES:
            return
        for i in range(_BACKUPS, 0, -1):
            older = path.with_name(f"{_LOG_FILE}.{i}")
            if i == _BACKUPS:
                older.unlink(missing_ok=True)
            elif older.exists():
                older.rename(path.with_name(f"{_LOG_FILE}.{i + 1}"))
        path.rename(path.with_name(f"{_LOG_FILE}.1"))
    except Exception:
        pass


def log(
    level: str,
    subsystem: str,
    event: str,
    msg: str = "",
    **fields: Any,
) -> None:
    """Log structured event to ~/.hcom/.tmp/logs/hcom.log

    Args:
        level: ERROR, WARN, or INFO
        subsystem: hooks, pty, cli, relay, core
        event: Canonical event name (e.g., hook.error, pty.ready)
        msg: Human-readable detail
        **fields: Additional context (instance, session_id, tool, etc.)
    """
    entry = {
        "ts": _ts(),
        "level": level,
        "subsystem": subsystem,
        "event": event,
        **fields,
    }
    if msg:
        entry["msg"] = msg

    try:
        path = _log_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        _rotate_if_needed(path)
        with open(path, "a") as f:
            f.write(json.dumps(entry, ensure_ascii=True) + "\n")
    except Exception:
        pass  # Fail silently - never break caller


def log_error(
    subsystem: str,
    event: str,
    error: Exception | str,
    msg: str = "",
    **fields: Any,
) -> None:
    """Log error with exception details. Accepts Exception or string."""
    if isinstance(error, Exception):
        fields["error_type"] = type(error).__name__
        fields["error_msg"] = str(error)
    else:
        fields["error_type"] = "str"
        fields["error_msg"] = error
    log("ERROR", subsystem, event, msg, **fields)


def log_warn(subsystem: str, event: str, msg: str = "", **fields: Any) -> None:
    log("WARN", subsystem, event, msg, **fields)


def log_info(subsystem: str, event: str, msg: str = "", **fields: Any) -> None:
    log("INFO", subsystem, event, msg, **fields)


def get_recent_logs(
    hours: float = 1.0,
    levels: tuple[str, ...] = ("ERROR", "WARN"),
    limit: int = 20,
) -> list[dict[str, Any]]:
    """Get recent log entries filtered by level and time.

    Args:
        hours: How far back to look (default 1 hour)
        levels: Log levels to include (default ERROR and WARN)
        limit: Max entries to return (default 20)

    Returns:
        List of log entries (newest first), each with ts, level, subsystem, event, etc.
    """
    from datetime import datetime, timezone, timedelta

    path = _log_path()
    if not path.exists():
        return []

    cutoff = datetime.now(timezone.utc) - timedelta(hours=hours)
    entries: list[dict[str, Any]] = []

    try:
        with open(path, "r") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    entry = json.loads(line)
                    if entry.get("level") not in levels:
                        continue
                    # Parse timestamp
                    ts_str = entry.get("ts", "")
                    if ts_str:
                        ts = datetime.fromisoformat(ts_str.replace("Z", "+00:00"))
                        if ts >= cutoff:
                            entries.append(entry)
                except (json.JSONDecodeError, ValueError):
                    continue
    except (json.JSONDecodeError, ValueError, OSError):
        return []

    # Sort by timestamp descending (newest first) and limit
    entries.sort(key=lambda e: e.get("ts", ""), reverse=True)
    return entries[:limit]


def get_log_summary(hours: float = 1.0) -> dict[str, Any]:
    """Get summary of recent log activity.

    Returns:
        Dict with error_count, warn_count, last_error (event + ts), last_warn
    """
    entries = get_recent_logs(hours=hours, levels=("ERROR", "WARN"), limit=100)

    errors = [e for e in entries if e.get("level") == "ERROR"]
    warns = [e for e in entries if e.get("level") == "WARN"]

    summary: dict[str, Any] = {
        "error_count": len(errors),
        "warn_count": len(warns),
        "last_error": None,
        "last_warn": None,
    }

    if errors:
        e = errors[0]
        summary["last_error"] = {
            "event": f"{e.get('subsystem', '')}.{e.get('event', '')}",
            "ts": e.get("ts"),
            "instance": e.get("instance"),
        }

    if warns:
        w = warns[0]
        summary["last_warn"] = {
            "event": f"{w.get('subsystem', '')}.{w.get('event', '')}",
            "ts": w.get("ts"),
            "instance": w.get("instance"),
        }

    return summary
