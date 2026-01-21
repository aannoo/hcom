"""Instance lifecycle and identity management.

This module handles the full lifecycle of HCOM instances - from name generation
to status tracking to cleanup. It's the central authority for instance identity.

Key Concepts
------------
Instance: An AI agent session (Claude/Gemini/Codex) participating in HCOM.
    Each instance has a unique name (e.g., 'luna'), status, and metadata.

Name Generation: CVCV pattern names (consonant-vowel-consonant-vowel) like
    'luna', 'nova', 'kira'. Curated "gold names" are preferred but allocated
    via softmax sampling to maintain variety.

Status Model:
    - 'active': Instance is processing (running tool, delivering message)
    - 'listening': Instance is idle, waiting for messages
    - 'blocked': Instance needs user approval
    - 'inactive': Instance has stopped (explicit exit or stale timeout)

Identity Tiers:
    1. Process binding: HCOM-launched instances (launcher sets HCOM_PROCESS_ID)
    2. Session binding: Vanilla instances (bound via session_id from hooks)
    3. Ad-hoc: External tools (hcom start creates instance, no auto-binding)

Key Functions
-------------
generate_unique_name()
    Generate a CVCV name that's not in use.

initialize_instance_in_position_file()
    Create/update instance record in DB. Row exists = participating.

set_status()
    Update instance status and log status change event.

get_instance_status()
    Compute current status from DB fields (includes stale detection).

bind_session_to_process()
    Handle resume scenarios by binding session_id to canonical instance.

cleanup_stale_instances()
    Garbage collect instances that timed out.

Architecture Notes
------------------
- Row exists = participating (no enabled flags)
- Heartbeat via last_stop field (listening instances update this)
- Stale detection: no heartbeat for 35s (TCP) or 10s (no TCP) → inactive
- Status events logged for cross-device sync and audit
"""

from __future__ import annotations
import math
import random
from dataclasses import dataclass
from pathlib import Path
from typing import (
    Any,
    Callable,
    Literal,
    NamedTuple,
    TypedDict,
    TypeVar,
)
import time
import os

from ..shared import format_age
from .timeouts import (
    LAUNCH_PLACEHOLDER_TIMEOUT,
    HEARTBEAT_THRESHOLD_TCP,
    HEARTBEAT_THRESHOLD_NO_TCP,
    STATUS_ACTIVITY_TIMEOUT,
    UNKNOWN_HEARTBEAT_AGE,
    CLEANUP_PLACEHOLDER_THRESHOLD,
    CLEANUP_STALE_THRESHOLD,
    CLEANUP_INACTIVE_THRESHOLD,
    PROCESS_REGISTRATION_TIMEOUT,
)


# ==================== Type Definitions ====================

# Tool types supported by hcom
ToolType = Literal["claude", "gemini", "codex", "adhoc"]

# Instance status values
StatusType = Literal["active", "listening", "blocked", "inactive", "launching"]


class SubagentInfo(TypedDict, total=False):
    """Subagent tracking info in running_tasks."""

    agent_id: str
    type: str


class RunningTasks(TypedDict, total=False):
    """Structure for running_tasks JSON field."""

    active: bool
    subagents: list[SubagentInfo]


class InstanceData(TypedDict, total=False):
    """Instance data stored in database and returned by get_instance().

    This TypedDict documents all fields that may appear in instance data.
    Not all fields are present on every instance - use .get() with defaults.
    """

    # Primary identity
    name: str
    session_id: str | None
    parent_session_id: str | None
    parent_name: str | None
    agent_id: str | None
    tag: str | None

    # Position tracking
    last_event_id: int
    last_stop: float

    # Status fields
    status: str
    status_time: int
    status_context: str
    status_detail: str

    # Metadata
    directory: str
    created_at: float
    transcript_path: str
    tool: ToolType
    background: int  # SQLite stores as int (0/1)
    background_log_file: str

    # Configuration
    tcp_mode: int  # SQLite stores as int (0/1)
    wait_timeout: int
    subagent_timeout: int
    hints: str

    # Notifications
    notify_port: int | None

    # Device/relay info
    origin_device_id: str
    pid: int | None
    launch_args: str
    launch_context: str

    # Announcement tracking
    name_announced: int  # SQLite stores as int (0/1)
    launch_context_announced: int  # SQLite stores as int (0/1)

    # Runtime state
    running_tasks: str  # JSON string
    idle_since: str

    # Cache field (computed, not stored)
    _full_name: str


class ProcessBinding(TypedDict, total=False):
    """Process binding data from process_bindings table."""

    process_id: str
    session_id: str
    instance_name: str
    bound_at: float


class InstanceStatus(NamedTuple):
    """Return type for get_instance_status().

    Provides structured access to computed instance status.
    """

    status: StatusType
    age_string: str
    description: str
    age_seconds: int
    context: str = ""  # Computed context (e.g., "stale", "killed") - avoids parsing description


# Type variable for instance data dicts (can be partial)
InstanceDataT = TypeVar("InstanceDataT", bound=dict[str, Any])

# Configuration
SKIP_HISTORY = True  # New instances start at current log position (skip old messages)
UNKNOWN_HEARTBEAT_AGE = 999999  # Sentinel value for unknown heartbeat age (no last_stop or status_time)


def parse_running_tasks(json_str: str | None) -> RunningTasks:
    """Parse running_tasks JSON with safe defaults.

    Args:
        json_str: JSON string from database, or None/empty.

    Returns:
        RunningTasks dict with 'active' (bool) and 'subagents' (list[SubagentInfo]).
    """
    import json

    default: RunningTasks = {"active": False, "subagents": []}

    if not json_str:
        return default

    try:
        rt = json.loads(json_str)
        if not isinstance(rt, dict):
            return default
        return {
            "active": rt.get("active", False),
            "subagents": rt.get("subagents", []),
        }
    except json.JSONDecodeError:
        return default


def is_remote_instance(instance_data: InstanceData | dict[str, Any]) -> bool:
    """Check if instance is synced from another device (has origin_device_id).

    Args:
        instance_data: Instance data dict from database.

    Returns:
        True if instance was synced from another device.
    """
    return bool(instance_data.get("origin_device_id"))


def is_external_sender(instance_data: InstanceData | dict[str, Any]) -> bool:
    """Check if instance is an external sender (created via hcom start + send --name).

    External senders have empty/null session_id (no hooks).
    Remote instances (synced from other devices) are NOT external.
    Subagents have parent_session_id, so are not external even without session_id.

    Args:
        instance_data: Instance data dict from database.

    Returns:
        True if instance is an external sender.
    """
    if is_remote_instance(instance_data):
        return False
    if instance_data.get("parent_session_id"):
        return False
    return not instance_data.get("session_id")


# ==================== Core Instance I/O ====================


def load_instance_position(instance_name: str) -> InstanceData | dict[str, Any]:
    """Load position data for a single instance (DB wrapper).

    Args:
        instance_name: Name of the instance to load.

    Returns:
        Instance data dict if found, empty dict if not found.
    """
    from .db import get_instance

    data = get_instance(instance_name)
    return data if data else {}


def update_instance_position(
    instance_name: str, update_fields: InstanceData | dict[str, Any]
) -> None:
    """Update instance position atomically (DB wrapper).

    If instance doesn't exist, UPDATE silently affects 0 rows.
    This is intentional - after hcom reset, instance must re-register via hcom start.

    Args:
        instance_name: Name of instance to update.
        update_fields: Fields to update (partial InstanceData).
    """
    from .db import update_instance
    from .log import log_error

    try:
        # Convert booleans to integers for SQLite
        update_copy: dict[str, Any] = dict(update_fields)
        for bool_field in [
            "tcp_mode",
            "background",
            "name_announced",
            "launch_context_announced",
        ]:
            if bool_field in update_copy:
                val = update_copy[bool_field]
                if isinstance(val, bool):
                    update_copy[bool_field] = 1 if val else 0

        update_instance(instance_name, update_copy)
    except Exception as e:
        log_error(
            "core", "db.error", e, op="update_instance_position", instance=instance_name
        )
        pass  # Silent to user, logged for debugging


def capture_and_store_launch_context(instance_name: str) -> None:
    """Capture environment context and store it for the instance.

    Captures git branch, terminal program, tty, and relevant env vars.
    Used at instance creation/binding across all tool types (claude/gemini/codex).

    Args:
        instance_name: Name of instance to update.
    """
    from .context import capture_context_json

    update_instance_position(instance_name, {"launch_context": capture_context_json()})


# ==================== Instance Helper Functions ====================


def is_parent_instance(instance_data: InstanceData | dict[str, Any] | None) -> bool:
    """Check if instance is a parent (has session_id, no parent_session_id).

    Args:
        instance_data: Instance data dict, or None.

    Returns:
        True if instance is a parent (has session, no parent).
    """
    if not instance_data:
        return False
    has_session = bool(instance_data.get("session_id"))
    has_parent = bool(instance_data.get("parent_session_id"))
    return has_session and not has_parent


def is_subagent_instance(instance_data: InstanceData | dict[str, Any] | None) -> bool:
    """Check if instance is a subagent (has parent_session_id).

    Args:
        instance_data: Instance data dict, or None.

    Returns:
        True if instance is a subagent.
    """
    if not instance_data:
        return False
    return bool(instance_data.get("parent_session_id"))


# ==================== Status Functions ====================


def is_launching_placeholder(pos_data: InstanceData | dict[str, Any]) -> bool:
    """Check if instance is a launching placeholder (no session_id yet).

    These are temporary instances created by launcher before hooks fire.
    They should be counted but not shown as named rows (to avoid confusing
    temp names during resume, where placeholder name differs from canonical).

    Args:
        pos_data: Instance data dict from database.

    Returns:
        True if instance is a launching placeholder.
    """
    return (
        not pos_data.get("session_id")
        and pos_data.get("status_context") == "new"
        and pos_data.get("status", "inactive") == "inactive"
    )


def cleanup_stale_placeholders() -> int:
    """Delete placeholder instances that have been launching too long.

    Returns count of deleted instances.
    """
    from .db import iter_instances, delete_instance

    deleted = 0
    now = time.time()

    for data in iter_instances():
        if not is_launching_placeholder(data):
            continue
        name = data.get("name")
        created_at = data.get("created_at", 0)
        if name and created_at and (now - created_at) > CLEANUP_PLACEHOLDER_THRESHOLD:
            delete_instance(name)
            deleted += 1

    return deleted


def cleanup_stale_instances(
    max_stale_seconds: int = CLEANUP_STALE_THRESHOLD,
    max_inactive_seconds: int = CLEANUP_INACTIVE_THRESHOLD,
) -> int:
    """Delete instances that have been inactive too long.

    Three tiers:
    - exit:* contexts (definitively dead) → 1min cleanup
    - stale (heartbeat/activity timeout) → 1hr default
    - other inactive (adhoc, unknown, empty) → 12hr default

    This is lazy cleanup - runs during list/TUI refresh.

    Args:
        max_stale_seconds: Seconds before stale cleanup (default 3600 = 1hr, 0 = disabled)
        max_inactive_seconds: Seconds before other inactive cleanup (default 43200 = 12hr, 0 = disabled)

    Returns count of deleted instances.
    """
    from .db import iter_instances
    from .tool_utils import stop_instance

    deleted = 0

    for data in iter_instances():
        instance_status = get_instance_status(data)

        # Only target inactive instances
        if instance_status.status != "inactive":
            continue

        name = data.get("name")
        if not name:
            continue

        # Use computed context directly (no description parsing needed)
        context = instance_status.context
        age_seconds = instance_status.age_seconds

        # Exit contexts: 1min cleanup (definitively dead - killed, closed, timeout, etc)
        # These have explicit exit status set but cleanup failed (e.g., I/O error on terminal close)
        if (
            context in ("killed", "closed", "timeout", "interrupted", "session_switch")
            and age_seconds > 60
        ):
            stop_instance(name, initiated_by="system", reason="exit_cleanup")
            deleted += 1
            # Clean up one per cycle to avoid DB locks
            return deleted

        # Stale instances: shorter threshold (1hr default)
        # get_instance_status() computes stale dynamically from heartbeat timeout
        if context == "stale":
            if max_stale_seconds > 0 and age_seconds > max_stale_seconds:
                stop_instance(name, initiated_by="system", reason="stale_cleanup")
                deleted += 1
                # Clean up one per cycle to avoid DB locks
                return deleted

        # Any other inactive: longer threshold (12hr default)
        # Covers: adhoc, unknown, empty context
        if max_inactive_seconds > 0 and age_seconds > max_inactive_seconds:
            stop_instance(name, initiated_by="system", reason="inactive_cleanup")
            deleted += 1
            return deleted

    return deleted


def get_instance_status(pos_data: InstanceData | dict[str, Any]) -> InstanceStatus:
    """Get current status of instance.

    Computes the current instance status based on stored fields and heartbeat.
    Includes stale detection (heartbeat timeout) for listening instances.

    Args:
        pos_data: Instance data dict from database.

    Returns:
        InstanceStatus namedtuple with:
            - status: 'active', 'listening', 'blocked', 'inactive', or 'launching'
            - age_string: "16m" (clean format, no parens/suffix - consumers handle display)
            - description: Human-readable description like "active: Bash"
            - age_seconds: Raw integer seconds for programmatic filtering

    Note:
        Status is activity state (what instance is doing): 'active', 'listening', 'inactive'.
        Row exists = participating (all instances in DB are active participants).
    """
    status = pos_data.get("status", "inactive")
    status_time = pos_data.get("status_time", 0)
    status_context = pos_data.get("status_context", "")

    # Launching: instance created but session not yet bound / status not yet updated
    # This is the window between launcher creating instance and first hook firing
    if status_context == "new" and status == "inactive":
        created_at = pos_data.get("created_at", 0)
        age = time.time() - created_at if created_at else 0
        if age < LAUNCH_PLACEHOLDER_TIMEOUT:
            return InstanceStatus("launching", "", "launching...", 0, "new")
        else:
            # Timeout without hooks firing = launch probably failed
            return InstanceStatus(
                "inactive",
                format_age(int(age)),
                "launch probably failed",
                int(age),
                "launch_failed",
            )

    # Handle string status_time (can happen with remote instances from sync)
    if isinstance(status_time, str):
        try:
            status_time = int(float(status_time))
        except (ValueError, TypeError):
            status_time = 0

    now = int(time.time())
    age = now - status_time if status_time else 0
    # Fallback to created_at for never-started instances (status_time=0)
    # Note: check status_time, not age - age=0 when status just changed (same second)
    if not status_time:
        created_at = pos_data.get("created_at", 0)
        if created_at:
            age = now - int(created_at)

    # Heartbeat timeout check: instance was listening but heartbeat died
    # This detects terminated instances (closed window/crashed) that were listening
    # 'listening' is special: heartbeat-proven current (refreshed every ~30s)
    if status == "listening":
        last_stop = pos_data.get("last_stop", 0)
        is_remote = bool(pos_data.get("origin_device_id"))

        # Remote instances: skip heartbeat check if no last_stop (can't verify remote heartbeat)
        # Local instances: missing last_stop = stale (use status_time as fallback)
        if not last_stop and is_remote:
            pass  # Trust synced status for remote instances
        else:
            heartbeat_age = (
                now - last_stop
                if last_stop
                else (now - status_time if status_time else UNKNOWN_HEARTBEAT_AGE)
            )
            tcp_mode = pos_data.get("tcp_mode", False)
            # Remote instances use 40s threshold (sync interval).
            # Local instances use:
            # - HEARTBEAT_THRESHOLD_TCP when there is an active TCP notify endpoint (pty, listen, hooks)
            # - HEARTBEAT_THRESHOLD_NO_TCP otherwise (no TCP listener means rapid stale detection)
            has_tcp_listener = bool(tcp_mode)
            if not has_tcp_listener:
                try:
                    from .db import get_db

                    conn = get_db()
                    row = conn.execute(
                        "SELECT 1 FROM notify_endpoints WHERE instance = ? LIMIT 1",
                        (pos_data.get("name") or "",),
                    ).fetchone()
                    has_tcp_listener = bool(row)
                except Exception:
                    has_tcp_listener = bool(tcp_mode)

            threshold = HEARTBEAT_THRESHOLD_TCP if (has_tcp_listener or is_remote) else HEARTBEAT_THRESHOLD_NO_TCP
            if heartbeat_age > threshold:
                status = "inactive"
                status_context = "stale:listening"
                age = heartbeat_age
            else:
                # Heartbeat within threshold - age=0 shows "now" in TUI
                age = 0
    # Activity timeout check: no status updates for extended period
    # This detects terminated instances that were active/blocked/etc when closed
    elif status not in ["inactive"]:
        status_age = now - status_time if status_time else 0

        # Fallback to created_at for instances that never updated status (e.g. active: new)
        # Note: check status_time, not status_age - status_age=0 when status just changed (same second)
        if not status_time:
            created_at = pos_data.get("created_at", 0)
            if created_at:
                status_age = now - int(created_at)

        if status_age > STATUS_ACTIVITY_TIMEOUT:
            prev_status = status  # Capture before changing
            status = "inactive"
            status_context = f"stale:{prev_status}"
            age = status_age

    # Build description from status and context
    description = get_status_description(status, status_context)

    # Adhoc instances: strip "inactive: " prefix (we don't claim dead, just show last event)
    tool = pos_data.get("tool", "claude")
    if tool == "adhoc" and status == "inactive":
        if description.startswith("inactive: "):
            description = description[10:]
        elif description == "inactive":
            description = ""

    # Extract simple context key for callers (e.g., "stale", "killed", "timeout")
    # Format: "stale:listening" -> "stale", "exit:timeout" -> "timeout", "tool:Bash" -> "tool"
    if ":" in status_context:
        prefix, suffix = status_context.split(":", 1)
        # For exit: use suffix (the exit reason), otherwise use prefix
        simple_context = suffix if prefix == "exit" else prefix
    else:
        simple_context = status_context

    # Cast status to StatusType for type safety (validated above)
    return InstanceStatus(status, format_age(int(age)), description, int(age), simple_context)  # type: ignore[arg-type]


def get_status_description(status: str, context: str = "") -> str:
    """Build human-readable status description from status + metadata tokens

    Metadata token format:
    - deliver:{sender} - message delivery
    - tool:{name} - tool execution
    - exit:{reason} - exit states (timeout, orphaned, task_completed, disabled, clear)
    - stale:{prev_status} - stale detection preserving previous state
    - suspended - headless process exited, resumable via message
    - resuming - headless process starting up after suspend (~5s)
    - unknown - unknown state
    - Empty string - simple idle (no context needed)
    """
    if status == "active":
        if context.startswith("deliver:"):
            sender = context[8:]  # "deliver:luna" → "luna"
            return f"active: msg from {sender}"
        elif context.startswith("tool:"):
            tool = context[5:]  # "tool:Bash" → "Bash"
            return f"active: {tool}"
        elif context.startswith("approved:"):
            tool = context[9:]  # "approved:Bash" → "Bash"
            return f"active: approved {tool}"
        elif context == "resuming":
            return "resuming..."
        return f"active: {context}" if context else "active"
    elif status == "listening":
        if context == "tui:not-ready":
            return "listening: blocked"
        elif context == "tui:not-idle":
            return "listening: waiting for idle"
        elif context == "tui:user-active":
            return "listening: user typing"
        elif context == "tui:output-unstable":
            return "listening: output streaming"
        elif context == "tui:prompt-has-text":
            return "listening: uncommitted text"
        elif context.startswith("tui:"):
            # Fallback for any other tui:* gate context
            reason = context[4:]  # "tui:something" → "something"
            return f"listening: {reason.replace('-', ' ')}"
        elif context == "suspended":
            return "listening: suspended"
        # Don't show 'ready' or other normal contexts
        return "listening"
    elif status == "blocked":
        if context == "pty:approval":
            return "blocked: approval pending"
        return f"blocked: {context}" if context else "blocked: permission needed"
    elif status == "inactive":
        if context.startswith("stale:"):
            return "inactive: stale"
        elif context.startswith("exit:"):
            reason = context[5:]  # "exit:timeout" → "timeout"
            return f"inactive: {reason}"
        elif context == "unknown":
            return "inactive: unknown"
        return f"inactive: {context}" if context else "inactive"
    return "unknown"


def get_status_icon(
    pos_data: InstanceData | dict[str, Any], status: StatusType | str | None = None
) -> str:
    """Get status icon for instance, considering tool type.

    Adhoc instances use neutral icon (◦) when inactive,
    since we don't know if alive or dead - just last event.

    Args:
        pos_data: Instance data dict from database.
        status: Computed status from get_instance_status() (use this, not raw DB status).

    Returns:
        Single character status icon (e.g., '▶', '◉', '■', '○').
    """
    from ..shared import STATUS_ICONS, ADHOC_ICON

    # Resolve status from pos_data if not provided
    resolved_status: str = status if status is not None else (
        pos_data.get("status") or "inactive"
    )
    tool = pos_data.get("tool", "claude")

    # Launching: flash between ◎ and ○ (2Hz)
    if resolved_status == "launching":
        return "◎○"[int(time.time() * 2) % 2]

    # Adhoc: only 2 states - listening (normal ◉) or neutral (◦)
    # Neutral when not actively listening (we can't verify if alive)
    if tool == "adhoc" and resolved_status != "listening":
        return ADHOC_ICON

    return STATUS_ICONS.get(resolved_status, STATUS_ICONS["inactive"])


def set_status(
    instance_name: str,
    status: StatusType | str,
    context: str = "",
    detail: str = "",
    msg_ts: str = "",
    launcher: str | None = None,
    batch_id: str | None = None,
) -> None:
    """Set instance status with timestamp and log status change event.

    Args:
        instance_name: Name of the instance to update.
        status: New status value. Valid values:
            'active' - Instance is actively working (processing tool, delivering message).
            'listening' - Instance is idle and waiting for messages.
            'inactive' - Instance is not running (stopped, timed out).
            'blocked' - Instance is waiting for user approval.
        context: Type token describing what triggered the status change.
            Format: 'category:identifier' (e.g., 'tool:Bash', 'deliver:luna', 'exit:timeout').
        detail: Additional context value (command string, file path, task prompt).
        msg_ts: Timestamp of last message read (for cross-device read receipts).
        launcher: Override for HCOM_LAUNCHED_BY (for headless launches where env not set).
        batch_id: Override for HCOM_LAUNCH_BATCH_ID (for headless launches where env not set).
    """
    from .db import log_event

    # Check if this is first status update (for ready event / launcher notification)
    current_data = load_instance_position(instance_name)
    is_new = current_data.get("status_context") == "new" if current_data else True

    # Build updates dict - atomically include last_stop when entering idle
    updates = {
        "status": status,
        "status_time": int(time.time()),
        "status_context": context,
        "status_detail": detail,
    }
    # Set last_stop heartbeat when entering listening state (for staleness detection)
    if status == "listening":
        updates["last_stop"] = int(time.time())

    update_instance_position(instance_name, updates)

    if is_new:
        try:
            # Use explicit params if provided, else fall back to env vars
            launcher = launcher or os.environ.get("HCOM_LAUNCHED_BY", "unknown")
            batch_id = batch_id or os.environ.get("HCOM_LAUNCH_BATCH_ID")

            event_data = {
                "action": "ready",
                "by": launcher,
                "status": status,
                "context": context,
            }
            if batch_id:
                event_data["batch_id"] = batch_id

            log_event(event_type="life", instance=instance_name, data=event_data)

            # Check if this is the last instance from a launch batch
            if launcher != "unknown" and batch_id:
                from .db import get_db
                import json

                db = get_db()

                # Find the launch event for this batch
                launch_event = db.execute(
                    """
                    SELECT data FROM events
                    WHERE type = 'life'
                      AND instance = ?
                      AND json_extract(data, '$.action') = 'batch_launched'
                      AND json_extract(data, '$.batch_id') = ?
                    LIMIT 1
                """,
                    (launcher, batch_id),
                ).fetchone()

                if launch_event:
                    launch_data = json.loads(launch_event["data"])
                    expected_count = launch_data.get("launched", 0)

                    if expected_count > 0:
                        # Count ready events with matching batch_id
                        ready_count = db.execute(
                            """
                            SELECT COUNT(*) as count FROM events
                            WHERE type = 'life'
                              AND json_extract(data, '$.action') = 'ready'
                              AND json_extract(data, '$.batch_id') = ?
                        """,
                            (batch_id,),
                        ).fetchone()["count"]

                        # If this is the last one, send notification to launcher
                        if ready_count >= expected_count:
                            # Check if notification already sent (idempotency)
                            existing = db.execute(
                                """
                                SELECT 1 FROM events
                                WHERE type = 'message'
                                  AND instance = 'sys_[hcom-launcher]'
                                  AND json_extract(data, '$.text') LIKE ?
                                LIMIT 1
                            """,
                                (f"%batch: {batch_id}%",),
                            ).fetchone()

                            if not existing:
                                from .messages import send_system_message

                                # Get instance names from this batch
                                ready_instances = db.execute(
                                    """
                                    SELECT DISTINCT instance FROM events
                                    WHERE type = 'life'
                                      AND json_extract(data, '$.action') = 'ready'
                                      AND json_extract(data, '$.batch_id') = ?
                                """,
                                    (batch_id,),
                                ).fetchall()

                                instances_list = ", ".join(
                                    row["instance"] for row in ready_instances
                                )

                                send_system_message(
                                    "[hcom-launcher]",
                                    f"@{launcher} All {expected_count} instances ready: {instances_list} (batch: {batch_id})",
                                )
        except Exception as e:
            from .log import log_error

            log_error("core", "db.error", e, op="batch_notification")

    # Log status change event (best-effort, non-blocking)
    # Include position + msg_ts for cross-device read receipt sync
    try:
        position = current_data.get("last_event_id", 0) if current_data else 0
        data = {"status": status, "context": context, "position": position}
        if detail:
            data["detail"] = detail
        if msg_ts:
            data["msg_ts"] = msg_ts
        log_event(event_type="status", instance=instance_name, data=data)
        # Push immediately on exit so remote devices see final state
        if status == "inactive":
            from ..relay import notify_relay_tui, push

            if not notify_relay_tui():
                push(force=True)
    except Exception:
        pass  # Don't break hooks if event logging fails  # Don't break hooks if event logging fails


def set_gate_status(instance_name: str, context: str, detail: str = ""):
    """Update gate blocking status WITHOUT logging a status event.

    Used for transient PTY gate states (tui:*) that shouldn't pollute the events table.
    Only updates the instance row; TUI reads this for display but no event is created.

    Args:
        instance_name: Instance to update
        context: Gate context (e.g., 'tui:not-ready', 'tui:user-active') or '' to clear
        detail: Human-readable detail (e.g., 'user typing', 'prompt not visible')
    """
    updates = {
        "status_context": context,
        "status_detail": detail,
        # Don't update status_time - keep last real status time
    }
    update_instance_position(instance_name, updates)


# ==================== Identity Management ====================

# ----------------------------
# CVCV Name Generation System
# ----------------------------
# Names are 4-letter CVCV (consonant-vowel-consonant-vowel) patterns.
# Curated "gold" names score highest, generated names fill the pool.

# CANDIDATE WORDS TO CONSIDER ADDING TO GOLD_NAMES:
# (All verified CVCV, real or real-sounding)
# ----------------------------------------------------------------
# Real names: rina, sana, kana, hana, yuki, riku, sora, hiro, yuma, rena
#             yuki, miki, yuri, mari, yoko, keji, tomo, nana, rumi, sumi
#             zena, dina, gina, fara, mara, sera, vera, zora, lena, lana
#             hera, juno, dara, kana, maya, vega, zola, kobe, rafa, beto
# Real words: mesa, cola, sofa, yoga, tuna, puma, diva, lava, dodo, solo
#             memo, demo, veto, hero, zero, kiwi, tofu, tutu, guru, sumo
#             polo, logo, loco, mojo, dojo, judo, silo, halo, vino, peso
#             feta, pita, saga, raga, duma, soma, coma, beta, zeta, gala
#             mama, papa, baba, dada, bobo, coco, lulu, fifi, gogo, mumu
# Nature:     fava, lima, sago, tapa, kava, bora, faro, mako, nori, miso
#             kobi, tobi, raki, sake, brie, goji, gabi, ragi, soba, maca
# Invented:   ziru, voku, neku, rizu, kovi, miru, boku, tazu, rino, zeno
#             kiro, vero, miko, delu, pazu, hiko, zumi, reko, niku, valo
#             kazu, mero, zuki, piru, hoku, vano, kelu, ritu, zako, melu
#             niro, veki, toku, razu, kinu, zelo, piko, hazu, viru, moku
# ----------------------------------------------------------------


GOLD_NAMES: set[str] = {
    # Real/common names (high recognition)
    "luna",
    "nova",
    "nora",
    "zara",
    "kira",
    "mila",
    "lola",
    "lara",
    "sara",
    "rhea",
    "nina",
    "mira",
    "tara",
    "sora",
    "cora",
    "dora",
    "gina",
    "lina",
    "viva",
    "risa",
    "mimi",
    "coco",
    "koko",
    "lili",
    "navi",
    "ravi",
    "rani",
    "riko",
    "niko",
    "mako",
    "saki",
    "maki",
    "nami",
    "loki",
    "rori",
    "lori",
    "mori",
    "nori",
    "tori",
    "gigi",
    "hana",
    "hiro",
    "tomo",
    "sumi",
    "vega",
    "kobe",
    "rafa",
    "lana",
    "lena",
    "dara",
    "niro",
    "yuki",
    "yuri",
    "maya",
    "juno",
    "nico",
    "rosa",
    "vera",
    "rina",
    "mika",
    "yoko",
    "yumi",
    "ruby",
    "lily",
    "cici",
    "hera",
    # Real words (familiar sounds)
    "miso",
    "taro",
    "boba",
    "kava",
    "soda",
    "cola",
    "coda",
    "data",
    "beta",
    "sofa",
    "mono",
    "moto",
    "tiki",
    "koda",
    "kali",
    "gala",
    "hula",
    "kula",
    "puma",
    "yoga",
    "zola",
    "zori",
    "veto",
    "vivo",
    "dino",
    "nemo",
    "hero",
    "zero",
    "memo",
    "demo",
    "polo",
    "solo",
    "logo",
    "halo",
    "dojo",
    "judo",
    "sumo",
    "tofu",
    "guru",
    "vino",
    "diva",
    "dodo",
    "silo",
    "peso",
    "lulu",
    "pita",
    "feta",
    "bobo",
    "brie",
    "fava",
    "duma",
    "beto",
    "moku",
    "bozo",
    "tuna",
    "lava",
    "hobo",
    "kiwi",
    "mojo",
    "yoyo",
    "sake",
    "wiki",
    "fiji",
    "bali",
    "kona",
    "poke",
    "cafe",
    "soho",
    "boho",
    "nano",
    "zulu",
    "deli",
    "rose",
    "jedi",
    "yoda",
    # Invented but natural-sounding
    "zumi",
    "reko",
    "valo",
    "kazu",
    "mero",
    "niru",
    "piko",
    "hazu",
    "toku",
    "veki",
}

BANNED_NAMES: set[str] = {
    # CLI commands / common terms that would cause confusion
    "help",
    "exit",
    "quit",
    "sudo",
    "bash",
    "curl",
    "grep",
    "init",
    "list",
    "send",
    "stop",
    "test",
    "meta",
}

# CVCV generator alphabet (tuned for friendly/pronounceable names)
_CONSONANTS = "bdfghklmnprstvz"  # 15 consonants (soft + slight spice)
_VOWELS = "aeiou"  # 5 vowels


def _score_name(name: str) -> int:
    """Score a name for quality. Higher = more preferred."""
    if name in BANNED_NAMES:
        return -(10**9)

    score = 0

    # Strong preference for curated names (~90% chance when pool is empty)
    if name in GOLD_NAMES:
        score += 4_000

    # Friendly flow letters (l, r, n, m)
    if any(ch in "lrnm" for ch in name):
        score += 40

    # Slight spice: prefer exactly one v/z
    vz_count = sum(ch in "vz" for ch in name)
    if vz_count == 1:
        score += 12
    elif vz_count >= 2:
        score -= 15

    # Avoid doubled vowels (e.g., "mama" pattern)
    if name[1] == name[3]:
        score -= 8

    # Name-like endings (a, e, o)
    if name[3] in "aeo":
        score += 6

    return score


@dataclass(frozen=True)
class _ScoredName:
    score: int
    name: str


def _build_name_pool(limit: int = 5000) -> list[_ScoredName]:
    """Build scored pool of all valid CVCV names plus curated GOLD_NAMES."""
    candidates: list[_ScoredName] = []
    seen: set[str] = set()

    # Generate all CVCV combinations from the alphabet
    for c1 in _CONSONANTS:
        for v1 in _VOWELS:
            for c2 in _CONSONANTS:
                for v2 in _VOWELS:
                    name = f"{c1}{v1}{c2}{v2}"
                    if name in BANNED_NAMES:
                        continue
                    candidates.append(_ScoredName(_score_name(name), name))
                    seen.add(name)

    # Inject GOLD_NAMES that don't match the CVCV pattern (e.g., coco, juno, maya)
    # These get the +10,000 score bonus from _score_name
    for name in GOLD_NAMES:
        if name not in seen and name not in BANNED_NAMES:
            candidates.append(_ScoredName(_score_name(name), name))
            seen.add(name)

    # Sort by score descending
    candidates.sort(key=lambda x: x.score, reverse=True)

    # Limit results
    return candidates[:limit]


# Pre-built pool (computed once at module load)
_NAME_POOL: list[_ScoredName] = _build_name_pool()


def _is_too_similar(name: str, existing: set[str]) -> bool:
    """Reject names that are too similar to active instances (e.g., zavi vs zivi)."""
    for other in existing:
        if len(other) != len(name):
            continue
        if sum(1 for a, b in zip(name, other) if a != b) <= 1:
            return True
    return False


def _allocate_name(
    is_taken: Callable[[str], bool],
    existing_names: set[str],
    attempts: int = 200,
    top_window: int = 1200,
    temperature: float = 900.0,
) -> str:
    """Allocate a name with bias toward high-scoring names.

    Uses softmax-like sampling from top-scored names to avoid
    always consuming gold names first while maintaining quality.

    Args:
        is_taken: Callback to check if name is already used
        attempts: Max sampling attempts before greedy fallback
        top_window: Only sample from top N ranked names
        temperature: Higher = flatter distribution, lower = more greedy
    """
    rng = random.Random()

    window = _NAME_POOL[: max(50, min(top_window, len(_NAME_POOL)))]
    scores = [x.score for x in window]
    max_score = max(scores)

    # Softmax-like weights (numerically stable)
    weights = [math.exp((s - max_score) / temperature) for s in scores]

    for _ in range(attempts):
        choice = rng.choices(window, weights=weights, k=1)[0].name
        if not is_taken(choice) and not _is_too_similar(choice, existing_names):
            return choice

    # Fallback: greedy scan (guarantees return if any available)
    for item in _NAME_POOL:
        if not is_taken(item.name) and not _is_too_similar(item.name, existing_names):
            return item.name

    raise RuntimeError("No available names left in pool")


# Legacy: hash_to_name used by device.py for device short IDs
_HASH_WORDS = [n.name for n in _NAME_POOL[:500]]  # Top 500 for hashing


def hash_to_name(input_str: str, collision_attempt: int = 0) -> str:
    """Hash any string to a memorable 4-char name.

    Used for device short IDs. For instance names, use generate_unique_name().
    Uses FNV-1a inspired hash for better distribution.
    """
    # FNV-1a hash (32-bit) for better distribution
    h = 2166136261  # FNV offset basis
    for c in input_str:
        h ^= ord(c)
        h = (h * 16777619) & 0xFFFFFFFF  # FNV prime, mask to 32-bit
    h = (h + collision_attempt * 31337) & 0xFFFFFFFF
    return _HASH_WORDS[h % len(_HASH_WORDS)]


def get_full_name(instance_data: InstanceData | dict[str, Any] | None) -> str:
    """Get full display name from instance data.

    Architecture: DB stores base name ('luna') + optional tag ('team').
    Full name ('team-luna') is computed at display time, not stored.
    Use this in display/output code. Use base name for DB lookups and routing.

    Args:
        instance_data: Instance data dict, or None.

    Returns:
        '{tag}-{name}' if tag exists, else just '{name}'.
        Returns empty string if instance_data is None.

    Note:
        Caches result on dict as '_full_name' for subsequent calls.
    """
    if not instance_data:
        return ""

    # Return cached value if available
    if "_full_name" in instance_data:
        return instance_data["_full_name"]

    name = instance_data.get("name", "")
    tag = instance_data.get("tag")
    full_name = f"{tag}-{name}" if tag else name

    # Cache on dict (safe - update functions use explicit field dicts)
    instance_data["_full_name"] = full_name
    return full_name


def generate_unique_name(max_retries: int = 200) -> str:
    """Generate a unique random instance name using CVCV pattern.

    Names are 4-letter CVCV (consonant-vowel-consonant-vowel) patterns.
    Curated "gold" names (luna, nova, kira, etc.) are preferred but not
    always chosen first to maintain variety.

    Collision handling: biased random sampling with greedy fallback.
    Checks both active instances AND stopped instances from events table
    to avoid name reuse within the same session.
    DB UNIQUE constraint provides safety net for TOCTOU races.
    """
    from .db import get_instance, iter_instances, get_db

    existing_names = {row.get("name", "") for row in iter_instances()}

    # Also check stopped instances from events to avoid name reuse
    try:
        db = get_db()
        stopped_rows = db.execute(
            """
            SELECT DISTINCT instance FROM events
            WHERE type = 'life'
              AND json_extract(data, '$.action') = 'stopped'
            """
        ).fetchall()
        stopped_names = {row["instance"] for row in stopped_rows}
        existing_names.update(stopped_names)
    except Exception:
        pass  # Best-effort - continue with active names only

    return _allocate_name(
        is_taken=lambda n: bool(get_instance(n)) or n in existing_names,
        existing_names=existing_names,
        attempts=max_retries,
    )


def resolve_instance_name(session_id: str) -> tuple[str | None, dict | None]:
    """Resolve instance name (base name) for a session_id by lookup only."""
    from .db import get_session_binding, get_instance

    if not session_id:
        return None, None

    existing_name = get_session_binding(session_id)
    if not existing_name:
        return None, None

    data = get_instance(existing_name)
    return existing_name, data


def resolve_process_binding(process_id: str | None) -> str | None:
    """Resolve instance name for a process_id via process_bindings."""
    if not process_id:
        return None
    from .db import get_process_binding

    binding = get_process_binding(process_id)
    if binding:
        return binding.get("instance_name") or None
    return None


def resolve_instance_from_binding(
    session_id: str | None = None,
    process_id: str | None = None,
    transcript_path: str | None = None,
) -> dict | None:
    """Resolve instance via process binding, session binding, or transcript marker.

    Shared resolution logic for Claude/Codex/Gemini hooks. Tries in order:
    1. HCOM_PROCESS_ID env var → process_bindings → instance
    2. session_id parameter → session_bindings → instance
    3. transcript_path → [HCOM:BIND:X] marker → create binding → instance

    Args:
        session_id: Session ID from hook payload (thread-id for Codex, session_id for Gemini/Claude)
        process_id: Process ID (defaults to HCOM_PROCESS_ID env var)
        transcript_path: Path to transcript file for marker-based binding (handles !hcom start)

    Returns:
        Instance dict if found, None otherwise.
    """
    from .db import get_instance, get_process_binding, get_session_binding

    # Use env var if process_id not provided
    if process_id is None:
        process_id = os.environ.get("HCOM_PROCESS_ID")

    # Path 1: Process binding (hcom-launched instances)
    if process_id:
        binding = get_process_binding(process_id)
        instance_name = binding.get("instance_name") if binding else None
        if instance_name:
            instance = get_instance(instance_name)
            if instance:
                return instance

    # Path 2: Session binding (vanilla instances, or fallback after DB reset)
    if session_id:
        instance_name = get_session_binding(session_id)
        if instance_name:
            instance = get_instance(instance_name)
            if instance:
                return instance

    # Path 3: Transcript marker binding (handles !hcom start / vanilla instances)
    if session_id and transcript_path:
        from ..hooks.utils import _try_bind_from_transcript

        bound_name = _try_bind_from_transcript(session_id, transcript_path)
        if bound_name:
            instance = get_instance(bound_name)
            if instance:
                return instance

    return None


def bind_session_to_process(
    session_id: str,
    process_id: str | None,
) -> str | None:
    """Bind session_id to canonical instance for process_id.

    Handles resume scenarios:
    - If session_id matches existing instance (canonical), switch to it
    - If placeholder has no session_id (true placeholder from launcher), merge fields and delete
    - If placeholder has session_id (real instance, user switched sessions), keep it, just rebind

    Returns canonical instance name if resolved, else None.
    """
    from .log import log_info

    log_info(
        "binding",
        "bind_session_to_process.entry",
        session_id=session_id,
        process_id=process_id,
    )

    if not session_id:
        log_info("binding", "bind_session_to_process.no_session_id")
        return None

    from .db import (
        get_session_binding,
        get_process_binding,
        get_instance,
        set_process_binding,
        update_instance,
        delete_instance,
        delete_session_bindings_for_instance,
        migrate_notify_endpoints,
    )

    placeholder_name = None
    placeholder_data = None
    if process_id:
        binding = get_process_binding(process_id)
        placeholder_name = binding.get("instance_name") if binding else None
        log_info(
            "binding",
            "bind_session_to_process.process_binding",
            process_id=process_id,
            binding=binding,
            placeholder_name=placeholder_name,
        )
        if placeholder_name:
            placeholder_data = get_instance(placeholder_name)
            log_info(
                "binding",
                "bind_session_to_process.placeholder_data",
                placeholder_name=placeholder_name,
                placeholder_session_id=placeholder_data.get("session_id") if placeholder_data else None,
            )

    canonical = get_session_binding(session_id)
    log_info(
        "binding",
        "bind_session_to_process.canonical_lookup",
        session_id=session_id,
        canonical=canonical,
    )

    if canonical:
        log_info(
            "binding",
            "bind_session_to_process.canonical_exists",
            canonical=canonical,
            placeholder_name=placeholder_name,
            will_switch=placeholder_name != canonical if placeholder_name else False,
        )
        # Reset last_stop on resume to prevent stale heartbeat triggering immediate inactive detection
        resume_updates: dict[str, Any] = {"last_stop": int(time.time())}

        if placeholder_name and placeholder_name != canonical:
            # Always migrate notify_endpoints to canonical
            migrate_notify_endpoints(placeholder_name, canonical)

            # Check if placeholder is a true placeholder (no session_id) or a real instance
            is_true_placeholder = placeholder_data and not placeholder_data.get(
                "session_id"
            )
            log_info(
                "binding",
                "bind_session_to_process.placeholder_handling",
                placeholder_name=placeholder_name,
                is_true_placeholder=is_true_placeholder,
                action="merge_and_delete" if is_true_placeholder else "mark_inactive",
            )

            if (
                is_true_placeholder and placeholder_data
            ):  # placeholder_data checked for mypy
                # Merge launcher-set fields from placeholder to canonical
                if placeholder_data.get("tag"):
                    resume_updates["tag"] = placeholder_data["tag"]
                if placeholder_data.get("background"):
                    resume_updates["background"] = placeholder_data["background"]
                if placeholder_data.get("launch_args"):
                    resume_updates["launch_args"] = placeholder_data["launch_args"]
                # Reset status_context for ready event (HCOM-launched resumes)
                if os.environ.get("HCOM_LAUNCHED") == "1":
                    resume_updates["status_context"] = "new"

                # Delete true placeholder (it was just a temporary identity)
                if not delete_instance(placeholder_name):
                    # Deletion failed - rollback notify_endpoints migration
                    migrate_notify_endpoints(canonical, placeholder_name)
            else:
                # Real instance being abandoned due to session switch.
                # Mark inactive and remove session binding (process no longer serves it)
                set_status(placeholder_name, "inactive", "exit:session_switch")
                delete_session_bindings_for_instance(placeholder_name)

        # Apply resume updates to canonical instance
        update_instance(canonical, resume_updates)

        if process_id:
            set_process_binding(process_id, session_id, canonical)
        log_info(
            "binding",
            "bind_session_to_process.return_canonical",
            canonical=canonical,
            session_id=session_id,
        )
        return canonical

    if placeholder_name:
        # Clear session_id from any old instance (UNIQUE constraint on instances.session_id)
        from .db import clear_session_id_from_other_instances, rebind_session

        log_info(
            "binding",
            "bind_session_to_process.bind_placeholder",
            placeholder_name=placeholder_name,
            session_id=session_id,
        )

        clear_session_id_from_other_instances(session_id, placeholder_name)

        update_instance(placeholder_name, {"session_id": session_id})
        # Create session_binding here (not in caller) to ensure atomic binding
        rebind_session(session_id, placeholder_name)
        if process_id:
            set_process_binding(process_id, session_id, placeholder_name)
        log_info(
            "binding",
            "bind_session_to_process.return_placeholder",
            placeholder_name=placeholder_name,
            session_id=session_id,
        )
        return placeholder_name

    log_info("binding", "bind_session_to_process.return_none")
    return None


def initialize_instance_in_position_file(
    instance_name: str,
    session_id: str | None = None,
    parent_session_id: str | None = None,
    parent_name: str | None = None,
    agent_id: str | None = None,
    transcript_path: str | None = None,
    tool: str | None = None,
    background: bool = False,
    tag: str | None = None,
    wait_timeout: int | None = None,
    subagent_timeout: int | None = None,
    hints: str | None = None,
) -> bool:
    """Initialize instance in DB with required fields (idempotent).

    Row exists = participating. No enabled flag needed.

    Args:
        instance_name: Unique name for this instance (e.g., 'luna', 'nova').
        session_id: Claude session ID for transcript binding (from ~/.claude/projects/).
        parent_session_id: Parent's session ID (for subagents).
        parent_name: Parent instance name (for subagents).
        agent_id: Claude agent ID (UUID from Task tool).
        transcript_path: Path to transcript file.
        tool: Tool type - 'claude' (default), 'gemini', 'codex', or 'adhoc'.
        background: If True, this is a headless instance (no interactive terminal).
        tag: Optional team tag (for @-mention groups).
        wait_timeout: Idle timeout in seconds before disconnecting.
        subagent_timeout: Timeout for subagents in seconds.
        hints: Text appended to all messages this instance receives.

    Returns:
        True on success, False on failure.
    """
    from .db import get_instance, save_instance, get_last_event_id
    import sqlite3

    try:
        # Check if already exists - if so, update it with provided params (don't skip)
        existing = get_instance(instance_name)
        if existing:
            # Instance exists (possibly placeholder) - update with provided metadata
            updates: dict[str, Any] = {}
            if session_id is not None:
                updates["session_id"] = session_id
            if parent_session_id is not None:
                updates["parent_session_id"] = parent_session_id
            if parent_name is not None:
                updates["parent_name"] = parent_name
            if agent_id is not None:
                updates["agent_id"] = agent_id
            if transcript_path is not None:
                updates["transcript_path"] = transcript_path
            if tool is not None:
                updates["tool"] = tool
            if background:
                updates["background"] = int(background)

            # Fix last_event_id for new instances (SKIP_HISTORY fix)
            # Only set if:
            # 1. last_event_id is 0 (never received messages)
            # 2. AND session_id is not set (true placeholder, not a resumed instance)
            # This prevents accidentally skipping messages for resumed instances
            is_true_placeholder = not existing.get("session_id")
            if (
                SKIP_HISTORY
                and existing.get("last_event_id", 0) == 0
                and is_true_placeholder
            ):
                current_max = get_last_event_id()
                # Validate launch event ID isn't stale (higher than max = DB was reset)
                launch_event_id_str = os.environ.get("HCOM_LAUNCH_EVENT_ID")
                if launch_event_id_str:
                    launch_event_id = int(launch_event_id_str)
                    if launch_event_id <= current_max:
                        updates["last_event_id"] = launch_event_id
                    else:
                        updates["last_event_id"] = current_max
                else:
                    updates["last_event_id"] = current_max

            # Reset status_context for HCOM-launched resumed sessions (triggers ready event)
            if os.environ.get("HCOM_LAUNCHED") == "1":
                updates["status_context"] = "new"

            if updates:
                from .db import update_instance

                update_instance(instance_name, updates)
            return True

        # Determine starting event ID: skip history or read from beginning
        initial_event_id = 0
        if SKIP_HISTORY:
            current_max = get_last_event_id()
            # Use launch event ID if valid (for hcom-launched instances)
            # Validate it's not stale (higher than current max = DB was reset)
            launch_event_id_str = os.environ.get("HCOM_LAUNCH_EVENT_ID")
            if launch_event_id_str:
                launch_event_id = int(launch_event_id_str)
                if launch_event_id <= current_max:
                    initial_event_id = launch_event_id
                else:
                    # Stale env var from before DB reset - use current max
                    initial_event_id = current_max
            else:
                initial_event_id = current_max

        data = {
            "name": instance_name,
            "last_event_id": initial_event_id,
            "directory": str(Path.cwd()),
            "last_stop": 0,
            "created_at": time.time(),
            "session_id": session_id if session_id else None,  # NULL not empty string
            "transcript_path": "",
            "name_announced": 0,
            "tag": None,
            "status": "inactive",  # New instances start inactive until first hook/PTY fires
            "status_time": int(time.time()),
            # status_context="new" triggers ready event on first status update (see set_status)
            "status_context": "new",
            "tool": tool or "claude",  # Tool type: claude, gemini, codex
            "background": int(background),  # Headless mode flag
        }

        # Set tag: use provided tag (for reclaimed instances), or config tag, or None
        if tag:
            data["tag"] = tag
        elif session_id or parent_session_id or os.environ.get("HCOM_LAUNCHED") == "1":
            try:
                from .config import get_config

                config_tag = get_config().tag
                if config_tag:
                    data["tag"] = config_tag
            except Exception:
                pass

        # Set restored settings (for reclaimed instances)
        if wait_timeout is not None:
            data["wait_timeout"] = wait_timeout
        if subagent_timeout is not None:
            data["subagent_timeout"] = subagent_timeout
        if hints is not None:
            data["hints"] = hints

        # Add parent_session_id and parent_name for subagents
        if parent_session_id:
            data["parent_session_id"] = parent_session_id
        if parent_name:
            data["parent_name"] = parent_name
        if agent_id:
            data["agent_id"] = agent_id
        if transcript_path:
            data["transcript_path"] = transcript_path

        try:
            success = save_instance(instance_name, data)

            # Log creation event
            if success:
                try:
                    from .db import log_event

                    # Determine who launched this instance
                    launcher = os.environ.get("HCOM_LAUNCHED_BY", "unknown")
                    is_hcom_launched = os.environ.get("HCOM_LAUNCHED") == "1"

                    log_event(
                        "life",
                        instance_name,
                        {
                            "action": "created",
                            "by": launcher,
                            "is_hcom_launched": is_hcom_launched,
                            "is_subagent": bool(parent_session_id),
                            "parent_name": parent_name or "",
                        },
                    )
                except Exception as e:
                    from .log import log_error

                    log_error("core", "db.error", e, op="initialize_instance")

                # Auto-subscribe to event presets from config
                try:
                    from .ops import auto_subscribe_defaults

                    auto_subscribe_defaults(instance_name, tool or "claude")
                except Exception:
                    pass

            return success
        except sqlite3.IntegrityError:
            # UNIQUE constraint violation - paranoid safety net for hash collision TOCTOU
            # (Another process won the INSERT race after both checked DB. Astronomically rare.)
            # Safe to treat as success since instance exists with our intended name
            return True
    except Exception:
        return False


# ==================== PTY Helper Functions ====================
# These were moved from pty/pty_common.py to keep PTY layer thin


def wait_for_process_registration(
    process_id: str,
    timeout: int = PROCESS_REGISTRATION_TIMEOUT,
    log_fn: Any = None,
    require_session_id: bool = True,
) -> tuple[str | None, dict | None]:
    """Wait for process binding to resolve to a session-bound instance.

    Args:
        process_id: HCOM_PROCESS_ID (pre-generated by launcher)
        timeout: Max seconds to wait
        log_fn: Optional logging function
        require_session_id: If True, wait until session_id is set (default).
            If False, return as soon as instance_name is available.

    Returns:
        (instance_name, instance) or (None, None) on timeout
    """
    from .db import get_instance, get_process_binding

    last_status = "no_binding"
    last_instance_name = None

    for i in range(timeout):
        binding = get_process_binding(process_id)
        instance_name = binding.get("instance_name") if binding else None
        if instance_name:
            last_instance_name = instance_name
            instance = get_instance(instance_name)
            if instance:
                if instance.get("session_id") or not require_session_id:
                    return instance_name, instance
                last_status = "waiting_session_id"
                if log_fn and i % 5 == 0:  # Log every 5 seconds
                    log_fn(
                        f"Instance {instance_name} pre-registered, waiting for session_id..."
                    )
            else:
                last_status = "instance_not_found"
        else:
            if log_fn and i % 5 == 0:  # Log every 5 seconds
                log_fn("Waiting for process binding registration...")
        time.sleep(1.0)

    # Timeout - provide diagnostic info
    if log_fn:
        if last_status == "waiting_session_id":
            log_fn(
                f"TIMEOUT: Instance {last_instance_name} found but session_id never set. "
                "SessionStart hook may have failed."
            )
        elif last_status == "instance_not_found":
            log_fn(
                f"TIMEOUT: Process binding found instance {last_instance_name} but "
                "instance not in DB. Launcher may have failed."
            )
        else:
            log_fn(
                f"TIMEOUT: Process binding {process_id[:8]}... never registered. "
                "Launcher or SessionStart hook may have failed."
            )

    return None, None


def get_instance_cursor(instance_name: str) -> int:
    """Get current cursor position (last_event_id) for instance.

    Used by PTY modules to track delivery confirmation:
    - Snapshot cursor before inject
    - If cursor advances after inject, message was delivered/read

    Returns:
        Current last_event_id, or 0 if not found
    """
    data = load_instance_position(instance_name)
    return data.get("last_event_id", 0) if data else 0


__all__ = [
    # Type definitions
    "ToolType",
    "StatusType",
    "RunningTasks",
    "InstanceData",
    "ProcessBinding",
    "InstanceStatus",
    # Instance I/O
    "load_instance_position",
    "update_instance_position",
    # Instance predicates
    "is_parent_instance",
    "is_subagent_instance",
    "is_remote_instance",
    "is_external_sender",
    "is_launching_placeholder",
    # Cleanup
    "cleanup_stale_placeholders",
    "cleanup_stale_instances",
    # Status functions
    "get_instance_status",
    "get_status_description",
    "get_status_icon",
    "set_status",
    "set_gate_status",
    "parse_running_tasks",
    # Identity management
    "get_full_name",
    "generate_unique_name",
    "resolve_instance_name",
    "resolve_process_binding",
    "resolve_instance_from_binding",
    "bind_session_to_process",
    "initialize_instance_in_position_file",
    "capture_and_store_launch_context",
    # PTY helpers
    "wait_for_process_registration",
    "get_instance_cursor",
]
