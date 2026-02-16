"""Gemini CLI hook handlers for hcom.

Hook handlers called by Gemini CLI at various lifecycle points.
Each handler reads JSON from stdin (hook payload) and may output
JSON to stdout (hook response with additionalContext).

Hook Lifecycle:
    SessionStart → BeforeAgent → [BeforeTool → AfterTool]* → AfterAgent → SessionEnd

Hooks and Responsibilities:
    SessionStart: Bind session to hcom identity, set terminal title
    BeforeAgent: Inject bootstrap on first prompt, deliver pending messages
    AfterAgent: Set status to listening (idle), notify subscribers
    BeforeTool: Track tool execution status (tool:X context)
    AfterTool: Deliver messages, handle vanilla instance binding
    Notification: Track approval prompts (blocked status)
    SessionEnd: Stop instance, log exit reason

Identity Resolution:
    - HCOM-launched: HCOM_PROCESS_ID env var → process binding → instance
    - Vanilla: transcript search for [hcom:name] marker after hcom start

Message Delivery:
    Messages are delivered via additionalContext in hook response JSON.
    Gemini displays this to the model as system context.
"""

from __future__ import annotations

import json

import sys
from typing import TYPE_CHECKING

from ...core.log import log_error, log_info
from ...shared import ST_ACTIVE, ST_BLOCKED, ST_LISTENING

if TYPE_CHECKING:
    from ...core.hcom_context import HcomContext
    from ...core.hook_payload import HookPayload
    from ...core.hook_result import HookResult


# =============================================================================
# Helper Functions
# =============================================================================


def try_capture_transcript_path(
    instance_name: str,
    payload: "HookPayload",
) -> None:
    """Try to capture transcript_path from payload if not already set.

    Gemini's ChatRecordingService isn't initialized at SessionStart,
    so transcript_path is empty. It becomes available at BeforeAgent/AfterAgent.
    This opportunistically captures it when available.

    Args:
        instance_name: The instance name to update.
        payload: Normalized hook payload.
    """
    from ...core.instances import load_instance_position, update_instance_position
    from ...core.transcript import derive_gemini_transcript_path

    data = load_instance_position(instance_name)
    if data and data.get("transcript_path"):
        return

    transcript_path = payload.transcript_path or ""

    # If not in payload, try deriving from session_id
    if not transcript_path:
        session_id = data.get("session_id", "") if data else ""
        if session_id:
            transcript_path = derive_gemini_transcript_path(session_id) or ""

    if transcript_path:
        update_instance_position(instance_name, {"transcript_path": transcript_path})


def resolve_instance_gemini(
    ctx: "HcomContext",  # noqa: ARG001 - kept for API consistency
    payload: "HookPayload",
) -> dict | None:
    """Resolve instance using process binding or session binding.

    Args:
        ctx: Execution context with process_id.
        payload: Normalized hook payload with session_id, transcript_path.

    Returns:
        Instance dict or None if not found.
    """
    from ...core.instances import resolve_instance_from_binding

    return resolve_instance_from_binding(
        session_id=payload.session_id,
        transcript_path=payload.transcript_path,
    )


def _bind_vanilla_instance(
    ctx: "HcomContext",  # noqa: ARG001 - kept for API consistency
    payload: "HookPayload",
) -> str | None:
    """Bind vanilla Gemini instance by parsing tool_result for [hcom:X] marker.

    Args:
        ctx: Execution context.
        payload: Normalized hook payload with tool_name, tool_result.

    Returns:
        Instance name if bound, None otherwise.
    """
    from ...core.db import get_pending_instances

    # Skip if no pending instances (optimization)
    if not get_pending_instances():
        return None

    # Only check run_shell_command tool responses
    if payload.tool_name != "run_shell_command":
        return None

    tool_response = payload.tool_result or ""
    if not tool_response:
        return None

    from ...shared import BIND_MARKER_RE

    match = BIND_MARKER_RE.search(tool_response)
    if not match:
        return None

    from ...hooks.family import bind_vanilla_instance

    return bind_vanilla_instance(
        instance_name=match.group(1),
        session_id=payload.session_id,
        transcript_path=payload.transcript_path,
        tool="gemini",
        hook="gemini-aftertool",
    )


# =============================================================================
# Handler Functions (ctx/payload pattern)
# =============================================================================


def handle_sessionstart(
    ctx: "HcomContext",
    payload: "HookPayload",
) -> "HookResult":
    """Handle Gemini SessionStart hook.

    HCOM-launched: bind session_id, inject bootstrap if not announced.
    Vanilla: show hcom hint.

    Args:
        ctx: Execution context (process_id, is_launched, etc.).
        payload: Hook payload with session_id, transcript_path.

    Returns:
        HookResult with hook output.
    """
    from ...core.hook_result import HookResult

    if not ctx.process_id:
        # Vanilla instance - show hint
        from ...core.tool_utils import build_hcom_command

        return HookResult.allow_with_context(
            "SessionStart",
            f"[hcom available - run '{build_hcom_command()} start' to participate]",
        )

    session_id = payload.session_id
    transcript_path = payload.transcript_path

    if not session_id:
        return HookResult.success()

    from ...core.instances import (
        set_status,
        update_instance_position,
        bind_session_to_process,
    )
    from ...core.db import rebind_instance_session
    from ...core.tool_utils import create_orphaned_pty_identity

    instance_name = bind_session_to_process(session_id, ctx.process_id)
    log_info(
        "hooks",
        "gemini.sessionstart.bind",
        instance=instance_name,
        session_id=session_id,
        process_id=ctx.process_id,
    )

    # Orphaned PTY: process_id exists but no binding (e.g., after session clear)
    # Create fresh identity automatically
    if not instance_name and ctx.process_id:
        instance_name = create_orphaned_pty_identity(session_id, ctx.process_id, tool="gemini")
        log_info(
            "hooks",
            "gemini.sessionstart.orphan_created",
            instance=instance_name,
            process_id=ctx.process_id,
        )

    if not instance_name:
        return HookResult.success()

    rebind_instance_session(instance_name, session_id)

    # Capture launch context (env vars, git branch, tty)
    from ...core.instances import capture_and_store_launch_context

    capture_and_store_launch_context(instance_name)

    updates: dict = {"directory": str(ctx.cwd)}
    if transcript_path:
        updates["transcript_path"] = transcript_path
    update_instance_position(instance_name, updates)
    set_status(instance_name, ST_LISTENING, "start")

    from ...pty.pty_common import set_terminal_title

    set_terminal_title(instance_name)

    # Bootstrap injection moved to BeforeAgent only
    # Reason: Gemini doesn't display SessionStart hook output after /clear
    # BeforeAgent output always works, so it handles all bootstrap injection
    # SessionStart just does identity binding, BeforeAgent does bootstrap
    return HookResult.success()


def handle_beforeagent(
    ctx: "HcomContext",
    payload: "HookPayload",
) -> "HookResult":
    """Handle BeforeAgent hook - fires after user submits prompt.

    Fallback bootstrap if SessionStart injection failed. Primary injection is at SessionStart.
    name_announced check prevents duplicates. Also delivers pending messages.

    Also binds session_id for fresh instances (after /clear creates new identity).

    Args:
        ctx: Execution context.
        payload: Hook payload with session_id, transcript_path.

    Returns:
        HookResult with bootstrap/messages or success.
    """
    from ...core.hook_result import HookResult

    instance = resolve_instance_gemini(ctx, payload)
    if not instance:
        return HookResult.success()

    from ...core.instances import set_status, update_instance_position

    instance_name = instance["name"]

    # Keep directory current
    update_instance_position(instance_name, {"directory": str(ctx.cwd)})

    # Bind session_id if instance doesn't have one (fresh instance after /clear)
    if not instance.get("session_id") and payload.session_id:
        from ...core.db import rebind_session, set_process_binding

        log_info(
            "hooks",
            "gemini.beforeagent.bind_session",
            instance=instance_name,
            session_id=payload.session_id,
        )
        update_instance_position(instance_name, {"session_id": payload.session_id})
        rebind_session(payload.session_id, instance_name)
        # Update process binding with session_id too
        if ctx.process_id:
            set_process_binding(ctx.process_id, payload.session_id, instance_name)

    try_capture_transcript_path(instance_name, payload)

    outputs: list[str] = []

    # Inject bootstrap if not already announced
    from ...hooks.utils import inject_bootstrap_once

    if bootstrap := inject_bootstrap_once(instance_name, instance, tool="gemini"):
        outputs.append(bootstrap)

    # Deliver pending messages
    from ...hooks.common import deliver_pending_messages

    _msgs, formatted = deliver_pending_messages(instance_name)
    if formatted:
        outputs.append(formatted)
    else:
        # Real user prompt (not hcom injection)
        set_status(instance_name, ST_ACTIVE, "prompt")

    if outputs:
        combined = "\n\n---\n\n".join(outputs)
        return HookResult.allow_with_context("BeforeAgent", combined)

    return HookResult.success()


def handle_afteragent(
    ctx: "HcomContext",
    payload: "HookPayload",
) -> "HookResult":
    """Handle AfterAgent hook - fires when agent turn completes.

    Args:
        ctx: Execution context.
        payload: Hook payload.

    Returns:
        HookResult (always success).
    """
    from ...core.hook_result import HookResult

    instance = resolve_instance_gemini(ctx, payload)
    if not instance:
        return HookResult.success()

    from ...core.instances import set_status
    from ...core.runtime import notify_instance

    instance_name = instance["name"]
    set_status(instance_name, ST_LISTENING, "")
    notify_instance(instance_name)

    return HookResult.success()


def handle_beforetool(
    ctx: "HcomContext",
    payload: "HookPayload",
) -> "HookResult":
    """Handle BeforeTool hook - fires before tool execution.

    Args:
        ctx: Execution context.
        payload: Hook payload with tool_name, tool_input.

    Returns:
        HookResult (always success).
    """
    from ...core.hook_result import HookResult
    from ...hooks.common import update_tool_status

    instance = resolve_instance_gemini(ctx, payload)
    if not instance:
        return HookResult.success()

    update_tool_status(instance["name"], "gemini", payload.tool_name or "unknown", payload.tool_input or {})

    return HookResult.success()


def handle_aftertool(
    ctx: "HcomContext",
    payload: "HookPayload",
) -> "HookResult":
    """Handle AfterTool hook - fires after tool execution.

    Vanilla binding: detects [hcom:X] marker from hcom start output.
    Bootstrap injection here is defensive fallback - hcom start already prints bootstrap
    to stdout which Gemini sees in tool_response. name_announced check prevents duplicates.
    Message delivery uses JSON format via additionalContext.

    Args:
        ctx: Execution context.
        payload: Hook payload with tool_name, tool_result.

    Returns:
        HookResult with bootstrap/messages or success.
    """
    from ...core.hook_result import HookResult

    instance = None

    # Vanilla binding: try tool_response first (immediate), transcript fallback
    if not ctx.process_id:
        bound_name = _bind_vanilla_instance(ctx, payload)
        if bound_name:
            from ...core.db import get_instance

            instance = get_instance(bound_name)

    # Process/session binding, or transcript fallback if tool_response failed
    if not instance:
        instance = resolve_instance_gemini(ctx, payload)

    if not instance:
        return HookResult.success()

    instance_name = instance["name"]
    outputs: list[str] = []

    # Inject bootstrap if not already announced
    from ...hooks.utils import inject_bootstrap_once

    if bootstrap := inject_bootstrap_once(instance_name, instance, tool="gemini"):
        outputs.append(bootstrap)

    # Deliver pending messages (JSON format)
    from ...hooks.common import deliver_pending_messages

    _msgs, formatted = deliver_pending_messages(instance_name)
    if formatted:
        outputs.append(formatted)

    if outputs:
        combined = "\n\n---\n\n".join(outputs)
        return HookResult.allow_with_context("AfterTool", combined)

    return HookResult.success()


def handle_notification(
    ctx: "HcomContext",
    payload: "HookPayload",
) -> "HookResult":
    """Handle Notification hook - fires on approval prompts, etc.

    Args:
        ctx: Execution context.
        payload: Hook payload with notification_type.

    Returns:
        HookResult (always success).
    """
    from ...core.hook_result import HookResult

    instance = resolve_instance_gemini(ctx, payload)
    if not instance:
        return HookResult.success()

    from ...core.instances import set_status

    notification_type = payload.notification_type or "unknown"
    if notification_type == "ToolPermission":
        set_status(instance["name"], ST_BLOCKED, "approval")

    return HookResult.success()


def handle_sessionend(
    ctx: "HcomContext",
    payload: "HookPayload",
) -> "HookResult":
    """Handle SessionEnd hook - fires when a session ends.

    Note: Gemini DOES fire SessionStart after /clear, so orphan creation
    is handled there via create_orphaned_pty_identity, not here.
    """
    from ...core.hook_result import HookResult
    from ...hooks.common import finalize_session

    instance = resolve_instance_gemini(ctx, payload)
    if not instance:
        return HookResult.success()

    finalize_session(instance["name"], payload.raw.get("reason", "unknown"))
    return HookResult.success()


# =============================================================================
# Handler Dispatch Map
# =============================================================================

# New handlers that accept ctx/payload and return HookResult
GEMINI_HANDLERS = {
    "gemini-sessionstart": handle_sessionstart,
    "gemini-beforeagent": handle_beforeagent,
    "gemini-afteragent": handle_afteragent,
    "gemini-beforetool": handle_beforetool,
    "gemini-aftertool": handle_aftertool,
    "gemini-notification": handle_notification,
    "gemini-sessionend": handle_sessionend,
}


# =============================================================================
# Entry Points
# =============================================================================


def handle_gemini_hook_with_context(
    hook_name: str,
    ctx: "HcomContext",
    payload: "HookPayload",
) -> "HookResult":
    """Daemon entry point for Gemini hooks - context already built.

    This function enables daemon integration by accepting explicit context
    rather than reading from os.environ/sys.stdin. Returns HookResult
    instead of using sys.exit()/print().

    Args:
        hook_name: Gemini hook name (gemini-sessionstart, gemini-beforeagent, etc.).
        ctx: Immutable execution context (replaces os.environ reads).
        payload: Normalized hook payload (replaces sys.stdin reads).

    Returns:
        HookResult with exit_code, stdout, stderr.

    Example:
        ctx = HcomContext.from_env(request.env, request.cwd)
        payload = HookPayload.from_gemini(request.stdin_json, "gemini-beforeagent")
        result = handle_gemini_hook_with_context("gemini-beforeagent", ctx, payload)
    """
    import time as _time

    from ...core.hook_result import HookResult
    from ...core.paths import ensure_hcom_directories

    start = _time.perf_counter()

    # Check vanilla skip condition using context
    if not ctx.is_launched and hook_name == "gemini-beforeagent":
        return HookResult.success()

    init_start = _time.perf_counter()
    if not ensure_hcom_directories():
        return HookResult.success()
    init_ms = (_time.perf_counter() - init_start) * 1000

    handler = GEMINI_HANDLERS.get(hook_name)
    if not handler:
        return HookResult.error(f"Unknown Gemini hook: {hook_name}")

    try:
        handler_start = _time.perf_counter()
        result = handler(ctx, payload)
        handler_ms = (_time.perf_counter() - handler_start) * 1000
        total_ms = (_time.perf_counter() - start) * 1000
        log_info("hooks", "gemini.dispatch.timing", hook=hook_name,
                 init_ms=round(init_ms, 2), handler_ms=round(handler_ms, 2),
                 total_ms=round(total_ms, 2), exit_code=result.exit_code)
        return result
    except Exception as e:
        log_error("hooks", "gemini_hook_with_context.error", e, hook=hook_name)
        return HookResult.error(str(e))


def handle_gemini_hook(hook_name: str) -> None:
    """Legacy entry point - direct invocation from Gemini CLI.

    Reads stdin/environ, calls new handlers via handle_gemini_hook_with_context.
    """
    from ...core.hcom_context import HcomContext
    from ...core.hook_payload import HookPayload
    from ...core.paths import ensure_hcom_directories

    # Check vanilla skip condition
    from ...core.thread_context import get_is_launched

    if not get_is_launched() and hook_name == "gemini-beforeagent":
        return

    if not ensure_hcom_directories():
        return

    # Build context from os.environ
    ctx = HcomContext.from_os()

    # Read stdin ONCE at dispatch entry
    try:
        stdin_json = json.load(sys.stdin)
    except (json.JSONDecodeError, ValueError):
        stdin_json = {}

    # Build payload from stdin
    payload = HookPayload.from_gemini(stdin_json, hook_name)

    # Dispatch to new handler
    try:
        result = handle_gemini_hook_with_context(hook_name, ctx, payload)
        if result.stdout:
            print(result.stdout, end="")
        if result.stderr:
            print(result.stderr, file=sys.stderr, end="")
        if result.exit_code != 0:
            sys.exit(result.exit_code)
    except Exception as e:
        log_error("hooks", "hook.error", e, hook=hook_name, tool="gemini")


__all__ = [
    "handle_gemini_hook",
    "handle_gemini_hook_with_context",
    "handle_sessionstart",
    "handle_beforeagent",
    "handle_afteragent",
    "handle_beforetool",
    "handle_aftertool",
    "handle_notification",
    "handle_sessionend",
    "GEMINI_HANDLERS",
]
