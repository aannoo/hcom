"""Shared hook helpers used by both parent and subagent contexts
Not just message relay related code but anything shared
Functions in this module are called by hooks running in both parent and subagent contexts.
Parent-only or subagent-only logic belongs in parent.py or subagent.py respectively.
"""
from __future__ import annotations
from typing import Any
import sys
import time
import os
import socket

from ..shared import MAX_MESSAGES_PER_DELIVERY
from ..core.instances import load_instance_position, update_instance_position, set_status
from ..core.messages import get_unread_messages, format_hook_messages
from .utils import log_hook_error


def check_external_stop_notification(instance_name: str, instance_data: dict[str, Any] | None) -> dict[str, Any] | None:
    """Shared: show notification if instance was externally stopped

    Returns hook output dict or None.
    """
    if not instance_data or not instance_data.get('external_stop_pending'):
        return None

    update_instance_position(instance_name, {'external_stop_pending': False})

    # Show notification if instance was stopped externally
    if not instance_data.get('enabled', False):
        message = (
            "[HCOM NOTIFICATION]\n"
            "Your HCOM connection has been stopped by an external command.\n"
            "You will no longer receive messages. Stop your current work immediately."
        )
        return {
            "systemMessage": "[hcom stop notification]",
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": message
            }
        }

    return None


def _check_claude_alive() -> bool:
    """Check if Claude process still alive (orphan detection)"""
    # Background instances are intentionally detached (HCOM_BACKGROUND is log filename, not '1')
    if os.environ.get('HCOM_BACKGROUND'):
        return True
    # stdin closed = Claude Code died
    return not sys.stdin.closed


def _setup_tcp_notification(instance_name: str) -> tuple[socket.socket | None, bool]:
    """Setup TCP server for instant wake (shared by parent and subagent)

    Returns (server, tcp_mode)
    """
    try:
        notify_server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        notify_server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        notify_server.bind(('127.0.0.1', 0))
        notify_server.listen(128)  # Larger backlog for notification bursts
        notify_server.setblocking(False)

        return (notify_server, True)
    except Exception as e:
        log_hook_error(f'tcp_notification:{instance_name}', e)
        return (None, False)


def poll_messages(
    instance_id: str,
    timeout: int,
    disable_on_timeout: bool = False
) -> tuple[int, dict[str, Any] | None, bool]:
    """Shared message polling for parent Stop and SubagentStop

    Args:
        instance_id: Instance name to poll for
        timeout: Timeout in seconds (wait_timeout for parent, subagent_timeout for subagent)
        disable_on_timeout: Whether to disable instance on timeout (True for subagents)

    Returns:
        (exit_code, hook_output, timed_out)
        - exit_code: 0 for timeout/disabled, 2 for message delivery
        - output: hook output dict if messages delivered
        - timed_out: True if polling timed out
    """
    try:
        instance_data = load_instance_position(instance_id)
        if not instance_data or not instance_data.get('enabled', False):
            if instance_data and not instance_data.get('enabled'):
                # DEBUG: Log disabled instance detected
                from .utils import log_hook_error
                log_hook_error(f'poll_messages:disabled', f'Instance {instance_id} is disabled, exiting poll')
                set_status(instance_id, 'inactive', 'exit:disabled')
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

        update_instance_position(instance_id, {
            'notify_port': notify_port,
            'tcp_mode': tcp_mode
        })

        # Set status BEFORE loop (visible immediately)
        update_instance_position(instance_id, {'last_stop': time.time()})
        set_status(instance_id, 'idle')

        start = time.time()

        try:
            while time.time() - start < timeout:
                # Check for disabled/session_ended
                instance_data = load_instance_position(instance_id)
                if not instance_data or not instance_data.get('enabled', False):
                    set_status(instance_id, 'inactive', 'exit:disabled')
                    return (0, None, False)
                if instance_data.get('session_ended'):
                    return (0, None, False)

                # Sync: pull remote state + push local events
                try:
                    from ..relay import relay_wait
                    remaining = timeout - (time.time() - start)
                    relay_wait(min(remaining, 25))  # relay.py logs errors internally
                except Exception as e:
                    # Best effort - log import/unexpected errors (relay.py handles its own)
                    log_hook_error('poll_messages:relay_wait', e)

                # Poll BEFORE select() to catch messages from PostToolUse→Stop transition gap
                messages, max_event_id = get_unread_messages(instance_id, update_position=False)

                if messages:
                    # Orphan detection - don't mark as read if Claude died
                    if not _check_claude_alive():
                        return (0, None, False)

                    # Mark as read and deliver
                    update_instance_position(instance_id, {'last_event_id': max_event_id})

                    # Limit messages (both parent and subagent)
                    messages = messages[:MAX_MESSAGES_PER_DELIVERY]
                    formatted = format_hook_messages(messages, instance_id)
                    set_status(instance_id, 'active', f"deliver:{messages[0]['from']}", msg_ts=messages[-1]['timestamp'])

                    output = {
                        "decision": "block",
                        "reason": formatted
                    }
                    return (2, output, False)

                # Calculate remaining time to prevent timeout overshoot
                remaining = timeout - (time.time() - start)
                if remaining <= 0:
                    break

                # TCP select for local notifications
                # relay_wait() already did long-poll, so just short check here
                wait_time = min(remaining, 1.0)

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
                update_instance_position(instance_id, {'last_stop': time.time()})

            # Timeout reached
            if disable_on_timeout:
                update_instance_position(instance_id, {'enabled': False})
                set_status(instance_id, 'inactive', 'exit:timeout')
            return (0, None, True)

        finally:
            # Close socket but keep notify_port in DB (stale reference acceptable)
            # Notifications to stale port fail silently (best-effort). Better than None which skips notification.
            # Next Stop cycle updates to new port. Only clear on true exit (disabled/session ended).
            if notify_server:
                try:
                    notify_server.close()
                except Exception:
                    pass

    except Exception as e:
        # Participant context (after gates) - log errors for debugging
        log_hook_error('poll_messages', e)
        return (0, None, False)
