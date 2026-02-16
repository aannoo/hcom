"""Codex CLI hook handlers for hcom.

Codex has a single hook type: notify. Called via config.toml setting:
    notify = ["hcom", "codex-notify"]

The notify hook receives JSON payload as argv[2] (not stdin like Gemini):
    {
        "type": "agent-turn-complete",
        "thread-id": "uuid",
        "turn-id": "12345",
        "cwd": "/path/to/project",
        "input-messages": ["user prompt"],
        "last-assistant-message": "response text"
    }

Key Functions:
    handle_codex_hook: Entry point dispatcher (only codex-notify supported)
    handle_notify: Process turn completion, update status to listening

Identity Resolution:
    - HCOM-launched: HCOM_PROCESS_ID env var → process binding → instance
    - Vanilla: Search transcript for [hcom:name] marker

Note: Unlike Gemini/Claude, message delivery is NOT done in hooks.
Codex uses PTY injection triggered by TranscriptWatcher detecting idle.
"""

from __future__ import annotations

import json
import sqlite3
import sys
from typing import TYPE_CHECKING

from ...core.log import log_error, log_info
from ...core.transcript import derive_codex_transcript_path
from ...shared import ST_LISTENING

if TYPE_CHECKING:
    from ...core.hcom_context import HcomContext
    from ...core.hook_payload import HookPayload
    from ...core.hook_result import HookResult


# =============================================================================
# Helper Functions
# =============================================================================


def _bind_vanilla_instance(
    ctx: "HcomContext",  # noqa: ARG001 - kept for API consistency
    payload: "HookPayload",
) -> str | None:
    """Bind Codex thread_id to instance by searching transcript for binding marker.

    Args:
        ctx: Execution context.
        payload: Normalized hook payload with thread_id.

    Returns:
        Instance name if bound, None otherwise.
    """
    from ...core.db import get_pending_instances

    # Skip if no pending instances (optimization)
    if not get_pending_instances():
        return None

    thread_id = payload.thread_id
    transcript_path = payload.transcript_path or derive_codex_transcript_path(thread_id)

    if not transcript_path:
        return None

    # Search backwards in chunks for marker (efficient for large files)
    from ...hooks.utils import find_last_bind_marker

    instance_name = find_last_bind_marker(transcript_path)
    if not instance_name:
        return None

    from ...hooks.family import bind_vanilla_instance

    return bind_vanilla_instance(
        instance_name=instance_name,
        session_id=thread_id,
        transcript_path=transcript_path,
        tool="codex",
        hook="codex-notify",
    )


def resolve_instance_codex(
    ctx: "HcomContext",  # noqa: ARG001 - kept for API consistency
    payload: "HookPayload",
) -> dict | None:
    """Resolve Codex instance via process binding or session binding.

    Args:
        ctx: Execution context with process_id.
        payload: Normalized hook payload with thread_id.

    Returns:
        Instance dict or None if not found.
    """
    from ...core.instances import resolve_instance_from_binding

    # Codex can derive transcript_path from thread_id
    transcript_path = payload.transcript_path
    if not transcript_path and payload.thread_id:
        transcript_path = derive_codex_transcript_path(payload.thread_id)

    return resolve_instance_from_binding(
        session_id=payload.thread_id,
        transcript_path=transcript_path,
    )


# =============================================================================
# Handler Functions (ctx/payload pattern)
# =============================================================================


def handle_notify(
    ctx: "HcomContext",
    payload: "HookPayload",
) -> "HookResult":
    """Handle Codex notify hook - signals turn completion.

    Called by Codex with JSON payload containing:
    {
        "type": "agent-turn-complete",
        "thread-id": "uuid",
        "turn-id": "12345",
        "cwd": "/path/to/project",
        "input-messages": ["user prompt"],
        "last-assistant-message": "response text"
    }

    Identity comes from HCOM_PROCESS_ID binding (set by launcher).

    Args:
        ctx: Execution context (process_id, etc.).
        payload: Normalized hook payload with event_type, thread_id.

    Returns:
        HookResult (always success - Codex doesn't use hook output).
    """
    from ...core.hook_result import HookResult

    # Only process agent-turn-complete events
    if payload.event_type != "agent-turn-complete":
        return HookResult.success()

    instance = resolve_instance_codex(ctx, payload)
    if not instance:
        # Try vanilla binding
        bound_name = _bind_vanilla_instance(ctx, payload)
        if not bound_name:
            return HookResult.success()
        from ...core.db import get_instance

        instance = get_instance(bound_name)
        if not instance:
            return HookResult.success()

    instance_name = instance["name"]
    thread_id = payload.thread_id
    transcript_path = payload.transcript_path or derive_codex_transcript_path(thread_id)

    # Update instance session_id to real thread_id FIRST (before status update)
    if thread_id or transcript_path:
        try:
            from ...core.instances import (
                update_instance_position,
                bind_session_to_process,
            )
            from ...core.db import rebind_instance_session

            if thread_id and ctx.process_id:
                canonical = bind_session_to_process(thread_id, ctx.process_id)
                if canonical and canonical != instance_name:
                    instance_name = canonical
                rebind_instance_session(instance_name, thread_id)

            # Capture launch context (env vars, git branch, tty)
            from ...core.context import capture_context_json

            updates: dict = {"launch_context": capture_context_json()}
            # Codex payload includes cwd; fall back to ctx.cwd
            cwd = payload.raw.get("cwd") or str(ctx.cwd)
            if cwd:
                updates["directory"] = str(cwd)
            if thread_id:
                updates["session_id"] = thread_id
            if transcript_path:
                updates["transcript_path"] = transcript_path
            update_instance_position(instance_name, updates)
        except (sqlite3.Error, KeyError) as e:
            log_error("hooks", "hook.error", e, hook="codex-notify", op="update_instance")

    # Update instance status (row exists = participating)
    try:
        from ...core.instances import set_status, update_instance_position
        from ...core.runtime import notify_instance
        from ...core.db import get_instance

        instance = get_instance(instance_name)
        if not instance:
            return HookResult.success()

        # Set idle status with timestamp for TranscriptWatcher race prevention
        from datetime import datetime, timezone

        idle_since = datetime.now(timezone.utc).isoformat()
        set_status(instance_name, ST_LISTENING, "")
        update_instance_position(instance_name, {"idle_since": idle_since})

        notify_instance(instance_name)
    except (sqlite3.Error, KeyError) as e:
        log_error("hooks", "hook.error", e, hook="codex-notify", op="update_status")

    return HookResult.success()


# =============================================================================
# Handler Dispatch Map
# =============================================================================

# New handlers that accept ctx/payload and return HookResult
CODEX_HANDLERS = {
    "codex-notify": handle_notify,
}


# =============================================================================
# Entry Points
# =============================================================================


def handle_codex_hook_with_context(
    hook_name: str,
    ctx: "HcomContext",
    payload: "HookPayload",
) -> "HookResult":
    """Daemon entry point for Codex hooks - context already built.

    This function enables daemon integration by accepting explicit context
    rather than reading from os.environ/sys.argv. Returns HookResult
    instead of using sys.exit()/print().

    Args:
        hook_name: Codex hook name (codex-notify).
        ctx: Immutable execution context (replaces os.environ reads).
        payload: Normalized hook payload (replaces sys.argv reads).

    Returns:
        HookResult with exit_code, stdout, stderr.

    Example:
        ctx = HcomContext.from_env(request.env, request.cwd)
        payload = HookPayload.from_codex(request.argv_json, "codex-notify")
        result = handle_codex_hook_with_context("codex-notify", ctx, payload)
    """
    import time as _time

    from ...core.hook_result import HookResult

    start = _time.perf_counter()

    handler = CODEX_HANDLERS.get(hook_name)
    if not handler:
        return HookResult.error(f"Unknown Codex hook: {hook_name}")

    try:
        handler_start = _time.perf_counter()
        result = handler(ctx, payload)
        handler_ms = (_time.perf_counter() - handler_start) * 1000
        total_ms = (_time.perf_counter() - start) * 1000
        log_info("hooks", "codex.dispatch.timing", hook=hook_name,
                 handler_ms=round(handler_ms, 2), total_ms=round(total_ms, 2),
                 exit_code=result.exit_code)
        return result
    except Exception as e:
        log_error("hooks", "codex_hook_with_context.error", e, hook=hook_name)
        return HookResult.error(str(e))


def handle_codex_hook(hook_name: str) -> None:
    """Legacy entry point - direct invocation from Codex CLI.

    Reads argv/environ, calls new handlers via handle_codex_hook_with_context.
    """
    from ...core.hcom_context import HcomContext
    from ...core.hook_payload import HookPayload

    if hook_name != "codex-notify":
        return

    # Parse payload from argv (sys.argv = ['hcom', 'codex-notify', '{json}'])
    if len(sys.argv) < 3:
        return

    try:
        argv_json = json.loads(sys.argv[2])
    except (json.JSONDecodeError, IndexError):
        return

    # Build context from os.environ
    ctx = HcomContext.from_os()

    # Build payload from argv
    payload = HookPayload.from_codex(argv_json, hook_name)

    # Dispatch to new handler
    try:
        result = handle_codex_hook_with_context(hook_name, ctx, payload)
        if result.stdout:
            print(result.stdout, end="")
        if result.stderr:
            print(result.stderr, file=sys.stderr, end="")
        if result.exit_code != 0:
            sys.exit(result.exit_code)
    except Exception as e:
        log_error("hooks", "hook.error", e, hook=hook_name, tool="codex")


__all__ = [
    "handle_codex_hook",
    "handle_codex_hook_with_context",
    "handle_notify",
    "resolve_instance_codex",
    "CODEX_HANDLERS",
]
