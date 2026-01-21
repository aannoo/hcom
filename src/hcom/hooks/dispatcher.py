"""Hook dispatcher - single entry point for all Claude Code hooks.

This module routes incoming hooks to the appropriate handler based on:
1. Hook type (sessionstart, pre, post, poll, notify, userpromptsubmit, etc.)
2. Context (parent instance vs subagent context)

Routing Logic:
    ┌─────────────────────────────────────────────────────────────────┐
    │                     handle_hook(hook_type)                       │
    ├─────────────────────────────────────────────────────────────────┤
    │  1. Parse hook_data from stdin (JSON)                           │
    │  2. Extract/correct session_id (workaround for CC fork bug)     │
    │  3. Check for Task tool transitions (pre/post Task)             │
    │  4. Detect subagent context via running_tasks.active            │
    │  5. Route to parent.* or subagent.* handlers                    │
    └─────────────────────────────────────────────────────────────────┘

Hook Types:
    sessionstart      - Session lifecycle start
    userpromptsubmit  - User/system prompt submitted (message delivery)
    pre               - PreToolUse (status tracking, Task start)
    post              - PostToolUse (message delivery, Task end, vanilla binding)
    poll              - Stop hook (idle polling for messages)
    notify            - Notification (blocked status)
    subagent-start    - SubagentStart (track new subagent)
    subagent-stop     - SubagentStop (subagent message polling)
    sessionend        - Session lifecycle end

Error Strategy:
    - Non-participants (no instance row): exit 0 silently
    - Participants (row exists): errors surface for debugging
    - Pre-gate errors: always silent (user may not be using hcom)
"""

from __future__ import annotations

import json
import os
import re
import sys
from pathlib import Path
from typing import Any, Final, cast

from ..core.db import get_db
from ..core.instances import load_instance_position
from ..core.log import log_error, log_info
from ..core.paths import ensure_hcom_directories
from . import parent, subagent
from .utils import init_hook_context

# Hook type constants
HOOK_SESSIONSTART: Final[str] = "sessionstart"
HOOK_USERPROMPTSUBMIT: Final[str] = "userpromptsubmit"
HOOK_PRE: Final[str] = "pre"
HOOK_POST: Final[str] = "post"
HOOK_POLL: Final[str] = "poll"
HOOK_NOTIFY: Final[str] = "notify"
HOOK_SUBAGENT_START: Final[str] = "subagent-start"
HOOK_SUBAGENT_STOP: Final[str] = "subagent-stop"
HOOK_SESSIONEND: Final[str] = "sessionend"


def handle_hook(hook_type: str) -> None:
    """Main entry point for all Claude Code hooks. Routes to appropriate handler.

    Called by Claude Code via: hcom <hook_type>
    Hook data is read from stdin as JSON.

    Args:
        hook_type: One of: sessionstart, userpromptsubmit, pre, post, poll,
                   notify, subagent-start, subagent-stop, sessionend

    Exit Codes:
        0 - Normal exit (non-participant or completed successfully)
        2 - Message delivered (Stop hook only, signals Claude to continue)

    Error Handling:
        Pre-gate errors (before instance resolved): exit 0 silently to avoid
        leaking errors into normal Claude usage when hcom installed but not used.
        Post-gate errors (participant): logged for debugging.
    """
    try:
        _handle_hook_impl(hook_type)
    except Exception as e:
        # Pre-gate error - must be silent (don't know if user is using hcom)
        log_error("hooks", "hook.error", e, hook=hook_type)
        sys.exit(0)


def _handle_hook_impl(hook_type: str) -> None:
    """Internal hook dispatcher implementation.

    Routing order:
        1. Task tool transitions (pre/post Task) - always handled first
        2. Subagent context hooks (if running_tasks.active=True)
        3. Parent instance hooks (default path)

    The subagent context is detected via the parent's running_tasks JSON field
    which tracks active Task tools and their spawned subagents.
    """

    # ============ SETUP, LOAD, SYNC (BOTH CONTEXTS) ============
    # Note: Permission approval now handled via settings.json permissions.allow
    # (see hooks/settings.py CLAUDE_HCOM_PERMISSIONS)

    hook_data: dict[str, Any] = json.load(sys.stdin)
    tool_name: str = hook_data.get("tool_name", "")

    # Debug: log all hook invocations to trace Task handling
    log_info("hooks", "dispatcher.entry", hook=hook_type, tool=tool_name or "(none)")

    # Get real session_id from CLAUDE_ENV_FILE path (workaround for CC fork bug)
    # CC passes wrong session_id in hook_data for --fork-session scenarios
    from .parent import get_real_session_id

    env_file: str | None = os.environ.get("CLAUDE_ENV_FILE")
    original_session_id: str = hook_data.get("session_id") or hook_data.get("sessionId", "")
    session_id: str = get_real_session_id(hook_data, env_file)

    log_info(
        "hooks",
        "dispatcher.session_id_resolution",
        hook=hook_type,
        original_session_id=original_session_id,
        resolved_session_id=session_id,
        env_file=env_file,
        process_id=os.environ.get("HCOM_PROCESS_ID"),
        changed=original_session_id != session_id,
    )

    # Store corrected session_id back into hook_data for downstream functions
    hook_data["session_id"] = session_id
    # Keep original for dual-bind in resume scenarios
    hook_data["original_session_id"] = original_session_id

    if not ensure_hcom_directories():
        log_error("hooks", "hook.error", "failed to create directories")
        sys.exit(0)

    get_db()

    # ============ TASK TRANSITIONS (PARENT CONTEXT) ============

    # Task start - enter subagent context and inject hcom hint into prompt
    if hook_type == HOOK_PRE and tool_name == "Task":
        log_info("hooks", "dispatcher.task_pre", session_id=session_id)
        updated_input: dict[str, Any] | None = parent.start_task(session_id, hook_data)
        if updated_input:
            output: dict[str, Any] = {
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "updatedInput": updated_input,
                }
            }
            print(json.dumps(output))
        sys.exit(0)

    # Task end - deliver freeze messages (SubagentStop handles cleanup)
    if hook_type == HOOK_POST and tool_name == "Task":
        parent.end_task(session_id, hook_data, interrupted=False)
        sys.exit(0)

    # ============ SUBAGENT CONTEXT HOOKS ============

    is_in_subagent_ctx: bool = subagent.in_subagent_context(session_id)
    log_info(
        "hooks",
        "dispatcher.subagent_check",
        hook=hook_type,
        session_id=session_id,
        in_subagent_context=is_in_subagent_ctx,
    )

    # Log when SubagentStart is skipped due to no subagent context
    if hook_type == HOOK_SUBAGENT_START and not is_in_subagent_ctx:
        log_info(
            "hooks",
            "dispatcher.subagent_start_skipped",
            session_id=session_id,
            reason="not_in_subagent_context",
        )

    if is_in_subagent_ctx:
        # UserPromptSubmit: check for dead subagents (interrupt detection)
        if hook_type == HOOK_USERPROMPTSUBMIT:
            transcript_path: str = hook_data.get("transcript_path", "")
            subagent.cleanup_dead_subagents(session_id, transcript_path)
            # Fall through to parent handling

        # SubagentStart/SubagentStop: have agent_id in payload
        match hook_type:
            case "subagent-start":
                agent_id: str | None = hook_data.get("agent_id")
                agent_type: str | None = hook_data.get("agent_type")
                log_info(
                    "hooks",
                    "dispatcher.subagent_start",
                    agent_id=agent_id,
                    agent_type=agent_type,
                    session_id=session_id,
                    is_in_ctx=is_in_subagent_ctx,
                )
                if agent_id and agent_type:
                    subagent.track_subagent(session_id, agent_id, agent_type)
                subagent.subagent_start(hook_data)
                sys.exit(0)
            case "subagent-stop":
                subagent.subagent_stop(hook_data)
                sys.exit(0)

        # Pre/Post: require explicit --name
        if hook_type in (HOOK_PRE, HOOK_POST) and tool_name == "Bash":
            tool_input: dict[str, Any] = hook_data.get("tool_input", {})
            command: str = tool_input.get("command", "")
            name_value: str | None = _extract_name(command)

            if name_value:
                # Identified subagent
                if hook_type == HOOK_POST:
                    subagent.posttooluse(hook_data, "", None)
                sys.exit(0)
            else:
                # No identity - skip silently
                sys.exit(0)
        elif hook_type in (HOOK_PRE, HOOK_POST):
            # Non-Bash pre/post during subagent context: skip
            sys.exit(0)

        # Other hooks (poll, notify, sessionend) fall through to parent

    # ============  PARENT INSTANCE HOOKS ============

    if hook_type == HOOK_SESSIONSTART:
        parent.sessionstart(hook_data)
        sys.exit(0)

    # Resolve instance for parent hooks
    instance_name: str | None
    updates: dict[str, Any] | None
    is_matched_resume: bool
    instance_name, updates, is_matched_resume = init_hook_context(hook_data, hook_type)

    # Vanilla binding: parse [HCOM:BIND:X] marker from PostToolUse Bash output
    if hook_type == HOOK_POST and tool_name == "Bash":
        bound_name: str | None = _bind_vanilla_from_marker(hook_data, session_id, instance_name)
        if bound_name:
            instance_name = bound_name
            updates = updates or {}
            updates.setdefault("directory", str(Path.cwd()))
            binding_transcript_path: str = hook_data.get("transcript_path", "")
            if binding_transcript_path:
                updates.setdefault("transcript_path", binding_transcript_path)

    if not instance_name:
        sys.exit(0)
    instance_data = load_instance_position(instance_name)  # InstanceData | dict[str, Any]

    # Participation gate: row exists = participating
    if not instance_data:
        sys.exit(0)

    # Cast InstanceData to dict for hook handlers (TypedDict is compatible with dict at runtime)
    instance_dict = cast(dict[str, Any], instance_data)

    match hook_type:
        case "pre":
            parent.pretooluse(hook_data, instance_name, tool_name)
        case "post":
            parent.posttooluse(hook_data, instance_name, instance_dict, updates)
        case "poll":
            parent.stop(instance_name, instance_dict)
        case "notify":
            parent.notify(hook_data, instance_name, updates, instance_dict)
        case "userpromptsubmit":
            parent.userpromptsubmit(hook_data, instance_name, updates, is_matched_resume, instance_dict)
        case "sessionend":
            parent.sessionend(hook_data, instance_name, updates)

    sys.exit(0)


def _extract_name(command: str) -> str | None:
    """Extract --name flag value from a bash command string.

    Used to identify subagent hcom commands which require explicit --name.

    Args:
        command: Bash command string (e.g., "hcom send --name abc123 'hello'")

    Returns:
        The name/agent_id value if --name flag present, None otherwise.

    Example:
        >>> _extract_name("hcom send --name luna 'hello'")
        'luna'
        >>> _extract_name("hcom list")
        None
    """
    match = re.search(r"--name\s+(\S+)", command)
    if match:
        return match.group(1)
    return None


def _bind_vanilla_from_marker(hook_data: dict[str, Any], session_id: str, current_instance: str | None) -> str | None:
    """Detect and process vanilla instance binding from `hcom start` output.

    When a vanilla Claude instance runs `hcom start`, it outputs [HCOM:BIND:name].
    This function parses that marker from PostToolUse Bash output and creates
    the session binding that enables hook participation.

    Args:
        hook_data: PostToolUse hook data containing tool_response
        session_id: Current Claude session ID
        current_instance: Already-resolved instance name, if any

    Returns:
        Instance name if successfully bound, None otherwise.

    Flow:
        1. Check for pending instances (optimization - skip if none)
        2. Extract tool_response from hook_data
        3. Search for [HCOM:BIND:X] marker
        4. Create session binding and update instance metadata
    """
    from ..shared import BIND_MARKER_RE
    from ..core.db import get_pending_instances

    # Skip if no pending instances (optimization)
    if not get_pending_instances():
        return None

    tool_response = hook_data.get("tool_response", "")
    if not tool_response:
        return None

    # tool_response can be dict with stdout/stderr or string
    if isinstance(tool_response, dict):
        tool_response = tool_response.get("stdout", "")
    if not tool_response:
        return None

    # Search for binding marker in tool output
    match = BIND_MARKER_RE.search(tool_response)
    if not match:
        return None

    instance_name = match.group(1)

    if not session_id:
        return current_instance or instance_name

    try:
        from ..core.db import rebind_instance_session, get_instance
        from ..core.instances import update_instance_position

        # Verify instance exists
        if not get_instance(instance_name):
            log_error("hooks", "bind.fail", "instance not found", instance=instance_name)
            return None

        rebind_instance_session(instance_name, session_id)
        log_info("hooks", "bind.session", instance=instance_name, session_id=session_id)

        # Update instance with session_id and mark as Claude (vanilla Claude binding)
        update_instance_position(instance_name, {"session_id": session_id, "tool": "claude"})

        return instance_name
    except Exception as e:
        log_error("hooks", "bind.fail", e)
        return None
