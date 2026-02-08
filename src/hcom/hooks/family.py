"""Shared hook helpers used by both parent and subagent contexts.

This module contains functionality needed by both parent instances and subagents:
- Message polling loop with TCP notification support
- Claude process health checks (orphan detection)
- TCP server setup for instant message wake

Message Delivery Paths:
    ┌─────────────────────────────────────────────────────────────────┐
    │ MAIN PATH (hcom claude interactive):                            │
    │   PTY wrapper handles injection → hooks skip polling            │
    │   HCOM_PTY_MODE=1 → Stop hook exits immediately                 │
    ├─────────────────────────────────────────────────────────────────┤
    │ SECONDARY PATHS (use poll_messages loop):                       │
    │   - Headless (hcom claude -p): HCOM_BACKGROUND set              │
    │   - Vanilla (claude + hcom start): no PTY wrapper               │
    │   - Subagents: SubagentStop polling                             │
    └─────────────────────────────────────────────────────────────────┘

TCP Notification:
    Senders call notify_instance() which connects to a TCP socket to
    instantly wake the poll_messages() select() call. This provides
    sub-second message delivery without busy polling.

Note:
    Parent-only or subagent-only logic belongs in parent.py or subagent.py.
"""

from __future__ import annotations
from typing import Any
import sys
import time
import socket

from ..core.thread_context import get_background_name
from ..shared import MAX_MESSAGES_PER_DELIVERY
from ..core.instances import (
    load_instance_position,
    update_instance_position,
    set_status,
)
from ..core.messages import get_unread_messages, format_messages_json
from ..core.log import log_error, log_info


def _check_claude_alive(is_background: bool | None = None) -> bool:
    """Check if the parent Claude process is still alive (orphan detection).

    Prevents marking messages as read when Claude has died but the hook
    process is still running. This avoids message loss.

    Limitation: sys.stdin.closed is a heuristic — stdin can stay open after
    parent exits (inherited FDs) or close for unrelated reasons (pipe breakage).
    Worst case is a false positive keeping the hook alive slightly longer.

    Pointless seems likely what this is.

    Args:
        is_background: If True, always return True. If None, falls back to
                       get_background_name() for thread-safe access.

    Returns:
        True if Claude is alive (or if running headless), False if Claude died.
    """
    # Background instances are intentionally detached
    if is_background is None:
        is_background = bool(get_background_name())
    if is_background:
        return True
    # stdin closed = Claude Code died
    return not sys.stdin.closed


def _setup_tcp_notification(instance_name: str) -> tuple[socket.socket | None, bool]:
    """Create a TCP server socket for instant message wake notifications.

    The socket binds to localhost on an ephemeral port. Senders connect to
    this port to wake the poll_messages() select() call instantly.

    Args:
        instance_name: Instance name for error logging

    Returns:
        Tuple of (server_socket, tcp_mode_enabled).
        If socket creation fails, returns (None, False) and falls back to polling.
    """
    try:
        notify_server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        notify_server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        notify_server.bind(("127.0.0.1", 0))
        notify_server.listen(128)  # Larger backlog for notification bursts
        notify_server.setblocking(False)

        return (notify_server, True)
    except Exception as e:
        log_error("hooks", "hook.error", e, hook="tcp_notification", instance=instance_name)
        return (None, False)


def poll_messages(
    instance_id: str,
    timeout: int,
    is_background: bool | None = None,
) -> tuple[int, dict[str, Any] | None, bool]:
    """Stop hook polling loop - NOT used by main path (hcom claude).

    WHEN THIS RUNS:
    - Headless Claude (hcom claude -p): HCOM_BACKGROUND set, uses this loop
    - Vanilla Claude (claude + hcom start): no PTY mode, uses this loop
    - Subagents: SubagentStop uses this for background Task agents

    MAIN PATH BYPASSES THIS:
    - hcom claude (interactive): HCOM_PTY_MODE=1, Stop hook exits immediately
      The PTY wrapper's poll thread handles message injection instead.
      See parent.py:stop() for the early exit, pty/claude.py for PTY injection.

    The loop uses select() on a TCP socket for efficient wake-on-message.
    Senders call notify_instance() which connects to wake the select().

    Args:
        instance_id: Instance name to poll for
        timeout: Timeout in seconds (wait_timeout for parent, subagent_timeout for subagent)
        is_background: If True, running in headless mode. If None, falls back to
                       get_background_name() for thread-safe access.

    Returns:
        (exit_code, hook_output, timed_out)
        - exit_code: 0 for timeout/disabled, 2 for message delivery
        - output: hook output dict if messages delivered
        - timed_out: True if polling timed out
    """
    try:
        instance_data = load_instance_position(instance_id)
        if not instance_data:
            # Row doesn't exist = not participating
            return (0, None, False)

        # Setup TCP notification (both parent and subagent use it)
        notify_server, tcp_mode = _setup_tcp_notification(instance_id)

        # Extract notify_port with error handling
        notify_port = None
        if notify_server:
            try:
                notify_port = notify_server.getsockname()[1]
            except Exception:
                # getsockname failed - close socket and fall back to polling
                try:
                    notify_server.close()
                except Exception:
                    pass
                notify_server = None
                tcp_mode = False

        update_instance_position(instance_id, {"tcp_mode": tcp_mode})
        # Register TCP endpoint for notifications (so notify_all_instances can wake us).
        if notify_port:
            try:
                from ..core.db import upsert_notify_endpoint

                upsert_notify_endpoint(instance_id, "hook", int(notify_port))
            except Exception as e:
                log_error(
                    "hooks",
                    "hook.error",
                    e,
                    hook="notify_endpoints",
                    instance=instance_id,
                )

        # Set status BEFORE loop (visible immediately)
        # Note: set_status() atomically updates last_stop when status='listening'
        # Resolve is_background with thread-safe fallback
        if is_background is None:
            is_background = bool(get_background_name())
        is_headless = is_background
        current_status = instance_data.get("status", "unknown")
        log_info(
            "hooks",
            "poll.set_listening",
            instance=instance_id,
            is_headless=is_headless,
            current_status=current_status,
            tcp_mode=tcp_mode,
            has_notify_port=bool(notify_port),
        )
        set_status(instance_id, "listening")
        # Verify status was set
        verify_data = load_instance_position(instance_id)
        log_info(
            "hooks",
            "poll.listening_set",
            instance=instance_id,
            is_headless=is_headless,
            new_status=verify_data.get("status") if verify_data else "no_data",
            last_stop=verify_data.get("last_stop") if verify_data else 0,
        )

        start = time.time()

        try:
            while time.time() - start < timeout:
                # Check for stopped (row deleted)
                instance_data = load_instance_position(instance_id)
                if not instance_data:
                    # Instance was stopped (deleted from DB)
                    return (0, None, False)

                # Sync: pull remote state + push local events
                try:
                    from ..relay import relay_wait

                    remaining = timeout - (time.time() - start)
                    relay_wait(min(remaining, 25))  # relay.py logs errors internally
                except Exception as e:
                    # Best effort - log import/unexpected errors (relay.py handles its own)
                    log_error("relay", "relay.error", e, hook="poll_messages")

                # Poll BEFORE select() to catch messages from PostToolUse→Stop transition gap
                messages, max_event_id = get_unread_messages(instance_id, update_position=False)

                if messages:
                    # Orphan detection - don't mark as read if Claude died
                    if not _check_claude_alive(is_background):
                        return (0, None, False)

                    # Limit messages (both parent and subagent) without losing any unread messages.
                    deliver_messages = messages[:MAX_MESSAGES_PER_DELIVERY]

                    # Mark as read only through the last delivered event ID.
                    delivered_last_event_id = deliver_messages[-1].get("event_id", max_event_id)
                    update_instance_position(instance_id, {"last_event_id": delivered_last_event_id})

                    formatted = format_messages_json(deliver_messages, instance_id)
                    set_status(
                        instance_id,
                        "active",
                        f"deliver:{deliver_messages[0]['from']}",
                        msg_ts=deliver_messages[-1]["timestamp"],
                    )

                    output = {"decision": "block", "reason": formatted}
                    return (2, output, False)

                # Calculate remaining time to prevent timeout overshoot
                remaining = timeout - (time.time() - start)
                if remaining <= 0:
                    break

                # TCP select for local notifications
                # - With relay: relay_wait() did long-poll, short TCP check (1s)
                # - Local-only with TCP: select wakes on notification (30s)
                # - Local-only no TCP: must poll frequently (100ms)
                from ..relay import is_relay_enabled

                if is_relay_enabled():
                    wait_time = min(remaining, 1.0)
                elif notify_server:
                    wait_time = min(remaining, 30.0)
                else:
                    wait_time = min(remaining, 0.1)

                if notify_server:
                    import select

                    readable, _, _ = select.select([notify_server], [], [], wait_time)
                    if readable:
                        # Drain all pending notifications
                        while True:
                            try:
                                notify_server.accept()[0].close()
                            except BlockingIOError:
                                break
                else:
                    time.sleep(wait_time)

                # Update heartbeat
                update_instance_position(instance_id, {"last_stop": time.time()})

            # Timeout reached
            return (0, None, True)

        finally:
            # Close socket; notify_endpoints pruning is best-effort (stale endpoint acceptable).
            if notify_server:
                try:
                    notify_server.close()
                except Exception:
                    pass

    except Exception as e:
        import traceback

        # Participant context (after gates) - log with traceback for debugging
        log_error("hooks", "hook.error", e, hook="poll_messages", tb=traceback.format_exc())
        return (0, None, False)


# ==================== Tool Detail Extraction ====================


TOOL_NAME_MAPPINGS: dict[str, dict[str, list[str]]] = {
    "claude": {
        "bash": ["Bash"],
        "file": ["Write", "Edit"],
        "delegate": ["Task"],
    },
    "gemini": {
        "bash": ["run_shell_command"],
        "file": ["write_file", "replace"],
        "delegate": ["delegate_to_agent"],
    },
    "codex": {
        "bash": ["execute_command", "shell", "shell_command"],
        "file": ["apply_patch"],
    },
}


def extract_tool_detail(tool: str, tool_name: str, tool_input: dict) -> str:
    """Extract human-readable detail from tool input for status display.

    Centralizes tool detail extraction across claude/gemini/codex hooks.

    Args:
        tool: Tool type ("claude", "gemini", "codex")
        tool_name: Specific tool name from hook payload
        tool_input: Tool input dictionary from hook_data

    Returns:
        Relevant detail string, or empty string if tool not recognized.
    """
    mappings = TOOL_NAME_MAPPINGS.get(tool, {})
    for category, tool_names in mappings.items():
        if tool_name in tool_names:
            match category:
                case "bash":
                    return tool_input.get("command", "")
                case "file":
                    return tool_input.get("file_path", "")
                case "delegate":
                    return tool_input.get("prompt", "") or tool_input.get("task", "")
    return ""


# ==================== Vanilla Binding ====================


def bind_vanilla_instance_from_marker(
    *,
    marker_text: str,
    session_id: str | None,
    transcript_path: str | None,
    tool: str,
    hook: str,
    error_returns_instance: bool,
) -> str | None:
    """Bind instance based on [hcom:name] marker.

    Centralizes vanilla binding logic for codex (and potentially other tools).
    Gemini keeps its own parsing due to complex tool_response format.

    Args:
        marker_text: Text content to search for binding marker
        session_id: Session ID to bind (if available)
        transcript_path: Transcript path to store (if available)
        tool: Tool name (claude/gemini/codex)
        hook: Hook name for logging
        error_returns_instance: Return instance_name on errors if True
    """
    if not marker_text:
        return None

    from ..shared import BIND_MARKER_RE

    match = BIND_MARKER_RE.search(marker_text)
    if not match:
        return None

    instance_name = match.group(1)
    if not session_id and not transcript_path:
        return instance_name

    try:
        from ..core.instances import update_instance_position
        from ..core.db import rebind_instance_session

        updates: dict[str, str] = {"tool": tool}
        if session_id:
            updates["session_id"] = session_id
            rebind_instance_session(instance_name, session_id)
        if transcript_path:
            updates["transcript_path"] = transcript_path
        if updates:
            update_instance_position(instance_name, updates)
        log_info("hooks", f"{tool}.bind.success", instance=instance_name, session_id=session_id)
        return instance_name
    except Exception as e:
        log_error("hooks", "hook.error", e, hook=hook, op="bind_vanilla")
        return instance_name if error_returns_instance else None
