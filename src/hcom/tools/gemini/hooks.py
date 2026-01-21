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
    - Vanilla: transcript search for [HCOM:BIND:name] marker after hcom start

Message Delivery:
    Messages are delivered via additionalContext in hook response JSON.
    Gemini displays this to the model as system context.
"""

import os
import sys
import json
from pathlib import Path

from ...core.log import log_error, log_info


def try_capture_transcript_path(
    instance_name: str, payload: dict | None = None
) -> None:
    """Try to capture transcript_path from payload if not already set.

    Gemini's ChatRecordingService isn't initialized at SessionStart,
    so transcript_path is empty. It becomes available at BeforeAgent/AfterAgent.
    This opportunistically captures it when available.

    Args:
        instance_name: The instance name to update.
        payload: Pre-read stdin payload (to avoid double-read). If None, reads stdin.
    """
    from ...core.instances import load_instance_position, update_instance_position

    data = load_instance_position(instance_name)
    if data and data.get("transcript_path"):
        return

    transcript_path = None
    # Use provided payload or read from stdin (only if not already consumed)
    if payload is None:
        try:
            payload = json.load(sys.stdin)
        except Exception:
            payload = {}
    transcript_path = payload.get("transcript_path", "") if payload else ""

    if not transcript_path:
        session_id = data.get("session_id", "") if data else ""
        if session_id:
            try:
                import glob

                session_prefix = session_id.split("-")[0]
                gemini_chats = Path.home() / ".gemini" / "tmp"
                pattern = str(
                    gemini_chats / "**" / "chats" / f"session-*-{session_prefix}*.json"
                )
                matches = glob.glob(pattern, recursive=True)
                if matches:
                    transcript_path = max(
                        matches, key=lambda p: Path(p).stat().st_mtime
                    )
            except Exception:
                pass

    if transcript_path:
        update_instance_position(instance_name, {"transcript_path": transcript_path})


def handle_sessionstart() -> None:
    """Handle Gemini SessionStart hook.

    HCOM-launched: bind session_id, inject bootstrap if not announced.
    Vanilla: show hcom hint.

    Bootstrap at SessionStart survives context loss scenarios better than BeforeAgent.
    name_announced flag gates injection to prevent duplicates.
    """
    process_id = os.environ.get("HCOM_PROCESS_ID")
    if not process_id:
        # Vanilla instance - show hint
        from ...core.tool_utils import build_hcom_command

        output = {
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": f"[hcom available - run '{build_hcom_command()} start' to participate]",
            }
        }
        print(json.dumps(output))
        return

    real_session_id = None
    transcript_path = None
    try:
        hook_input = json.load(sys.stdin)
        real_session_id = hook_input.get("session_id") or hook_input.get("sessionId")
        transcript_path = hook_input.get("transcript_path") or hook_input.get(
            "session_path"
        )
    except Exception:
        pass

    if not real_session_id:
        return

    try:
        from ...core.instances import (
            set_status,
            update_instance_position,
            bind_session_to_process,
        )
        from ...core.db import rebind_instance_session
        from ...core.tool_utils import create_orphaned_pty_identity

        instance_name = bind_session_to_process(real_session_id, process_id)
        log_info(
            "hooks",
            "gemini.sessionstart.bind",
            instance=instance_name,
            session_id=real_session_id,
            process_id=process_id,
        )

        # Orphaned PTY: process_id exists but no binding (e.g., after session clear)
        # Create fresh identity automatically
        if not instance_name and process_id:
            instance_name = create_orphaned_pty_identity(
                real_session_id, process_id, tool="gemini"
            )
            log_info(
                "hooks",
                "gemini.sessionstart.orphan_created",
                instance=instance_name,
                process_id=process_id,
            )

        if not instance_name:
            return

        rebind_instance_session(instance_name, real_session_id)

        # Capture launch context (env vars, git branch, tty)
        from ...core.instances import capture_and_store_launch_context

        capture_and_store_launch_context(instance_name)

        if transcript_path:
            update_instance_position(
                instance_name, {"transcript_path": transcript_path}
            )
        set_status(instance_name, "listening", "start")

        from ...pty.pty_common import set_terminal_title

        set_terminal_title(instance_name)

        # Bootstrap injection moved to BeforeAgent only
        # Reason: Gemini doesn't display SessionStart hook output after /clear
        # BeforeAgent output always works, so it handles all bootstrap injection
        # SessionStart just does identity binding, BeforeAgent does bootstrap
    except Exception as e:
        log_error("hooks", "hook.error", e, hook="gemini-sessionstart")


def handle_beforeagent() -> None:
    """Handle BeforeAgent hook - fires after user submits prompt.

    Fallback bootstrap if SessionStart injection failed. Primary injection is at SessionStart.
    name_announced check prevents duplicates. Also delivers pending messages.

    Also binds session_id for fresh instances (after /clear creates new identity).
    """
    instance, payload = resolve_instance()
    if not instance:
        return

    from ...core.messages import get_unread_messages, format_messages_json
    from ...core.instances import set_status, update_instance_position
    from ...core.runtime import build_hcom_bootstrap_text

    from ...shared import MAX_MESSAGES_PER_DELIVERY

    instance_name = instance["name"]

    # Bind session_id if instance doesn't have one (fresh instance after /clear)
    if not instance.get("session_id") and payload:
        session_id = payload.get("session_id") or payload.get("sessionId")
        if session_id:
            from ...core.db import rebind_session, set_process_binding

            log_info(
                "hooks",
                "gemini.beforeagent.bind_session",
                instance=instance_name,
                session_id=session_id,
            )
            update_instance_position(instance_name, {"session_id": session_id})
            rebind_session(session_id, instance_name)
            # Update process binding with session_id too
            process_id = os.environ.get("HCOM_PROCESS_ID")
            if process_id:
                set_process_binding(process_id, session_id, instance_name)

    try_capture_transcript_path(
        instance_name, payload
    )  # Pass payload to avoid double stdin read

    outputs: list[str] = []

    if not instance.get("name_announced", False):
        bootstrap = build_hcom_bootstrap_text(instance_name, tool="gemini")
        outputs.append(bootstrap)
        update_instance_position(instance_name, {"name_announced": True})

    # Deliver pending messages
    messages, max_id = get_unread_messages(instance_name, update_position=False)
    if messages:
        deliver_messages = messages[:MAX_MESSAGES_PER_DELIVERY]
        delivered_last_id = deliver_messages[-1].get("event_id", max_id)
        update_instance_position(instance_name, {"last_event_id": delivered_last_id})

        formatted = format_messages_json(deliver_messages, instance_name)
        msg_ts = deliver_messages[-1].get("timestamp", "")
        set_status(
            instance_name,
            "active",
            f"deliver:{deliver_messages[0]['from']}",
            msg_ts=msg_ts,
        )
        outputs.append(formatted)
    else:
        # Real user prompt (not hcom injection)
        set_status(instance_name, "active", "prompt")

    if outputs:
        combined = "\n\n---\n\n".join(outputs)
        print(
            json.dumps(
                {
                    "decision": "allow",
                    "hookSpecificOutput": {
                        "hookEventName": "BeforeAgent",
                        "additionalContext": combined,
                    },
                }
            )
        )


def handle_afteragent() -> None:
    """Handle AfterAgent hook - fires when agent turn completes."""
    instance, _ = resolve_instance()
    if not instance:
        return

    from ...core.instances import set_status
    from ...core.runtime import notify_instance

    instance_name = instance["name"]
    set_status(instance_name, "listening", "")
    notify_instance(instance_name)


def _extract_tool_detail(tool_name: str, tool_input: dict) -> str:
    """Extract status detail from Gemini tool input."""
    match tool_name:
        case "run_shell_command":
            return tool_input.get("command", "")
        case "write_file" | "replace":
            return tool_input.get("file_path", "")
        case "delegate_to_agent":
            return tool_input.get("prompt", "") or tool_input.get("task", "")
        case _:
            return ""


def handle_beforetool() -> None:
    """Handle BeforeTool hook - fires before tool execution."""
    try:
        payload = json.load(sys.stdin)
    except Exception:
        return

    instance, payload = resolve_instance(payload)
    if not instance:
        return

    from ...core.instances import set_status

    tool_name = "unknown"
    tool_input = {}
    if payload:
        tool_name = payload.get("tool_name", payload.get("toolName", "unknown"))
        tool_input = payload.get("tool_input", {})

    detail = _extract_tool_detail(tool_name, tool_input)
    set_status(instance["name"], "active", f"tool:{tool_name}", detail=detail)


def _bind_vanilla_instance(payload: dict) -> str | None:
    """Bind vanilla Gemini instance by parsing tool_response for [HCOM:BIND:X] marker."""
    from ...core.db import get_pending_instances

    # Skip if no pending instances (optimization)
    if not get_pending_instances():
        return None

    # Only check run_shell_command tool responses
    tool_name = payload.get("tool_name", payload.get("toolName", ""))
    if tool_name != "run_shell_command":
        return None

    tool_response_raw = payload.get("tool_response", "")
    if not tool_response_raw:
        return None

    # tool_response can be dict with various keys or string
    # Gemini format: {"llmContent": "..."} or {"output": "..."} or {"response": {"output": "..."}}
    tool_response = tool_response_raw
    if isinstance(tool_response, dict):
        tool_response = (
            tool_response.get("llmContent", "")
            or tool_response.get("output", "")
            or tool_response.get("response", {}).get("output", "")
        )

    if not tool_response:
        return None

    from ...shared import BIND_MARKER_RE

    match = BIND_MARKER_RE.search(tool_response)
    if not match:
        return None

    instance_name = match.group(1)
    session_id = payload.get("session_id") or payload.get("sessionId")
    transcript_path = payload.get("transcript_path") or payload.get("session_path")

    if not session_id and not transcript_path:
        return instance_name

    try:
        from ...core.instances import update_instance_position
        from ...core.db import rebind_instance_session

        updates = {"tool": "gemini"}
        if session_id:
            updates["session_id"] = session_id
            rebind_instance_session(instance_name, session_id)
        if transcript_path:
            updates["transcript_path"] = transcript_path
        if updates:
            update_instance_position(instance_name, updates)
        log_info(
            "hooks", "gemini.bind.success", instance=instance_name, session_id=session_id
        )
        return instance_name
    except Exception as e:
        log_error("hooks", "hook.error", e, hook="gemini-aftertool", op="bind_vanilla")
        return instance_name


def handle_aftertool() -> None:
    """Handle AfterTool hook - fires after tool execution.

    Vanilla binding: detects [HCOM:BIND:X] marker from hcom start output.
    Bootstrap injection here is defensive fallback - hcom start already prints bootstrap
    to stdout which Gemini sees in tool_response. name_announced check prevents duplicates.
    Message delivery uses JSON format via additionalContext.
    """
    try:
        payload = json.load(sys.stdin)
    except Exception:
        payload = {}

    instance = None

    # Vanilla binding: try tool_response first (immediate), transcript fallback
    if not os.environ.get("HCOM_PROCESS_ID"):
        bound_name = _bind_vanilla_instance(payload)
        if bound_name:
            from ...core.db import get_instance

            instance = get_instance(bound_name)

    # Process/session binding, or transcript fallback if tool_response failed
    if not instance:
        instance, payload = resolve_instance(payload)

    if not instance:
        return

    from ...core.messages import get_unread_messages, format_messages_json
    from ...core.instances import set_status, update_instance_position
    from ...core.runtime import build_hcom_bootstrap_text
    from ...shared import MAX_MESSAGES_PER_DELIVERY

    instance_name = instance["name"]
    outputs: list[str] = []

    if not instance.get("name_announced", False):
        bootstrap = build_hcom_bootstrap_text(instance_name, tool="gemini")
        outputs.append(bootstrap)
        update_instance_position(instance_name, {"name_announced": True})

    # Deliver pending messages (XML format)
    messages, max_id = get_unread_messages(instance_name, update_position=False)
    if messages:
        deliver_messages = messages[:MAX_MESSAGES_PER_DELIVERY]
        delivered_last_id = deliver_messages[-1].get("event_id", max_id)
        update_instance_position(instance_name, {"last_event_id": delivered_last_id})

        formatted = format_messages_json(deliver_messages, instance_name)
        msg_ts = deliver_messages[-1].get("timestamp", "")
        set_status(
            instance_name,
            "active",
            f"deliver:{deliver_messages[0]['from']}",
            msg_ts=msg_ts,
        )
        outputs.append(formatted)

    if outputs:
        combined = "\n\n---\n\n".join(outputs)
        print(
            json.dumps(
                {
                    "decision": "allow",
                    "hookSpecificOutput": {
                        "hookEventName": "AfterTool",
                        "additionalContext": combined,
                    },
                }
            )
        )


def handle_notification() -> None:
    """Handle Notification hook - fires on approval prompts, etc."""
    instance, payload = resolve_instance()
    if not instance:
        return

    from ...core.instances import set_status

    notification_type = payload.get("notification_type", "unknown") if payload else "unknown"
    if notification_type == "ToolPermission":
        set_status(instance["name"], "blocked", "approval")


def handle_sessionend() -> None:
    """Handle SessionEnd hook - fires when a session ends.

    Note: Gemini DOES fire SessionStart after /clear, so orphan creation
    is handled there via create_orphaned_pty_identity, not here.
    """
    instance, payload = resolve_instance()
    if not instance:
        return

    reason = payload.get("reason", "unknown") if payload else "unknown"

    try:
        from ...core.instances import set_status
        from ...core.tool_utils import stop_instance

        log_info(
            "hooks",
            "gemini.sessionend",
            instance=instance["name"],
            reason=reason,
        )

        set_status(instance["name"], "inactive", f"exit:{reason}")
        stop_instance(instance["name"], initiated_by="session", reason=f"exit:{reason}")
    except Exception as e:
        log_error("hooks", "hook.error", e, hook="gemini-sessionend")


def resolve_instance(payload: dict | None = None) -> tuple[dict | None, dict | None]:
    """Resolve instance using HCOM_PROCESS_ID binding or session binding.

    Thin wrapper around shared resolve_instance_from_binding that:
    - Reads payload from stdin if None (Gemini hook behavior)
    - Extracts Gemini-specific session ID keys ('session_id' or 'sessionId')
    """
    if payload is None:
        try:
            payload = json.load(sys.stdin)
        except Exception:
            payload = None

    from ...core.instances import resolve_instance_from_binding

    # Extract Gemini-specific keys
    session_id = None
    transcript_path = None
    if payload:
        session_id = payload.get("session_id") or payload.get("sessionId")
        transcript_path = payload.get("transcript_path") or payload.get("session_path")

    instance = resolve_instance_from_binding(
        session_id=session_id,
        transcript_path=transcript_path,
    )
    return instance, payload


# Handler dispatch map (matches hook names installed by settings.py)
GEMINI_HOOK_HANDLERS = {
    "gemini-sessionstart": handle_sessionstart,
    "gemini-beforeagent": handle_beforeagent,
    "gemini-afteragent": handle_afteragent,
    "gemini-beforetool": handle_beforetool,
    "gemini-aftertool": handle_aftertool,
    "gemini-notification": handle_notification,
    "gemini-sessionend": handle_sessionend,
}


def handle_gemini_hook(hook_name: str) -> None:
    """Dispatch to appropriate Gemini hook handler."""
    hcom_launched = os.environ.get("HCOM_LAUNCHED") == "1"

    # Skip BeforeAgent for vanilla instances - they haven't run hcom start yet
    if not hcom_launched and hook_name == "gemini-beforeagent":
        return

    from ...core.paths import ensure_hcom_directories

    if not ensure_hcom_directories():
        return

    handler = GEMINI_HOOK_HANDLERS.get(hook_name)
    if handler:
        try:
            handler()
        except Exception as e:
            log_error("hooks", "hook.error", e, hook=hook_name, tool="gemini")
    else:
        print(f"Unknown Gemini hook: {hook_name}", file=sys.stderr)
        sys.exit(1)


__all__ = [
    "handle_gemini_hook",
    "handle_sessionstart",
    "handle_beforeagent",
    "handle_afteragent",
    "handle_beforetool",
    "handle_aftertool",
    "handle_notification",
    "handle_sessionend",
    "GEMINI_HOOK_HANDLERS",
]
