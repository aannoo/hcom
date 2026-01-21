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
    - Vanilla: Search transcript for [HCOM:BIND:name] marker

Note: Unlike Gemini/Claude, message delivery is NOT done in hooks.
Codex uses PTY injection triggered by TranscriptWatcher detecting idle.
"""

import os
import sys
import json

from ...core.log import log_error


def _derive_transcript_path(thread_id: str | None) -> str | None:
    """Try to derive transcript_path from thread_id."""
    if not thread_id:
        return None
    try:
        import glob
        from pathlib import Path

        # Respect CODEX_HOME env var if set, else default to ~/.codex
        codex_base = os.environ.get("CODEX_HOME") or str(Path.home() / ".codex")
        codex_home = Path(codex_base) / "sessions"
        pattern = str(codex_home / "**" / f"rollout-*-{thread_id}.jsonl")
        matches = glob.glob(pattern, recursive=True)
        if matches:
            return matches[0]
    except Exception:
        pass
    return None


def _bind_vanilla_instance(payload: dict) -> str | None:
    """Bind Codex thread_id to instance by searching transcript for binding marker.

    Creates session binding for hook participation.
    """
    from ...core.db import get_pending_instances

    # Skip if no pending instances (optimization)
    if not get_pending_instances():
        return None

    thread_id = payload.get("thread-id")
    transcript_path = (
        payload.get("transcript_path")
        or payload.get("session_path")
        or _derive_transcript_path(thread_id)
    )

    if not transcript_path:
        return None

    # Search transcript for binding marker: [HCOM:BIND:<name>]
    try:
        with open(transcript_path, "r") as f:
            content = f.read()
    except Exception:
        return None

    from ...shared import BIND_MARKER_RE

    match = BIND_MARKER_RE.search(content)
    if not match:
        return None

    instance_name = match.group(1)

    try:
        from ...core.instances import update_instance_position
        from ...core.db import rebind_instance_session

        updates: dict[str, str] = {"tool": "codex"}
        if thread_id:
            updates["session_id"] = thread_id
            rebind_instance_session(instance_name, thread_id)
        if transcript_path:
            updates["transcript_path"] = transcript_path
        if updates:
            update_instance_position(instance_name, updates)
        return instance_name
    except Exception as e:
        log_error("hooks", "hook.error", e, hook="codex-notify", op="bind_vanilla")
        return None


def handle_codex_hook(hook_name: str) -> None:
    """Handle Codex hook callbacks.

    Hooks run for both hcom-launched and vanilla Codex.
    For vanilla: attempts transcript-based binding.

    Hook errors are logged but not re-raised - hook failures shouldn't crash Codex.

    Args:
        hook_name: The hook being called (e.g., 'codex-notify')
    """
    if hook_name == "codex-notify":
        try:
            handle_notify()
        except Exception as e:
            log_error("hooks", "hook.error", e, hook=hook_name, tool="codex")


def handle_notify() -> None:
    """Handle Codex notify hook - signals turn completion.

    Called by Codex with JSON payload as argv[1]:
    {
        "type": "agent-turn-complete",
        "thread-id": "uuid",
        "turn-id": "12345",
        "cwd": "/path/to/project",
        "input-messages": ["user prompt"],
        "last-assistant-message": "response text"
    }

    Identity comes from HCOM_PROCESS_ID binding (set by launcher).
    """
    # Parse payload from argv (sys.argv = ['hcom', 'codex-notify', '{json}'])
    if len(sys.argv) < 3:
        return

    try:
        payload = json.loads(sys.argv[2])
    except (json.JSONDecodeError, IndexError):
        return

    event_type = payload.get("type", "")
    if event_type != "agent-turn-complete":
        return

    instance, payload = resolve_instance(payload)
    if not instance:
        if not payload:
            return
        bound_name = _bind_vanilla_instance(payload)
        if not bound_name:
            return
        from ...core.db import get_instance

        instance = get_instance(bound_name)
        if not instance:
            return

    instance_name = instance["name"]

    # Update instance session_id to real thread_id FIRST (before status update)
    if not payload:
        return
    thread_id = payload.get("thread-id")
    transcript_path = (
        payload.get("transcript_path")
        or payload.get("session_path")
        or _derive_transcript_path(thread_id)
    )

    if thread_id or transcript_path:
        try:
            from ...core.instances import (
                update_instance_position,
                bind_session_to_process,
            )
            from ...core.db import rebind_instance_session

            process_id = os.environ.get("HCOM_PROCESS_ID")
            if thread_id and process_id:
                canonical = bind_session_to_process(thread_id, process_id)
                if canonical and canonical != instance_name:
                    instance_name = canonical
                rebind_instance_session(instance_name, thread_id)

            # Capture launch context (env vars, git branch, tty)
            from ...core.context import capture_context_json

            updates = {"launch_context": capture_context_json()}
            if thread_id:
                updates["session_id"] = thread_id
            if transcript_path:
                updates["transcript_path"] = transcript_path
            update_instance_position(instance_name, updates)
        except Exception as e:
            log_error(
                "hooks", "hook.error", e, hook="codex-notify", op="update_instance"
            )

    # Update instance status (row exists = participating)
    try:
        from ...core.instances import set_status, update_instance_position
        from ...core.runtime import notify_instance
        from ...core.db import get_instance

        instance = get_instance(instance_name)
        if not instance:
            return

        # Set idle status with timestamp for TranscriptWatcher race prevention
        from datetime import datetime, timezone

        idle_since = datetime.now(timezone.utc).isoformat()
        set_status(instance_name, "listening", "")
        update_instance_position(instance_name, {"idle_since": idle_since})

        notify_instance(instance_name)
    except Exception as e:
        log_error("hooks", "hook.error", e, hook="codex-notify", op="update_status")


def resolve_instance(payload: dict | None = None) -> tuple[dict | None, dict | None]:
    """Resolve Codex instance via HCOM_PROCESS_ID or session binding.

    Thin wrapper around shared resolve_instance_from_binding that extracts
    Codex-specific session ID key ('thread-id') and derives transcript_path.
    """
    from ...core.instances import resolve_instance_from_binding

    # Extract Codex-specific keys
    session_id = payload.get("thread-id") if payload else None
    transcript_path = None
    if payload:
        transcript_path = payload.get("transcript_path") or payload.get("session_path")
    # Codex can derive transcript_path from thread_id
    if not transcript_path and session_id:
        transcript_path = _derive_transcript_path(session_id)

    instance = resolve_instance_from_binding(
        session_id=session_id,
        transcript_path=transcript_path,
    )
    return instance, payload


__all__ = ["handle_codex_hook", "handle_notify", "resolve_instance"]
