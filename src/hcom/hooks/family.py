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
import os
import socket

from ..shared import MAX_MESSAGES_PER_DELIVERY
from ..core.instances import (
    load_instance_position,
    update_instance_position,
    set_status,
)
from ..core.messages import get_unread_messages, format_messages_json
from ..core.log import log_error, log_info


def _check_claude_alive() -> bool:
    """Check if the parent Claude process is still alive (orphan detection).

    Prevents marking messages as read when Claude has died but the hook
    process is still running. This avoids message loss. (not sure this is correct)

    Returns:
        True if Claude is alive (or if running headless), False if Claude died.

    Detection method:
        - Background instances: always return True (intentionally detached)
        - Interactive instances: check if stdin is closed (Claude death signal)
    """
    # Background instances are intentionally detached (HCOM_BACKGROUND is log filename, not '1')
    if os.environ.get("HCOM_BACKGROUND"):
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
        log_error(
            "hooks", "hook.error", e, hook="tcp_notification", instance=instance_name
        )
        return (None, False)


def poll_messages(
    instance_id: str,
    timeout: int,
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
        is_headless = bool(os.environ.get("HCOM_BACKGROUND"))
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
                messages, max_event_id = get_unread_messages(
                    instance_id, update_position=False
                )

                if messages:
                    # Orphan detection - don't mark as read if Claude died
                    if not _check_claude_alive():
                        return (0, None, False)

                    # Limit messages (both parent and subagent) without losing any unread messages.
                    deliver_messages = messages[:MAX_MESSAGES_PER_DELIVERY]

                    # Mark as read only through the last delivered event ID.
                    delivered_last_event_id = deliver_messages[-1].get(
                        "event_id", max_event_id
                    )
                    update_instance_position(
                        instance_id, {"last_event_id": delivered_last_event_id}
                    )

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
        # Participant context (after gates) - log errors for debugging
        log_error("hooks", "hook.error", e, hook="poll_messages")
        return (0, None, False)
