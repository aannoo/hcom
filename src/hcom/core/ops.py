"""Core operations for HCOM.

Clean operational layer used by both CLI commands and Python API.
Raises HcomError on failure, returns meaningful data on success.
"""

from __future__ import annotations

import sqlite3

from .messages import MessageEnvelope
from ..shared import HcomError, SenderIdentity, ST_BLOCKED


def op_send(identity: SenderIdentity, message: str, envelope: MessageEnvelope | None = None) -> list[str]:
    """Send message.

    Args:
        identity: Sender identity (from resolve_identity or constructed)
        message: Message text (can include @mentions)
        envelope: Optional envelope fields {intent, reply_to, thread}

    Returns:
        List of instance names message was delivered to

    Raises:
        HcomError: If validation fails or delivery fails
    """
    from .messages import send_message

    return send_message(identity, message, envelope=envelope)


def op_stop(instance_name: str, initiated_by: str | None = None, reason: str = "api") -> None:
    """Stop an instance (deletes row).

    Args:
        instance_name: Instance to stop
        initiated_by: Who initiated the stop (for logging)
        reason: Reason for stop (for logging)

    Raises:
        HcomError: If instance not found or is remote
    """
    from .instances import load_instance_position
    from .tool_utils import stop_instance

    position = load_instance_position(instance_name)
    if not position:
        raise HcomError(f"Instance '{instance_name}' not found")

    if position.get("origin_device_id"):
        raise HcomError(f"Cannot stop remote instance '{instance_name}' via ops - use relay")

    stop_instance(instance_name, initiated_by=initiated_by or "api", reason=reason)


def op_start(instance_name: str, initiated_by: str | None = None, reason: str = "api") -> None:
    """Start an instance.

    With row-exists=participating model, stopped instances are deleted and
    cannot be restarted via this API. Use 'hcom start' command instead.

    Args:
        instance_name: Instance to start
        initiated_by: Who initiated the start (for logging)
        reason: Reason for start (for logging)

    Raises:
        HcomError: If instance not found or is remote
    """
    from .instances import load_instance_position

    position = load_instance_position(instance_name)
    if not position:
        raise HcomError(
            f"Instance '{instance_name}' not found. Stopped instances are deleted - use 'hcom start' to create a new one."
        )

    if position.get("origin_device_id"):
        raise HcomError(f"Cannot start remote instance '{instance_name}' via ops - use relay")

    # Row exists = already participating, nothing to do



def cleanup_instance_subscriptions(instance_name: str) -> int:
    """Remove all event subscriptions owned by an instance.

    Called during instance stop and before re-subscribing on creation.
    Subscriptions are stored as kv entries with key 'events_sub:sub-{hash}'
    and caller stored in the JSON value.

    Args:
        instance_name: Instance whose subscriptions to remove

    Returns:
        Number of subscriptions deleted
    """
    import json
    from .db import get_db, kv_set

    conn = get_db()
    deleted = 0

    try:
        rows = conn.execute("SELECT key, value FROM kv WHERE key LIKE 'events_sub:%'").fetchall()
        for row in rows:
            try:
                sub = json.loads(row["value"])
                if sub.get("caller") == instance_name:
                    kv_set(row["key"], None)  # Delete
                    deleted += 1
            except (json.JSONDecodeError, TypeError):
                pass
    except (json.JSONDecodeError, TypeError, sqlite3.Error):
        pass

    return deleted


def auto_subscribe_defaults(instance_name: str, tool: str) -> None:
    """Auto-subscribe instance to default event subscriptions from config.

    Called during instance creation. Only subscribes if tool supports collision
    detection (claude, gemini, codex). Cleans up any stale subscriptions first
    (from previously stopped instances with reused names).
    """
    if tool not in ("claude", "gemini", "codex"):
        return

    try:
        from .config import get_config
        from ..commands.events import _events_sub

        # Clean up stale subscriptions for this instance name (from reused names)
        cleanup_instance_subscriptions(instance_name)

        config = get_config()
        if not config.auto_subscribe:
            return

        presets = [p.strip() for p in config.auto_subscribe.split(",") if p.strip()]

        # Map old preset names to new composable filter flags
        preset_to_flags = {
            "collision": ["--collision"],
            "created": ["--action", "created"],
            "stopped": ["--action", "stopped"],
            ST_BLOCKED: ["--status", ST_BLOCKED],
        }

        for preset in presets:
            try:
                flags = preset_to_flags.get(preset)
                if not flags:
                    # Unknown preset - skip
                    continue
                _events_sub(flags, caller_name=instance_name, silent=True)
            except Exception:
                pass
    except Exception:
        pass


def load_stopped_snapshot(name: str) -> dict | None:
    """Load instance snapshot from most recent stopped life event.

    Used by both CLI resume and API launch(resume=name).
    """
    import json
    from .db import get_db

    row = get_db().execute(
        "SELECT json_extract(data, '$.snapshot') FROM events"
        " WHERE type='life' AND instance=? AND json_extract(data, '$.action')='stopped'"
        " ORDER BY id DESC LIMIT 1",
        (name,),
    ).fetchone()
    if row and row[0]:
        return json.loads(row[0])
    return None


def resume_system_prompt(name: str, *, fork: bool) -> str:
    """System prompt injected when resuming or forking an instance."""
    if fork:
        return (
            f"YOU ARE A FORK of agent '{name}'. "
            f"You have the same session history but are a NEW agent. "
            f"Run hcom start to get your own identity."
        )
    return f"YOUR SESSION HAS BEEN RESUMED! You are still '{name}'."


__all__ = [
    "op_send",
    "op_stop",
    "op_start",
    "auto_subscribe_defaults",
    "cleanup_instance_subscriptions",
    "load_stopped_snapshot",
    "resume_system_prompt",
]
