"""Subagent context hooks - handles hooks during Task tool execution.

When a parent Claude instance runs the Task tool, hooks continue to fire in the
parent's process but need to handle the subagent context. This module provides:

- Subagent context detection via parent's running_tasks JSON field
- SubagentStart/SubagentStop lifecycle hooks
- Interrupt detection (subagent dies without SubagentStop)
- Message polling for background Task subagents

Subagent Context Tracking:
    The parent's `running_tasks` field (JSON) tracks:
    - active: bool - True when inside Task tool execution
    - subagents: list - Array of {agent_id, agent_type} for spawned subagents

Hook Flow (Subagent):
    1. PreToolUse Task → parent.start_task() sets active=True
    2. SubagentStart → track_subagent() appends to subagents array
    3. SubagentStop → subagent_stop() polls messages, clears running_tasks
    4. PostToolUse Task → parent.end_task() delivers freeze messages

Background vs Foreground:
    - Foreground: Parent frozen while subagents are alive
    - Background (run_in_background=True): Parent continues while subagents are alive

Interrupt Handling:
    If Task is interrupted (user submits prompt), cleanup_dead_subagents()
    checks transcript for agent completion and marks interrupted subagents.
"""

from __future__ import annotations
from typing import Any
import sys
import json

from ...core.config import get_config
from ...core.instances import (
    load_instance_position,
    update_instance_position,
    set_status,
    parse_running_tasks,
)
from ...core.db import get_db

# ============ TASK CONTEXT TRACKING ============


def in_subagent_context(session_id_or_name: str) -> bool:
    """Check if session/instance is in subagent context (Task active).

    Uses database running_tasks.active field for cross-process detection.
    Task is active if running_tasks JSON has active=true.
    Note: Parent frozen only for foreground Tasks; background Tasks allow live bidirectional comms.

    Args:
        session_id_or_name: Either session_id (from hooks) or instance_name (from commands)
    """
    from ...core.db import get_session_binding

    conn = get_db()

    # Try as session_id first via binding (fast path for hooks)
    instance_name = get_session_binding(session_id_or_name)
    if not instance_name:
        # Try as instance_name (for commands)
        instance_name = session_id_or_name

    row = conn.execute("SELECT running_tasks FROM instances WHERE name = ? LIMIT 1", (instance_name,)).fetchone()

    if not row or not row["running_tasks"]:
        return False

    try:
        running_tasks = json.loads(row["running_tasks"])
        return running_tasks.get("active", False)
    except (json.JSONDecodeError, AttributeError):
        return False


def check_dead_subagents(transcript_path: str, running_tasks: dict, subagent_timeout: int | None = None) -> list[str]:
    """Detect dead subagents by checking multiple death signals.

    Called by UserPromptSubmit after user interrupts a Task. To clean up orphaned
    subagents that won't receive a SubagentStop hook.

    Death signals checked:
    1. Instance deleted from DB (SubagentStop cleanup already ran)
    2. No transcript file exists
    3. Transcript stale (no writes for 2x timeout)
    4. Transcript contains "[Request interrupted by user]" marker

    Args:
        transcript_path: Parent's transcript path (subagent transcripts are siblings)
        running_tasks: Parent's running_tasks dict with subagents array
        subagent_timeout: Parent's override timeout, or None for global config

    Returns:
        List of dead subagent agent_ids to clean up.
    """
    from pathlib import Path
    import time

    dead = []
    transcript_dir = Path(transcript_path).parent if transcript_path else None
    conn = get_db()
    # Subagent dead if transcript unchanged for 2x timeout (session ended before SubagentStop cleanup)
    timeout = subagent_timeout if subagent_timeout is not None else get_config().subagent_timeout
    stale_threshold = timeout * 2

    for subagent in running_tasks.get("subagents", []):
        agent_id = subagent.get("agent_id")
        if not agent_id:
            continue

        # Rare: instance not in DB but still in running_tasks (SubagentStop cleanup failed)
        row = conn.execute("SELECT 1 FROM instances WHERE agent_id = ?", (agent_id,)).fetchone()
        if not row:
            dead.append(agent_id)
            continue

        if not transcript_dir:
            dead.append(agent_id)
            continue

        agent_transcript = transcript_dir / f"agent-{agent_id}.jsonl"
        try:
            if not agent_transcript.exists():
                dead.append(agent_id)
                continue

            # Stale: transcript not modified in 2x timeout = session ended without cleanup
            mtime = agent_transcript.stat().st_mtime
            if time.time() - mtime > stale_threshold:
                dead.append(agent_id)
                continue

            # Check last 4KB for interrupt marker
            with open(agent_transcript, "rb") as f:
                f.seek(0, 2)
                size = f.tell()
                f.seek(max(0, size - 4096))
                tail = f.read().decode("utf-8", errors="ignore")
                if "[Request interrupted by user]" in tail:
                    dead.append(agent_id)
        except Exception:
            dead.append(agent_id)  # Can't read = assume dead

    return dead


def cleanup_dead_subagents(session_id: str, transcript_path: str) -> None:
    """Check and remove dead subagents from running_tasks

    Called by UserPromptSubmit when in subagent context.
    """
    from ...core.db import get_session_binding

    instance_name = get_session_binding(session_id)
    if not instance_name:
        return

    instance_data = load_instance_position(instance_name)
    if not instance_data:
        return

    running_tasks = parse_running_tasks(instance_data.get("running_tasks", ""))
    if not running_tasks.get("subagents"):
        return

    # Pass parent's subagent_timeout override to check_dead_subagents
    dead_ids = check_dead_subagents(transcript_path, running_tasks, instance_data.get("subagent_timeout"))  # type: ignore[arg-type]
    if not dead_ids:
        return

    # Remove dead subagents
    for agent_id in dead_ids:
        _remove_subagent_from_parent(instance_name, agent_id)
        # Also stop the subagent instance if it exists
        conn = get_db()
        row = conn.execute("SELECT name FROM instances WHERE agent_id = ?", (agent_id,)).fetchone()
        if row:
            from ...core.tool_utils import stop_instance

            set_status(row["name"], "inactive", "exit:interrupted")
            stop_instance(row["name"], initiated_by="system", reason="interrupted")


def track_subagent(parent_session_id: str, agent_id: str, agent_type: str) -> None:
    """Track subagent in parent's running_tasks.subagents array

    Appends {agent_id, type} to parent's running_tasks.subagents array.
    """
    from ...core.db import get_session_binding
    from ...core.log import log_info

    log_info(
        "hooks",
        "track_subagent.enter",
        session_id=parent_session_id,
        agent_id=agent_id,
        agent_type=agent_type,
    )

    instance_name = get_session_binding(parent_session_id)
    if not instance_name:
        log_info("hooks", "track_subagent.no_binding", session_id=parent_session_id)
        return

    log_info("hooks", "track_subagent.resolved", parent=instance_name, agent_id=agent_id)

    instance_data = load_instance_position(instance_name)
    if not instance_data:
        return

    # Load existing running_tasks structure
    running_tasks = parse_running_tasks(instance_data.get("running_tasks", ""))
    running_tasks["active"] = True  # Ensure active flag is set

    # Add subagent if not already tracked
    subagents = running_tasks["subagents"]
    if not any(s.get("agent_id") == agent_id for s in subagents):
        subagents.append({"agent_id": agent_id, "type": agent_type})
        update_instance_position(instance_name, {"running_tasks": json.dumps(running_tasks)})


def _remove_subagent_from_parent(parent_name: str, agent_id: str) -> None:
    """Remove subagent from parent's running_tasks.subagents array

    Called when subagent exits (SubagentStop).
    Sets active=False when last subagent removed (enables parallel Task support).
    """
    parent_data = load_instance_position(parent_name)
    if not parent_data:
        return

    # Load existing running_tasks structure
    running_tasks = parse_running_tasks(parent_data.get("running_tasks", ""))
    if not running_tasks.get("subagents"):
        return

    # Remove subagent with matching agent_id
    running_tasks["subagents"] = [s for s in running_tasks["subagents"] if s.get("agent_id") != agent_id]

    # If no more subagents, clear active flag
    if not running_tasks["subagents"]:
        running_tasks["active"] = False

    # Update parent
    update_instance_position(parent_name, {"running_tasks": json.dumps(running_tasks)})


# ============ HOOK HANDLERS ============


def posttooluse(
    hook_data: dict[str, Any],
    _instance_name: str,
    _instance_data: dict[str, Any] | None,
) -> None:
    """Subagent PostToolUse: pull remote events, external stop notification, message delivery

    Handles subagents running hcom commands (identified by --name in Bash command).
    """
    import re
    from ...core.db import get_db
    from ...core.messages import get_unread_messages, format_messages_json

    tool_name = hook_data.get("tool_name", "")
    tool_input = hook_data.get("tool_input", {})

    # Only handle Bash commands with --name flag
    if tool_name != "Bash":
        sys.exit(0)

    command = tool_input.get("command", "")
    if "--name" not in command:
        sys.exit(0)

    # Extract name_value and resolve subagent
    match = re.search(r"--name\s+(\S+)", command)
    if not match:
        sys.exit(0)

    agent_id = match.group(1)
    conn = get_db()
    row = conn.execute("SELECT name FROM instances WHERE agent_id = ?", (agent_id,)).fetchone()
    if not row:
        sys.exit(0)

    subagent_name = row["name"]
    subagent_data = load_instance_position(subagent_name)
    if not subagent_data:
        # Row doesn't exist = not participating
        sys.exit(0)

    # Row exists = participating, proceed with message delivery

    # Pull remote events (rate-limited)
    try:
        from ...relay import is_relay_handled_by_daemon, pull

        if not is_relay_handled_by_daemon():
            pull()
    except Exception as e:
        # Best-effort sync - log for debugging
        from ...core.log import log_error

        log_error("relay", "relay.error", e, hook="subagent_posttooluse")

    outputs = []

    # Message delivery (like parent PostToolUse)
    from ...shared import MAX_MESSAGES_PER_DELIVERY

    messages, max_event_id = get_unread_messages(subagent_name, update_position=False)
    if messages:
        deliver_messages = messages[:MAX_MESSAGES_PER_DELIVERY]
        delivered_last_event_id = deliver_messages[-1].get("event_id", max_event_id)
        update_instance_position(subagent_name, {"last_event_id": delivered_last_event_id})

        formatted = format_messages_json(deliver_messages, subagent_name)
        set_status(
            subagent_name,
            "active",
            f"deliver:{deliver_messages[0]['from']}",
            msg_ts=deliver_messages[-1]["timestamp"],
        )
        outputs.append(
            {
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUse",
                    "additionalContext": formatted,
                }
            }
        )

    # Combine outputs if multiple
    if outputs:
        if len(outputs) == 1:
            print(json.dumps(outputs[0], ensure_ascii=False))
        else:
            contexts = [o["hookSpecificOutput"]["additionalContext"] for o in outputs if "hookSpecificOutput" in o]
            combined = {
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUse",
                    "additionalContext": "\n\n---\n\n".join(contexts),
                }
            }
            print(json.dumps(combined, ensure_ascii=False))

    sys.exit(0)


def subagent_start(hook_data: dict[str, Any]) -> dict | None:
    """SubagentStart: Surface agent_id to subagent.

    Returns:
        Output dict to send to Claude, or None if no output needed.
    """
    from ...core.log import log_info

    agent_id = hook_data.get("agent_id")
    log_info("hooks", "subagent_start.enter", agent_id=agent_id)

    if not agent_id:
        log_info("hooks", "subagent_start.no_agent_id")
        return None

    hint = f"Your agent ID: {agent_id}"
    # hint += f"Before any hcom commands, run: {hcom_cmd} start --name {agent_id}\n"
    # hint += f"Then use --name {agent_id} on all commands (required for identity routing)."

    return {
        "hookSpecificOutput": {
            "hookEventName": "SubagentStart",
            "additionalContext": hint,
        }
    }


def subagent_stop(hook_data: dict[str, Any]) -> None:
    """SubagentStop: Message polling using agent_id (lazy creation pattern)"""
    # Extract agent_id
    agent_id = hook_data.get("agent_id")
    if not agent_id:
        sys.exit(0)

    # Query for subagent by agent_id (stored when subagent ran hcom start)
    conn = get_db()
    row = conn.execute(
        "SELECT name, transcript_path, parent_name FROM instances WHERE agent_id = ?",
        (agent_id,),
    ).fetchone()

    if not row:
        # No instance = subagent never ran hcom start.
        # Still remove from parent's running_tasks to prevent stuck active state.
        from ...core.db import get_session_binding

        parent_name = get_session_binding(hook_data.get("session_id", ""))
        if parent_name:
            _remove_subagent_from_parent(parent_name, agent_id)
        sys.exit(0)

    subagent_id = row["name"]
    parent_name = row["parent_name"]

    # Store transcript_path if not already set
    if not row["transcript_path"]:
        transcript_path = hook_data.get("agent_transcript_path")
        if transcript_path:
            update_instance_position(subagent_id, {"transcript_path": transcript_path})

    # Poll messages using shared helper
    # Resolve timeout: parent instance override > global config
    timeout = None
    if parent_name:
        parent_data = load_instance_position(parent_name)
        if parent_data:
            timeout = parent_data.get("subagent_timeout")
    if timeout is None:
        timeout = get_config().subagent_timeout
    from ...hooks.family import poll_messages

    exit_code, output, timed_out = poll_messages(
        subagent_id,
        timeout,
    )

    if output:
        print(json.dumps(output, ensure_ascii=False))

    # exit_code=2: message delivered, subagent continues processing, SubagentStop fires again
    # exit_code=0: no message/timeout, stop and cleanup
    if exit_code == 0:
        from ...core.tool_utils import stop_instance

        reason = "timeout" if timed_out else "task_completed"
        set_status(subagent_id, "inactive", f"exit:{reason}")
        if parent_name:
            _remove_subagent_from_parent(parent_name, agent_id)
        stop_instance(subagent_id, initiated_by="subagent", reason=reason)

    sys.exit(exit_code)
