"""Shared hook handler logic for all tools (Claude, Gemini, Codex).

Extracts duplicated patterns from parent.py and tool-specific hook files.
"""

from __future__ import annotations

from typing import Any


def deliver_pending_messages(instance_name: str) -> tuple[list[dict[str, Any]], str | None]:
    """Fetch unread messages, update position, set delivery status.

    Returns:
        (delivered_messages, formatted_json) — empty list and None if no messages.
        Callers that need additional formatting (e.g. Claude's format_hook_messages)
        can use the returned messages list.
    """
    from ..core.messages import get_unread_messages, format_messages_json
    from ..core.instances import update_instance_position, set_status
    from ..shared import MAX_MESSAGES_PER_DELIVERY

    messages, max_event_id = get_unread_messages(instance_name, update_position=False)
    if not messages:
        return [], None

    deliver = messages[:MAX_MESSAGES_PER_DELIVERY]
    last_id = deliver[-1].get("event_id", max_event_id)
    # Cursor advances before format — safe because format is stdlib string ops on
    # data that just came from SQLite (won't throw). Rust delivery loop retries if
    # this hook process dies before returning (does it?).
    update_instance_position(instance_name, {"last_event_id": last_id})

    formatted = format_messages_json(deliver, instance_name)
    set_status(
        instance_name,
        "active",
        f"deliver:{deliver[0]['from']}",
        msg_ts=deliver[-1].get("timestamp", ""),
    )
    return deliver, formatted


def finalize_session(instance_name: str, reason: str, updates: dict[str, Any] | None = None) -> None:
    """Set inactive status, persist updates, and stop instance.

    Common to Claude and Gemini SessionEnd handlers.
    """
    from ..core.instances import set_status, update_instance_position
    from ..core.tool_utils import stop_instance
    from ..core.log import log_error

    set_status(instance_name, "inactive", f"exit:{reason}")

    try:
        if updates:
            update_instance_position(instance_name, updates)
    except Exception as e:
        log_error("hooks", "hook.error", e, hook="sessionend", instance=instance_name)

    stop_instance(instance_name, initiated_by="session", reason=f"exit:{reason}")


def update_tool_status(instance_name: str, tool: str, tool_name: str, tool_input: dict[str, Any]) -> None:
    """Update instance status for tool execution.

    Calls extract_tool_detail for tool-specific detail formatting,
    then sets status to active with tool context.
    """
    from ..hooks.family import extract_tool_detail
    from ..core.instances import set_status

    detail = extract_tool_detail(tool, tool_name, tool_input)
    set_status(instance_name, "active", f"tool:{tool_name}", detail=detail)
