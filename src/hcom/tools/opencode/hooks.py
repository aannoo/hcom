"""OpenCode hook command handlers for hcom.

OpenCode plugin shells out to hcom for lifecycle management:
    hcom opencode-start --session-id <id>     -> bind session, return instance name
    hcom opencode-status --name <n> --status <s> -> update status
    hcom opencode-read --name <n>             -> fetch messages (stub)

Commands use argv flags (not JSON payload like Claude/Gemini).
All handlers return JSON via HookResult -- never print() or sys.exit().

Entry Points:
    handle_opencode_hook: Direct CLI invocation (reads sys.argv, os.environ)
    handle_opencode_hook_with_context: Daemon invocation (explicit ctx + argv)
"""

from __future__ import annotations

import json
import sys
from typing import TYPE_CHECKING

from ...core.log import log_error, log_info

if TYPE_CHECKING:
    from ...core.hcom_context import HcomContext
    from ...core.hook_result import HookResult


# =============================================================================
# Helper Functions
# =============================================================================


def _parse_flag(argv: list[str], flag: str) -> str | None:
    """Extract --flag value from argv. Returns None if not found."""
    try:
        idx = argv.index(flag)
        if idx + 1 < len(argv):
            return argv[idx + 1]
    except ValueError:
        pass
    return None


# =============================================================================
# Handler Functions (ctx/argv pattern)
# =============================================================================


def handle_start(
    ctx: "HcomContext",
    argv: list[str],
) -> "HookResult":
    """Handle opencode-start: bind session to process, set listening status.

    Called by OpenCode plugin on session.created event.
    Expects: hcom opencode-start --session-id <session_id> [--notify-port <port>]

    Args:
        ctx: Execution context (process_id from HCOM_PROCESS_ID).
        argv: Command arguments after hook name (e.g. ['--session-id', 'abc', '--notify-port', '12345']).

    Returns:
        HookResult with JSON: {"name": "<instance>", "session_id": "<id>"}
        or JSON error on failure.
    """
    from ...core.hook_result import HookResult
    from ...core.paths import ensure_hcom_directories

    ensure_hcom_directories()

    session_id = _parse_flag(argv, "--session-id")
    if not session_id:
        return HookResult.success(json.dumps({"error": "Missing --session-id"}))

    notify_port_str = _parse_flag(argv, "--notify-port")
    notify_port = int(notify_port_str) if notify_port_str else None

    process_id = ctx.process_id
    if not process_id:
        return HookResult.success(json.dumps({"error": "HCOM_PROCESS_ID not set"}))

    from ...shared import ST_LISTENING

    # Re-binding detection: session already bound (compaction or reconnect)
    try:
        from ...core.db import get_session_binding

        existing_name = get_session_binding(session_id)
        if existing_name:
            from ...core.bootstrap import get_bootstrap
            from ...core.instances import set_status, update_instance_position

            rebind_updates: dict = {"name_announced": False, "session_id": session_id}
            # Store transcript_path (DB path) if not already set
            from ...core.transcript.paths import get_opencode_db_path

            db_path = get_opencode_db_path()
            if db_path:
                rebind_updates["transcript_path"] = db_path
            update_instance_position(existing_name, rebind_updates)
            set_status(existing_name, ST_LISTENING, "start")
            bootstrap_text = None
            try:
                bootstrap_text = get_bootstrap(existing_name, tool="opencode")
            except Exception as e:
                log_error("hooks", "opencode-start.rebind_bootstrap_error", e, instance=existing_name)
            result_data: dict = {"name": existing_name, "session_id": session_id}
            if bootstrap_text is not None:
                result_data["bootstrap"] = bootstrap_text
            if notify_port:
                try:
                    from ...core.db import upsert_notify_endpoint

                    upsert_notify_endpoint(existing_name, "plugin", notify_port)
                except Exception as e:
                    log_error("hooks", "opencode-start.notify_port_error", e, instance=existing_name)
            log_info("hooks", "opencode-start.rebind", instance=existing_name, session_id=session_id)
            return HookResult.success(json.dumps(result_data))
    except Exception as e:
        log_error("hooks", "opencode-start.rebind_check_error", e)
        # Fall through to normal binding path on error

    try:
        from ...core.instances import bind_session_to_process

        instance_name = bind_session_to_process(session_id, process_id)
        if not instance_name:
            return HookResult.success(json.dumps({"error": "No instance bound to this process"}))
    except Exception as e:
        log_error("hooks", "opencode-start.bind_error", e)
        return HookResult.success(json.dumps({"error": f"Binding failed: {e}"}))

    try:
        from ...core.db import rebind_instance_session
        from ...core.instances import set_status, update_instance_position

        rebind_instance_session(instance_name, session_id)

        # Initialize last_event_id BEFORE set_status() — set_status triggers
        # notify_instance() which TCP-wakes the plugin's deliverPendingToIdle().
        # If last_event_id is still 0 at that point, ALL historical events get delivered.
        from ...core.db import get_instance, get_last_event_id

        existing = get_instance(instance_name)
        if existing and existing.get("last_event_id", 0) == 0:
            from ...core.thread_context import get_launch_event_id

            launch_event_id_str = get_launch_event_id()
            current_max = get_last_event_id()
            if launch_event_id_str:
                lei = int(launch_event_id_str)
                update_instance_position(instance_name, {"last_event_id": lei if lei <= current_max else current_max})
            else:
                update_instance_position(instance_name, {"last_event_id": current_max})

        set_status(instance_name, ST_LISTENING, "start")

        # Capture launch context (preserves pane_id/terminal_preset from Rust PTY)
        from ...core.instances import capture_and_store_launch_context

        capture_and_store_launch_context(instance_name)

        # Update remaining instance position fields
        from ...core.transcript.paths import get_opencode_db_path

        updates: dict = {"session_id": session_id}
        db_path = get_opencode_db_path()
        if db_path:
            updates["transcript_path"] = db_path
        if ctx.cwd:
            updates["directory"] = str(ctx.cwd)
        update_instance_position(instance_name, updates)
    except Exception as e:
        log_error("hooks", "opencode-start.update_error", e, instance=instance_name)
        # Non-fatal: binding succeeded, status/context update failed

    # Register TCP notify endpoint for instant message delivery
    if notify_port:
        try:
            from ...core.db import upsert_notify_endpoint

            upsert_notify_endpoint(instance_name, "plugin", notify_port)
        except Exception as e:
            log_error("hooks", "opencode-start.notify_port_error", e, instance=instance_name)

    # Build bootstrap text (non-fatal: identity binding is more important)
    bootstrap_text = None
    try:
        from ...core.bootstrap import get_bootstrap

        bootstrap_text = get_bootstrap(instance_name, tool="opencode")
    except Exception as e:
        log_error("hooks", "opencode-start.bootstrap_error", e, instance=instance_name)

    response: dict = {"name": instance_name, "session_id": session_id}
    if bootstrap_text is not None:
        response["bootstrap"] = bootstrap_text
    return HookResult.success(json.dumps(response))


def handle_status(
    ctx: "HcomContext",  # noqa: ARG001 - kept for handler signature consistency
    argv: list[str],
) -> "HookResult":
    """Handle opencode-status: update instance status.

    Called by OpenCode plugin on session.status and session.idle events.
    Expects: hcom opencode-status --name <name> --status <status> [--context <ctx>] [--detail <d>]

    Args:
        ctx: Execution context (unused, kept for handler pattern).
        argv: Command arguments after hook name.

    Returns:
        HookResult with JSON: {"ok": true} or error JSON.
    """
    from ...core.hook_result import HookResult

    name = _parse_flag(argv, "--name")
    status = _parse_flag(argv, "--status")
    if not name or not status:
        return HookResult.success(json.dumps({"error": "Missing --name or --status"}))

    context = _parse_flag(argv, "--context") or ""
    detail = _parse_flag(argv, "--detail") or ""

    try:
        from ...core.instances import set_status

        set_status(name, status, context, detail)
    except Exception as e:
        log_error("hooks", "opencode-status.error", e, instance=name)
        return HookResult.success(json.dumps({"error": f"Status update failed: {e}"}))

    # Wake delivery thread if instance is now listening
    if status == "listening":
        try:
            from ...core.runtime import notify_instance

            notify_instance(name)
        except Exception:
            pass  # Non-fatal: delivery thread will poll eventually

    return HookResult.success(json.dumps({"ok": True}))


def handle_read(
    ctx: "HcomContext",  # noqa: ARG001 - kept for handler signature consistency
    argv: list[str],
) -> "HookResult":
    """Handle opencode-read: fetch pending messages, check for messages, format, or ack.

    Modes:
    - Default: Return pending messages as JSON array (does NOT advance cursor)
    - --format: Return formatted text (same format as Claude/Gemini delivery)
    - --check: Return "true" or "false" string (has pending messages?)
    - --ack --up-to <id>: Advance cursor to explicit event_id (no fresh read, no overshoot race)
    - --ack (no --up-to): Advance cursor to max pending event_id (legacy, races with new arrivals)

    Args:
        ctx: Execution context (unused, kept for handler pattern).
        argv: Command arguments after hook name.

    Returns:
        HookResult with JSON array, formatted text, boolean string, or ack JSON.
    """
    from ...core.hook_result import HookResult

    name = _parse_flag(argv, "--name")
    if not name:
        return HookResult.success(json.dumps({"error": "Missing --name"}))

    format_mode = "--format" in argv
    check_mode = "--check" in argv
    ack_mode = "--ack" in argv

    try:
        from ...core.messages import get_unread_messages

        messages, max_event_id = get_unread_messages(name, update_position=False)
    except Exception as e:
        log_error("hooks", "opencode-read.fetch_error", e, instance=name)
        return HookResult.success(json.dumps({"error": f"Fetch failed: {e}"}))

    if format_mode:
        from ...core.messages import format_messages_json

        if not messages:
            return HookResult.success("")
        formatted = format_messages_json(messages, name)
        return HookResult.success(formatted)

    if ack_mode:
        # --up-to <id>: ack to explicit position (no fresh read, no overshoot race)
        # Without --up-to: ack to max of current pending (legacy, races with new arrivals)
        up_to = _parse_flag(argv, "--up-to")
        if up_to:
            try:
                ack_id = int(up_to)
            except ValueError:
                return HookResult.success(json.dumps({"error": f"Invalid --up-to: {up_to}"}))
            try:
                from ...core.instances import update_instance_position

                update_instance_position(name, {"last_event_id": ack_id})
            except Exception as e:
                log_error("hooks", "opencode-read.ack_error", e, instance=name)
                return HookResult.success(json.dumps({"error": f"Ack failed: {e}"}))
            return HookResult.success(json.dumps({"acked_to": ack_id}))
        # Legacy: ack all pending
        if not messages:
            return HookResult.success(json.dumps({"acked": 0}))
        last_id = max(m.get("event_id", 0) for m in messages)
        if last_id == 0:
            last_id = max_event_id
        try:
            from ...core.instances import update_instance_position

            update_instance_position(name, {"last_event_id": last_id})
        except Exception as e:
            log_error("hooks", "opencode-read.ack_error", e, instance=name)
            return HookResult.success(json.dumps({"error": f"Ack failed: {e}"}))
        return HookResult.success(json.dumps({"acked": len(messages)}))

    if check_mode:
        return HookResult.success("true" if messages else "false")

    # Default: return raw JSON array of message objects
    return HookResult.success(json.dumps(messages or []))


def handle_stop(
    ctx: "HcomContext",  # noqa: ARG001 - kept for handler signature consistency
    argv: list[str],
) -> "HookResult":
    """Handle opencode-stop: finalize session and clean up instance.

    Called by OpenCode plugin on session.deleted event.
    Expects: hcom opencode-stop --name <name> --reason <reason>

    Uses finalize_session() from hooks/common.py -- same pattern as
    Claude handle_sessionend and Gemini handle_sessionend.
    """
    from ...core.hook_result import HookResult
    from ...hooks.common import finalize_session

    name = _parse_flag(argv, "--name")
    reason = _parse_flag(argv, "--reason") or "unknown"
    if not name:
        return HookResult.success(json.dumps({"error": "Missing --name"}))

    try:
        finalize_session(name, reason)
    except Exception as e:
        log_error("hooks", "opencode-stop.error", e, instance=name)
        return HookResult.success(json.dumps({"error": f"Stop failed: {e}"}))

    return HookResult.success(json.dumps({"ok": True}))


# =============================================================================
# Handler Dispatch Map
# =============================================================================

OPENCODE_HANDLERS = {
    "opencode-start": handle_start,
    "opencode-status": handle_status,
    "opencode-read": handle_read,
    "opencode-stop": handle_stop,
}


# =============================================================================
# Entry Points
# =============================================================================


def handle_opencode_hook_with_context(
    hook_name: str,
    ctx: "HcomContext",
    argv: list[str],
) -> "HookResult":
    """Daemon entry point for OpenCode hooks -- context already built.

    Accepts explicit context and argv rather than reading from os.environ/sys.argv.
    Returns HookResult instead of using sys.exit()/print().

    Args:
        hook_name: OpenCode hook name (opencode-start, opencode-status, opencode-read).
        ctx: Immutable execution context (replaces os.environ reads).
        argv: Command arguments after hook name (replaces sys.argv reads).

    Returns:
        HookResult with exit_code, stdout, stderr.
    """
    import time as _time

    from ...core.hook_result import HookResult

    start = _time.perf_counter()

    handler = OPENCODE_HANDLERS.get(hook_name)
    if not handler:
        return HookResult.error(f"Unknown OpenCode hook: {hook_name}")

    try:
        handler_start = _time.perf_counter()
        result = handler(ctx, argv)
        handler_ms = (_time.perf_counter() - handler_start) * 1000
        total_ms = (_time.perf_counter() - start) * 1000
        log_info(
            "hooks",
            "opencode.dispatch.timing",
            hook=hook_name,
            handler_ms=round(handler_ms, 2),
            total_ms=round(total_ms, 2),
            exit_code=result.exit_code,
        )
        return result
    except Exception as e:
        log_error("hooks", "opencode_hook_with_context.error", e, hook=hook_name)
        return HookResult.error(str(e))


def handle_opencode_hook(hook_name: str) -> None:
    """Direct CLI entry point -- reads argv/environ, dispatches to handler.

    Called from cli.py _route_hook_early() for direct Python invocation.
    sys.argv layout: ['hcom', 'opencode-start', '--session-id', 'abc']
    """
    from ...core.hcom_context import HcomContext

    ctx = HcomContext.from_os()
    argv = sys.argv[2:]  # Skip 'hcom' and hook name

    try:
        result = handle_opencode_hook_with_context(hook_name, ctx, argv)
        if result.stdout:
            print(result.stdout, end="")
        if result.stderr:
            print(result.stderr, file=sys.stderr, end="")
        if result.exit_code != 0:
            sys.exit(result.exit_code)
    except Exception as e:
        log_error("hooks", "hook.error", e, hook=hook_name, tool="opencode")


__all__ = [
    "handle_opencode_hook",
    "handle_opencode_hook_with_context",
    "handle_start",
    "handle_status",
    "handle_read",
    "handle_stop",
    "OPENCODE_HANDLERS",
]
