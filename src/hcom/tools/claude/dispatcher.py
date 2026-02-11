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
import re
import sys
from typing import Any, Final
from ...core.instances import load_instance_position
from ...core.log import log_error, log_info
from ...core.paths import ensure_hcom_directories
from . import hooks as parent, subagent
from ...hooks.utils import init_hook_context

# Hook type constants
HOOK_SESSIONSTART: Final[str] = "sessionstart"
HOOK_USERPROMPTSUBMIT: Final[str] = "userpromptsubmit"
HOOK_PRE: Final[str] = "pre"
HOOK_POST: Final[str] = "post"
HOOK_NOTIFY: Final[str] = "notify"
HOOK_SUBAGENT_START: Final[str] = "subagent-start"
HOOK_SUBAGENT_STOP: Final[str] = "subagent-stop"


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
    from ...core.hcom_context import HcomContext
    from ...core.hook_payload import HookPayload

    try:
        # Read stdin once at entry
        stdin_json = json.load(sys.stdin)

        # Build context from os.environ
        ctx = HcomContext.from_os()

        # Build normalized payload
        payload = HookPayload.from_claude(stdin_json, hook_type)

        # Dispatch via new code path
        result = handle_hook_with_context(hook_type, ctx, payload)

        # Output result (NOTE: result.hook_output intentionally not used - it's only
        # for testing/documentation. stdout contains the actual JSON sent to Claude.)
        if result.stdout:
            print(result.stdout, end="")
        if result.stderr:
            print(result.stderr, file=sys.stderr, end="")
        sys.exit(result.exit_code)
    except Exception as e:
        # Pre-gate error - must be silent (don't know if user is using hcom)
        log_error("hooks", "hook.error", e, hook=hook_type)
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

    When a vanilla Claude instance runs `hcom start`, it outputs [hcom:name].
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
        3. Search for [hcom:X] marker
        4. Create session binding and update instance metadata
    """
    from ...shared import BIND_MARKER_RE
    from ...core.db import get_pending_instances

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

    # Don't rebind if this session is already bound to a different instance
    if current_instance and current_instance != instance_name:
        return None

    if not session_id:
        return current_instance or instance_name

    try:
        from ...core.db import rebind_instance_session, get_instance
        from ...core.instances import update_instance_position

        # Verify instance exists and is actually pending binding (session_id IS NULL)
        inst = get_instance(instance_name)
        if not inst:
            log_error("hooks", "bind.fail", "instance not found", instance=instance_name)
            return None
        if inst.get("session_id"):
            # Already bound — marker is stale (e.g. from transcript output)
            return None

        rebind_instance_session(instance_name, session_id)
        log_info("hooks", "bind.session", instance=instance_name, session_id=session_id)

        # Update instance with session_id and mark as Claude (vanilla Claude binding)
        update_instance_position(instance_name, {"session_id": session_id, "tool": "claude"})

        return instance_name
    except Exception as e:
        log_error("hooks", "bind.fail", e)
        return None


# ============ DAEMON ENTRY POINT ============

# Type hints for daemon entry point (avoid circular imports at module level)
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from ...core.hcom_context import HcomContext
    from ...core.hook_payload import HookPayload
    from ...core.hook_result import HookResult


def handle_hook_with_context(
    hook_type: str,
    ctx: "HcomContext",
    payload: "HookPayload",
) -> "HookResult":
    """Daemon entry point for Claude hooks - context already built.

    Dispatches to refactored handle_* functions that accept ctx/payload directly.
    No os.environ mutation, no sys.stdin reads - fully thread-safe.

    Args:
        hook_type: Hook type (pre, post, sessionstart, poll, notify, etc.).
        ctx: Immutable execution context (replaces os.environ reads).
        payload: Normalized hook payload (replaces sys.stdin reads).

    Returns:
        HookResult with exit_code, stdout, stderr.

    Example:
        ctx = HcomContext.from_env(request.env, request.cwd)
        payload = HookPayload.from_claude(request.stdin_json, "pre")
        result = handle_hook_with_context("pre", ctx, payload)
    """
    from ...core.hook_result import HookResult

    try:
        return _dispatch_with_context(hook_type, ctx, payload)
    except Exception as e:
        log_error("hooks", "hook_with_context.error", e, hook=hook_type)
        return HookResult.error(str(e))


def _dispatch_with_context(
    hook_type: str,
    ctx: "HcomContext",
    payload: "HookPayload",
) -> "HookResult":
    """Internal dispatcher for daemon entry point - uses refactored handlers.

    Mirrors _handle_hook_impl() logic but uses ctx/payload throughout.
    """
    import time as _time

    dispatch_start = _time.perf_counter()

    from ...core.hook_result import HookResult
    from ...core.db import get_db
    from .hooks import get_real_session_id

    # Ensure directories and DB
    init_start = _time.perf_counter()
    if not ensure_hcom_directories():
        return HookResult.success()
    get_db()
    init_ms = (_time.perf_counter() - init_start) * 1000

    # Resolve session_id FIRST (get_real_session_id handles fork bug)
    # Must happen before SessionStart to ensure correct binding
    session_start = _time.perf_counter()
    hook_data = payload.raw.copy()
    session_id = get_real_session_id(hook_data, ctx.claude_env_file, is_fork=ctx.is_fork)
    hook_data["session_id"] = session_id
    session_ms = (_time.perf_counter() - session_start) * 1000

    # Update payload with corrected session_id (HookPayload is mutable)
    payload.session_id = session_id
    payload.raw["session_id"] = session_id

    # SessionStart is special - no instance resolution needed
    if hook_type == HOOK_SESSIONSTART:
        handler_start = _time.perf_counter()
        result = parent.handle_sessionstart(ctx, payload)
        handler_ms = (_time.perf_counter() - handler_start) * 1000
        total_ms = (_time.perf_counter() - dispatch_start) * 1000
        log_info("hooks", "dispatch.timing", hook=hook_type,
                 init_ms=round(init_ms, 2), session_ms=round(session_ms, 2),
                 handler_ms=round(handler_ms, 2), total_ms=round(total_ms, 2))
        return result

    # Task transitions
    tool_name = payload.tool_name or ""
    if hook_type == HOOK_PRE and tool_name == "Task":
        task_start = _time.perf_counter()
        updated_input = parent.start_task(session_id, hook_data)
        task_ms = (_time.perf_counter() - task_start) * 1000
        total_ms = (_time.perf_counter() - dispatch_start) * 1000
        log_info("hooks", "dispatch.timing", hook=hook_type, tool=tool_name,
                 init_ms=round(init_ms, 2), session_ms=round(session_ms, 2),
                 task_ms=round(task_ms, 2), total_ms=round(total_ms, 2))
        if updated_input:
            output = {
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "updatedInput": updated_input,
                }
            }
            import json
            return HookResult(exit_code=0, stdout=json.dumps(output))
        return HookResult.success()

    if hook_type == HOOK_POST and tool_name == "Task":
        task_start = _time.perf_counter()
        parent.end_task(session_id, hook_data, interrupted=False)
        task_ms = (_time.perf_counter() - task_start) * 1000
        total_ms = (_time.perf_counter() - dispatch_start) * 1000
        log_info("hooks", "dispatch.timing", hook=hook_type, tool=tool_name,
                 init_ms=round(init_ms, 2), session_ms=round(session_ms, 2),
                 task_ms=round(task_ms, 2), total_ms=round(total_ms, 2))
        return HookResult.success()

    # Subagent context handling
    subagent_check_start = _time.perf_counter()
    is_in_subagent_ctx = subagent.in_subagent_context(session_id)
    subagent_check_ms = (_time.perf_counter() - subagent_check_start) * 1000

    if is_in_subagent_ctx:
        if hook_type == HOOK_USERPROMPTSUBMIT:
            transcript_path = payload.transcript_path or ""
            subagent.cleanup_dead_subagents(session_id, transcript_path)

        if hook_type == HOOK_SUBAGENT_START:
            agent_id = payload.agent_id
            agent_type = payload.agent_type
            if agent_id and agent_type:
                subagent.track_subagent(session_id, agent_id, agent_type)
            subagent_output = subagent.subagent_start(hook_data)
            total_ms = (_time.perf_counter() - dispatch_start) * 1000
            log_info("hooks", "dispatch.timing", hook=hook_type,
                     init_ms=round(init_ms, 2), session_ms=round(session_ms, 2),
                     subagent_check_ms=round(subagent_check_ms, 2),
                     total_ms=round(total_ms, 2), context="subagent")
            if subagent_output:
                import json
                return HookResult(exit_code=0, stdout=json.dumps(subagent_output))
            return HookResult.success()

        if hook_type == HOOK_SUBAGENT_STOP:
            subagent.subagent_stop(hook_data)
            total_ms = (_time.perf_counter() - dispatch_start) * 1000
            log_info("hooks", "dispatch.timing", hook=hook_type,
                     init_ms=round(init_ms, 2), session_ms=round(session_ms, 2),
                     subagent_check_ms=round(subagent_check_ms, 2),
                     total_ms=round(total_ms, 2), context="subagent")
            return HookResult.success()

        if hook_type == HOOK_NOTIFY:
            total_ms = (_time.perf_counter() - dispatch_start) * 1000
            log_info("hooks", "dispatch.timing", hook=hook_type,
                     init_ms=round(init_ms, 2), session_ms=round(session_ms, 2),
                     subagent_check_ms=round(subagent_check_ms, 2),
                     total_ms=round(total_ms, 2), context="subagent")
            return HookResult.success()

        if hook_type in (HOOK_PRE, HOOK_POST) and tool_name == "Bash":
            tool_input = payload.tool_input
            command = tool_input.get("command", "") if isinstance(tool_input, dict) else ""
            name_value = _extract_name(command)
            if name_value and hook_type == HOOK_POST:
                subagent.posttooluse(hook_data, "", None)
            total_ms = (_time.perf_counter() - dispatch_start) * 1000
            log_info("hooks", "dispatch.timing", hook=hook_type, tool=tool_name,
                     init_ms=round(init_ms, 2), session_ms=round(session_ms, 2),
                     subagent_check_ms=round(subagent_check_ms, 2),
                     total_ms=round(total_ms, 2), context="subagent")
            return HookResult.success()

        if hook_type in (HOOK_PRE, HOOK_POST):
            total_ms = (_time.perf_counter() - dispatch_start) * 1000
            log_info("hooks", "dispatch.timing", hook=hook_type, tool=tool_name,
                     init_ms=round(init_ms, 2), session_ms=round(session_ms, 2),
                     subagent_check_ms=round(subagent_check_ms, 2),
                     total_ms=round(total_ms, 2), context="subagent")
            return HookResult.success()

    # Resolve instance for parent hooks
    resolve_start = _time.perf_counter()
    instance_name, updates, is_matched_resume = init_hook_context(ctx, hook_data, hook_type)
    resolve_ms = (_time.perf_counter() - resolve_start) * 1000

    # Vanilla binding for Bash post hook
    bind_ms = 0.0
    if hook_type == HOOK_POST and tool_name == "Bash":
        bind_start = _time.perf_counter()
        bound_name = _bind_vanilla_from_marker(hook_data, session_id, instance_name)
        bind_ms = (_time.perf_counter() - bind_start) * 1000
        if bound_name:
            instance_name = bound_name
            updates = updates or {}
            updates.setdefault("directory", str(ctx.cwd))
            if payload.transcript_path:
                updates.setdefault("transcript_path", payload.transcript_path)

    if not instance_name:
        total_ms = (_time.perf_counter() - dispatch_start) * 1000
        log_info("hooks", "dispatch.timing", hook=hook_type, tool=tool_name,
                 init_ms=round(init_ms, 2), session_ms=round(session_ms, 2),
                 resolve_ms=round(resolve_ms, 2), total_ms=round(total_ms, 2),
                 result="no_instance")
        return HookResult.success()

    instance_data = load_instance_position(instance_name)
    if not instance_data:
        total_ms = (_time.perf_counter() - dispatch_start) * 1000
        log_info("hooks", "dispatch.timing", hook=hook_type, tool=tool_name,
                 init_ms=round(init_ms, 2), session_ms=round(session_ms, 2),
                 resolve_ms=round(resolve_ms, 2), total_ms=round(total_ms, 2),
                 result="no_instance_data")
        return HookResult.success()

    from typing import cast
    instance_dict = cast(dict, instance_data)

    # Dispatch to refactored handlers
    handler_start = _time.perf_counter()
    match hook_type:
        case "pre":
            result = parent.handle_pretooluse(ctx, payload, instance_name)
        case "post":
            result = parent.handle_posttooluse(ctx, payload, instance_name, instance_dict, updates)
        case "poll":
            result = parent.handle_stop(ctx, payload, instance_name, instance_dict)
        case "notify":
            result = parent.handle_notify(ctx, payload, instance_name, updates)
        case "userpromptsubmit":
            result = parent.handle_userpromptsubmit(ctx, payload, instance_name, updates, instance_dict)
        case "sessionend":
            result = parent.handle_sessionend(ctx, payload, instance_name, updates)
        case _:
            result = HookResult.success()
    handler_ms = (_time.perf_counter() - handler_start) * 1000
    total_ms = (_time.perf_counter() - dispatch_start) * 1000

    log_info("hooks", "dispatch.timing", hook=hook_type, tool=tool_name,
             instance=instance_name, init_ms=round(init_ms, 2),
             session_ms=round(session_ms, 2), resolve_ms=round(resolve_ms, 2),
             bind_ms=round(bind_ms, 2), handler_ms=round(handler_ms, 2),
             total_ms=round(total_ms, 2), exit_code=result.exit_code)

    return result
