"""Parent instance hook implementations.

This module handles hooks for top-level Claude Code instances (not subagents).
Parent instances are created via `hcom claude` (PTY) or `hcom start` (vanilla).

Hook Handlers:
    sessionstart()      - Session lifecycle start, bind session, inject bootstrap
    pretooluse()        - Track tool execution status
    posttooluse()       - Message delivery, bootstrap injection, context updates
    stop()              - Idle polling for messages (main delivery path for non-PTY)
    userpromptsubmit()  - PTY mode message delivery, fallback bootstrap
    notify()            - Update status to blocked (permission prompts)
    sessionend()        - Session lifecycle end, cleanup instance

Task Coordination:
    start_task()        - Enter subagent context when Task tool starts
    end_task()          - Deliver freeze-period messages when Task completes

Key Concepts:
    - PTY mode (HCOM_PTY_MODE=1): Stop hook skips, PTY wrapper handles injection
    - Headless (HCOM_BACKGROUND): Background instances use Stop hook polling
    - Vanilla (hcom start): Session binding via [HCOM:BIND:X] marker
    - Bootstrap: Context text injected to tell Claude its hcom identity
    - Freeze messages: Messages sent during foreground Task execution
"""

from __future__ import annotations
from typing import Any
import sys
import os
import json
from pathlib import Path

from ..core.instances import (
    load_instance_position,
    update_instance_position,
    set_status,
    parse_running_tasks,
)
from ..core.config import get_config

from ..core.db import get_db, get_events_since

from .utils import build_hcom_bootstrap_text, notify_instance
from ..core.log import log_error, log_info
from ..core.tool_utils import stop_instance


def _extract_tool_detail(tool_name: str, tool_input: dict[str, Any]) -> str:
    """Extract human-readable detail from tool input for status display.

    Extracts the most relevant field from each tool type:
    - Bash: the command being executed
    - Write/Edit: the file path being modified
    - Task: the prompt/task description

    Args:
        tool_name: Name of the tool (Bash, Write, Edit, Task, etc.)
        tool_input: Tool input dictionary from hook_data

    Returns:
        Relevant detail string, or empty string if tool not recognized.
    """
    match tool_name:
        case "Bash":
            return tool_input.get("command", "")
        case "Write" | "Edit":
            return tool_input.get("file_path", "")
        case "Task":
            return tool_input.get("prompt", "")
        case _:
            return ""


def get_real_session_id(hook_data: dict[str, Any], env_file: str | None) -> str:
    """Extract real session_id from CLAUDE_ENV_FILE path, fallback to hook_data.

    Claude Code has a bug where hook_data.session_id is wrong for fork scenarios
    (--resume X --fork-session). The CLAUDE_ENV_FILE path contains the correct
    session_id since CC creates the directory with Q0() (current WQ.sessionId).

    Note: hook_data.transcript_path also has the wrong session_id in fork scenarios
    (both use the same buggy OLD value). Only CLAUDE_ENV_FILE path is reliable.

    Path structure: ~/.claude/session-env/{session_id}/hook-N.sh
    """
    hook_session_id = hook_data.get("session_id") or hook_data.get("sessionId", "")
    transcript_path = hook_data.get("transcript_path", "")

    log_info(
        "hooks",
        "get_real_session_id.input",
        hook_session_id=hook_session_id,
        env_file=env_file,
        transcript_path=transcript_path,
    )

    if env_file:
        try:
            parts = Path(env_file).parts
            if "session-env" in parts:
                idx = parts.index("session-env")
                if idx + 1 < len(parts):
                    candidate = parts[idx + 1]
                    # Sanity check: looks like UUID (36 chars, 4 hyphens)
                    if len(candidate) == 36 and candidate.count("-") == 4:
                        log_info(
                            "hooks",
                            "get_real_session_id.from_env_file",
                            candidate=candidate,
                            hook_session_id=hook_session_id,
                            match=candidate == hook_session_id,
                        )
                        return candidate
        except Exception as e:
            log_error("hooks", "hook.error", e, hook="get_real_session_id")

    log_info(
        "hooks",
        "get_real_session_id.fallback",
        hook_session_id=hook_session_id,
    )
    return hook_session_id


def sessionstart(hook_data: dict[str, Any]) -> None:
    """Parent SessionStart: bind session_id and inject bootstrap for HCOM-launched instances.

    Bootstrap injection at SessionStart (vs UserPromptSubmit) survives turn undo/rewind
    in Claude TUI since it's injected before first prompt.

    Vanilla doesn't need hook injection - `hcom start` prints bootstrap to stdout,
    Claude sees it in Bash response.

    name_announced flag gates all injection to prevent duplicates.
    """
    # Note: session_id is already corrected by dispatcher via get_real_session_id()
    session_id = hook_data.get("session_id")
    source = hook_data.get("source", "")
    process_id = os.environ.get("HCOM_PROCESS_ID")

    log_info(
        "hooks",
        "sessionstart.entry",
        session_id=session_id,
        source=source,
        process_id=process_id,
        raw_hook_session_id=hook_data.get("session_id"),
        transcript_path=hook_data.get("transcript_path"),
    )

    # Persist session_id for bash commands (CLAUDE_ENV_FILE only available in SessionStart)
    env_file = os.environ.get("CLAUDE_ENV_FILE")
    if env_file and session_id:
        try:
            with open(env_file, "a") as f:
                f.write(f"export HCOM_CLAUDE_UNIX_SESSION_ID={session_id}\n")
        except Exception:
            pass  # Best effort - don't fail hook if write fails

    # Handle compaction: re-inject bootstrap (metadata already exists)
    if source == "compact" and session_id:
        try:
            from ..core.db import get_session_binding
            from ..core.instances import resolve_process_binding

            instance_name = get_session_binding(session_id) or resolve_process_binding(process_id)
            if instance_name:
                if process_id:
                    # hcom-launched: inject full bootstrap (identity via HCOM_PROCESS_ID env)
                    bootstrap = build_hcom_bootstrap_text(instance_name)
                else:
                    # Vanilla: need to rebind session first, then bootstrap injects via posttooluse
                    bootstrap = (
                        f"[HCOM RECOVERY] You were participating in hcom as '{instance_name}'. "
                        f"Run this command now to continue: hcom start --as {instance_name}"
                    )
                    update_instance_position(instance_name, {"name_announced": False})
                output = {
                    "hookSpecificOutput": {
                        "hookEventName": "SessionStart",
                        "additionalContext": bootstrap,
                    }
                }
                print(json.dumps(output))
        except Exception as e:
            log_error("hooks", "hook.error", e, hook="sessionstart")

    # Vanilla instance - show hint
    if not process_id or not session_id:
        from ..core.tool_utils import build_hcom_command

        output = {
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": f"[hcom available - run '{build_hcom_command()} start' to participate]",
            }
        }
        print(json.dumps(output))
        return

    # HCOM-launched: bind session and inject bootstrap
    instance_name = None
    try:
        from ..core.db import get_instance, rebind_session
        from ..core.instances import bind_session_to_process
        from ..core.tool_utils import create_orphaned_pty_identity

        instance_name = bind_session_to_process(session_id, process_id)
        log_info(
            "hooks",
            "sessionstart.bind",
            instance=instance_name,
            session_id=session_id,
            process_id=process_id,
        )

        # Orphaned PTY: process_id exists but no binding (e.g., after /clear)
        # Create fresh identity automatically
        if not instance_name and process_id:
            instance_name = create_orphaned_pty_identity(session_id, process_id, tool="claude")
            log_info(
                "hooks",
                "sessionstart.orphan_created",
                instance=instance_name,
                process_id=process_id,
            )

        if instance_name:
            instance = get_instance(instance_name)
            if instance:
                # Use rebind_session to allow override if session was previously bound
                rebind_session(session_id, instance_name)

                # Handle --resume session_id mismatch: Claude Code gives different session_ids
                # at SessionStart (new internal ID) vs subsequent hooks (resumed ID).
                # Create binding for BOTH so hooks with either ID find this instance.
                original_session_id = hook_data.get("original_session_id", "")
                if original_session_id and original_session_id != session_id:
                    rebind_session(original_session_id, instance_name)
                    # Update process_binding with original session_id
                    # (post-compaction hooks will use this ID)
                    from ..core.db import set_process_binding

                    set_process_binding(process_id, original_session_id, instance_name)
                    log_info(
                        "hooks",
                        "sessionstart.resume_dual_bind",
                        instance=instance_name,
                        hook_session_id=session_id,
                        original_session_id=original_session_id,
                    )

                # Capture launch context (env vars, git branch, tty)
                from ..core.instances import capture_and_store_launch_context

                capture_and_store_launch_context(instance_name)
                set_status(instance_name, "listening", "start")
                # Terminal title
                try:
                    with open("/dev/tty", "w") as tty:
                        tty.write(f"\033]1;hcom: {instance_name}\007\033]2;hcom: {instance_name}\007")
                except (OSError, IOError):
                    pass

                # Inject bootstrap on SessionStart
                # - Fresh launch: name_announced=False → inject and set True
                # - Resume: name_announced=True → inject anyway (context may be lost)
                # SessionStart only fires once per session start, so no duplicate risk
                is_resume = instance.get("name_announced", False)
                bootstrap = build_hcom_bootstrap_text(instance_name)
                output = {
                    "hookSpecificOutput": {
                        "hookEventName": "SessionStart",
                        "additionalContext": bootstrap,
                    }
                }
                print(json.dumps(output))
                if not is_resume:
                    update_instance_position(instance_name, {"name_announced": True})
                    from ..core.paths import increment_flag_counter

                    increment_flag_counter("instance_count")
    except Exception as e:
        log_error("hooks", "bind.fail", e, hook="sessionstart")

    # Pull remote events
    try:
        from ..relay import is_relay_handled_by_tui, pull

        if not is_relay_handled_by_tui():
            pull()
    except Exception:
        pass


def start_task(session_id: str, hook_data: dict[str, Any]) -> dict[str, Any] | None:
    """Task started - enter subagent context

    Creates parent instance if doesn't exist.
    Returns updatedInput dict if hcom instructions should be appended to prompt.
    """
    from ..core.db import get_session_binding
    from .utils import build_hcom_command

    log_info("hooks", "start_task.enter", session_id=session_id)

    # Resolve parent instance via session binding
    instance_name = get_session_binding(session_id)
    if not instance_name:
        log_info("hooks", "start_task.no_binding", session_id=session_id)
        return None

    log_info("hooks", "start_task.resolved", instance=instance_name)

    # Set active flag (track_subagent will append to subagents array)
    # Don't reset subagents array here - multiple parallel Tasks would overwrite each other
    instance_data = load_instance_position(instance_name)
    running_tasks = parse_running_tasks(instance_data.get("running_tasks", ""))
    running_tasks["active"] = True
    update_instance_position(instance_name, {"running_tasks": json.dumps(running_tasks)})

    # Set status (with task prompt as detail) - row exists = participating
    instance_data = load_instance_position(instance_name)
    if instance_data:
        detail = _extract_tool_detail("Task", hook_data.get("tool_input", {}))
        set_status(instance_name, "active", "tool:Task", detail=detail)

    # Append hcom connection instructions to the Task prompt
    tool_input = hook_data.get("tool_input", {})
    original_prompt = tool_input.get("prompt", "")
    if original_prompt:
        hcom_cmd = build_hcom_command()
        hcom_hint = f"\n\n---\nTo use hcom: run `{hcom_cmd} start --name <your-agent-id>` first."
        # Return full tool_input with modified prompt (updatedInput replaces, doesn't merge)
        updated = dict(tool_input)
        updated["prompt"] = original_prompt + hcom_hint
        return updated

    return None


def end_task(session_id: str, hook_data: dict[str, Any], interrupted: bool = False) -> None:
    """Task ended - deliver freeze messages (foreground only), cleanup handled by SubagentStop

    Args:
        session_id: Parent's session ID
        hook_data: Hook data from dispatcher
        interrupted: True if Task was interrupted (UserPromptSubmit handles cleanup)
    """
    from ..core.db import get_session_binding

    # Resolve parent instance via session binding
    instance_name = get_session_binding(session_id)
    if not instance_name:
        return

    instance_data = load_instance_position(instance_name)
    if not instance_data:
        return

    if interrupted:
        # Interrupted via UserPromptSubmit - don't clear here
        # UserPromptSubmit will check transcripts and clean up dead subagents
        return

    # Deliver freeze messages (SubagentStop handles running_tasks cleanup)
    freeze_event_id = instance_data.get("last_event_id", 0)
    last_event_id = _deliver_freeze_messages(instance_name, freeze_event_id)
    update_instance_position(instance_name, {"last_event_id": last_event_id})


def _stop_tracked_subagents(instance_name: str, instance_data: dict[str, Any]) -> None:
    """Stop subagents in running_tasks with exit:interrupted"""
    running_tasks_json = instance_data.get("running_tasks", "")
    if not running_tasks_json:
        return

    try:
        running_tasks = json.loads(running_tasks_json)
        subagents = running_tasks.get("subagents", []) if isinstance(running_tasks, dict) else []
    except json.JSONDecodeError:
        return

    if not subagents:
        return

    conn = get_db()
    agent_id_map = {
        r["agent_id"]: r["name"]
        for r in conn.execute(
            "SELECT name, agent_id FROM instances WHERE parent_name = ?",
            (instance_name,),
        ).fetchall()
        if r["agent_id"]
    }

    for entry in subagents:
        if (aid := entry.get("agent_id")) and (name := agent_id_map.get(aid)):
            set_status(name, "inactive", "exit:interrupted")
            stop_instance(name, initiated_by="system", reason="interrupted")


def _deliver_freeze_messages(instance_name: str, freeze_event_id: int) -> int:
    """Deliver messages from Task freeze period (foreground Tasks only).

    Background Tasks use live delivery instead - parent isn't frozen so messages flow in real-time.
    Returns the last event ID processed (for updating parent position).
    """
    from ..core.messages import should_deliver_message

    # Query freeze period messages
    events = get_events_since(freeze_event_id, event_type="message")

    if not events:
        return freeze_event_id

    # Determine last_event_id from events retrieved
    last_id = max(e["id"] for e in events)

    # Get subagents for message filtering
    conn = get_db()
    subagent_rows = conn.execute(
        "SELECT name, agent_id FROM instances WHERE parent_name = ?", (instance_name,)
    ).fetchall()
    subagent_names = [row["name"] for row in subagent_rows]

    # Filter messages with scope validation
    subagent_msgs = []
    parent_msgs = []

    for event in events:
        event_data = event["data"]

        sender_name = event_data["from"]

        # Build message dict
        msg = {
            "timestamp": event["timestamp"],
            "from": sender_name,
            "message": event_data["text"],
        }

        try:
            # Messages FROM subagents
            if sender_name in subagent_names:
                subagent_msgs.append(msg)
            # Messages TO subagents via scope routing
            elif subagent_names and any(
                should_deliver_message(event_data, name, sender_name) for name in subagent_names
            ):
                if msg not in subagent_msgs:  # Avoid duplicates
                    subagent_msgs.append(msg)
            # Messages TO parent via scope routing
            elif should_deliver_message(event_data, instance_name, sender_name):
                parent_msgs.append(msg)
        except (ValueError, KeyError) as e:
            # ValueError: corrupt message data
            # KeyError: old message format missing 'scope' field
            # Row exists = participating, show error
            inst = load_instance_position(instance_name)
            if inst:
                print(
                    f"Error: Invalid message format in event {event['id']}: {e}. "
                    f"Run 'hcom reset logs' to clear old/corrupt messages.",
                    file=sys.stderr,
                )
            continue

    # Combine and format messages
    all_relevant = subagent_msgs + parent_msgs
    all_relevant.sort(key=lambda m: m["timestamp"])

    if all_relevant:
        formatted = "\n".join(f"{msg['from']}: {msg['message']}" for msg in all_relevant)

        # Format subagent list with agent_ids for correlation
        subagent_list = (
            ", ".join(
                f"{row['name']} (agent_id: {row['agent_id']})" if row["agent_id"] else row["name"]
                for row in subagent_rows
            )
            if subagent_rows
            else "none"
        )

        summary = (
            f"[Task tool completed - Message history during Task tool]\n"
            f"Subagents: {subagent_list}\n"
            f"The following {len(all_relevant)} message(s) occurred:\n\n"
            f"{formatted}\n\n"
            f"[End of message history. Subagents have finished and are no longer active.]"
        )

        output = {
            "systemMessage": "[Task subagent messages shown to instance]",
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": summary,
            },
        }
        print(json.dumps(output, ensure_ascii=False))

    return last_id


def pretooluse(hook_data: dict[str, Any], instance_name: str, tool_name: str) -> None:
    """Parent PreToolUse: status tracking with tool-specific detail

    Called only for enabled instances with validated existence.
    File collision detection handled via event subscriptions (hcom events collision).
    """
    detail = _extract_tool_detail(tool_name, hook_data.get("tool_input", {}))

    # Skip status update for Claude's internal memory operations
    # These Edit calls on session-memory/ files happen while Claude appears idle
    if tool_name in ("Edit", "Write") and "session-memory/" in detail:
        return

    set_status(instance_name, "active", f"tool:{tool_name}", detail=detail)


def update_status(instance_name: str, tool_name: str) -> None:
    """Update parent status (direct call, no checks)"""
    set_status(instance_name, "active", f"tool:{tool_name}")


def stop(instance_name: str, instance_data: dict[str, Any]) -> None:
    """Parent Stop hook - message delivery when Claude goes idle.

    MAIN PATH (hcom claude interactive):
        HCOM_PTY_MODE=1 - exits immediately, PTY wrapper handles injection.
        The PTY poll thread in pty/claude.py monitors for idle, injects "[hcom]"
        trigger, and UserPromptSubmit hook delivers actual messages.

    SECONDARY PATHS (use poll_messages loop):
        - Headless (hcom claude -p): HCOM_BACKGROUND set
        - Vanilla (claude + hcom start): no PTY mode
        Both use the Stop hook's poll_messages() loop with select() for wake.
    """
    from .family import poll_messages

    is_headless = bool(os.environ.get("HCOM_BACKGROUND"))
    log_info(
        "hooks",
        "stop.enter",
        instance=instance_name,
        is_headless=is_headless,
        pty_mode=os.environ.get("HCOM_PTY_MODE"),
    )

    # MAIN PATH: PTY mode exits immediately - poll thread handles injection
    if os.environ.get("HCOM_PTY_MODE") == "1":
        set_status(instance_name, "listening")
        notify_instance(instance_name)
        sys.exit(0)

    # Use shared polling helper (instance_data guaranteed by dispatcher)
    wait_timeout = instance_data.get("wait_timeout")
    timeout = wait_timeout or get_config().timeout

    log_info(
        "hooks",
        "stop.poll_start",
        instance=instance_name,
        timeout=timeout,
        is_headless=is_headless,
    )

    # Persist effective timeout for observability (hcom list --json, TUI)
    update_instance_position(instance_name, {"wait_timeout": timeout})

    exit_code, output, timed_out = poll_messages(
        instance_name,
        timeout,
    )

    log_info(
        "hooks",
        "stop.poll_done",
        instance=instance_name,
        exit_code=exit_code,
        timed_out=timed_out,
        has_output=bool(output),
    )

    if output:
        print(json.dumps(output, ensure_ascii=False))

    if timed_out:
        set_status(instance_name, "inactive", "exit:timeout")

    sys.exit(exit_code)


def posttooluse(
    hook_data: dict[str, Any],
    instance_name: str,
    instance_data: dict[str, Any],
    updates: dict[str, Any] | None = None,
) -> None:
    """Parent PostToolUse: launch context, bootstrap, messages"""
    tool_name = hook_data.get("tool_name", "")
    outputs_to_combine: list[dict[str, Any]] = []

    # Clear blocked status - tool completed means approval was granted
    if instance_data.get("status") == "blocked":
        set_status(instance_name, "active", f"approved:{tool_name}")

    # Pull remote events (rate-limited) - receive messages during operation
    try:
        from ..relay import is_relay_handled_by_tui, pull

        if not is_relay_handled_by_tui():
            pull()  # relay.py logs errors internally
    except Exception as e:
        log_error("relay", "relay.error", e, hook="posttooluse")

    # Bash-specific: persist updates and check bootstrap
    # Updates critical for vanilla instances binding via hcom start
    if tool_name == "Bash":
        if updates:
            update_instance_position(instance_name, updates)
        if output := _inject_bootstrap_if_needed(instance_name, instance_data):
            outputs_to_combine.append(output)

    # Message delivery for ALL tools (parent only)
    if output := _get_posttooluse_messages(instance_name, instance_data):
        outputs_to_combine.append(output)

    # Combine and deliver if any outputs
    if outputs_to_combine:
        combined = _combine_posttooluse_outputs(outputs_to_combine)
        print(json.dumps(combined, ensure_ascii=False))

    sys.exit(0)


def _inject_bootstrap_if_needed(instance_name: str, instance_data: dict[str, Any]) -> dict[str, Any] | None:
    """Defensive fallback bootstrap injection at PostToolUse.

    Rarely fires - vanilla `hcom start` already prints bootstrap to stdout which
    Claude sees in Bash response, and sets name_announced=True. This is a safety net.

    Returns hook output dict or None.
    """
    if instance_data.get("name_announced", False):
        return None

    msg = build_hcom_bootstrap_text(instance_name)
    update_instance_position(instance_name, {"name_announced": True})

    # Track bootstrap count for first-time user hints
    from ..core.paths import increment_flag_counter

    increment_flag_counter("instance_count")

    return {
        "systemMessage": "[HCOM info shown to instance]",
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": msg,
        },
    }


def _get_posttooluse_messages(instance_name: str, _instance_data: dict[str, Any]) -> dict[str, Any] | None:
    """Parent context: check for unread messages
    Returns hook output dict or None.
    """
    from ..core.messages import (
        get_unread_messages,
        format_messages_json,
        format_hook_messages,
    )
    from ..shared import MAX_MESSAGES_PER_DELIVERY

    # Instance guaranteed enabled by dispatcher
    messages, max_event_id = get_unread_messages(instance_name, update_position=False)
    if not messages:
        return None

    deliver_messages = messages[:MAX_MESSAGES_PER_DELIVERY]
    delivered_last_event_id = deliver_messages[-1].get("event_id", max_event_id)
    update_instance_position(instance_name, {"last_event_id": delivered_last_event_id})

    # User-facing (terminal) vs model-facing (context)
    user_display = format_hook_messages(deliver_messages, instance_name)
    model_context = format_messages_json(deliver_messages, instance_name)
    set_status(
        instance_name,
        "active",
        f"deliver:{deliver_messages[0]['from']}",
        msg_ts=deliver_messages[-1]["timestamp"],
    )

    return {
        "systemMessage": user_display,
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": model_context,
        },
    }


def _combine_posttooluse_outputs(outputs: list[dict[str, Any]]) -> dict[str, Any]:
    """Combine multiple PostToolUse outputs
    Returns combined hook output dict.
    """
    if len(outputs) == 1:
        return outputs[0]

    # Combine systemMessages
    system_msgs = [msg for o in outputs if (msg := o.get("systemMessage"))]
    combined_system = " + ".join(system_msgs) if system_msgs else None

    # Combine additionalContext with separator
    contexts = [o["hookSpecificOutput"]["additionalContext"] for o in outputs if "hookSpecificOutput" in o]
    combined_context = "\n\n---\n\n".join(contexts)

    result: dict[str, Any] = {
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": combined_context,
        }
    }
    if combined_system:
        result["systemMessage"] = combined_system

    return result


def userpromptsubmit(
    _hook_data: dict[str, Any],
    instance_name: str,
    updates: dict[str, Any] | None,
    is_matched_resume: bool,
    instance_data: dict[str, Any],
) -> None:
    """Parent UserPromptSubmit: fallback bootstrap, PTY mode message delivery.

    Bootstrap here is fallback for HCOM-launched if SessionStart injection failed.
    Primary injection is at SessionStart. name_announced check prevents duplicates.
    """
    from ..core.messages import (
        get_unread_messages,
        format_messages_json,
        format_hook_messages,
    )

    # Instance guaranteed to exist by dispatcher (row exists = participating)
    name_announced = instance_data.get("name_announced", False)

    # Persist updates (transcript_path, directory, tag, etc.)
    if updates:
        update_instance_position(instance_name, updates)

    # Bootstrap fallback (paranoid safety, likely never used - SessionStart handles this)
    if not name_announced and os.environ.get("HCOM_LAUNCHED") == "1":
        msg = build_hcom_bootstrap_text(instance_name)
        output: dict[str, Any] = {
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "additionalContext": msg,
            }
        }
        print(json.dumps(output), file=sys.stdout)
        update_instance_position(instance_name, {"name_announced": True})
        from ..core.paths import increment_flag_counter

        increment_flag_counter("instance_count")
        set_status(instance_name, "active", "prompt")
        return

    # PTY mode: deliver messages (like Gemini's BeforeAgent)
    # Poll thread injects trigger "[hcom]", this hook delivers actual messages
    if os.environ.get("HCOM_PTY_MODE") == "1":
        from ..shared import MAX_MESSAGES_PER_DELIVERY

        messages, max_event_id = get_unread_messages(instance_name, update_position=False)
        if messages:
            deliver_messages = messages[:MAX_MESSAGES_PER_DELIVERY]
            delivered_last_event_id = deliver_messages[-1].get("event_id", max_event_id)
            update_instance_position(instance_name, {"last_event_id": delivered_last_event_id})

            # User-facing (terminal) vs model-facing (context)
            user_display = format_hook_messages(deliver_messages, instance_name)
            model_context = format_messages_json(deliver_messages, instance_name)
            set_status(
                instance_name,
                "active",
                f"deliver:{deliver_messages[0]['from']}",
                msg_ts=deliver_messages[-1]["timestamp"],
            )
            delivery_output: dict[str, Any] = {
                "systemMessage": user_display,
                "hookSpecificOutput": {
                    "hookEventName": "UserPromptSubmit",
                    "additionalContext": model_context,
                },
            }
            print(json.dumps(delivery_output, ensure_ascii=False), file=sys.stdout)
            return  # Message delivery, not user prompt

    # Set status to active (real user prompt, not hcom injection)
    set_status(instance_name, "active", "prompt")


def notify(
    hook_data: dict[str, Any],
    instance_name: str,
    updates: dict[str, Any] | None,
    instance_data: dict[str, Any],
) -> None:
    """Parent Notification: update status to blocked (parent only, handler filters subagent context)"""
    message = hook_data.get("message", "")

    # Filter out generic "waiting for input" - not a meaningful status change
    if message == "Claude is waiting for your input":
        return

    if updates:
        update_instance_position(instance_name, updates)
    set_status(instance_name, "blocked", message)


def sessionend(hook_data: dict[str, Any], instance_name: str, updates: dict[str, Any]) -> None:
    """Parent SessionEnd: set final status and stop instance"""
    reason = hook_data.get("reason", "unknown")

    # Set status to inactive with reason as context (reason: clear, logout, prompt_input_exit, other)
    set_status(instance_name, "inactive", f"exit:{reason}")

    try:
        if updates:
            update_instance_position(instance_name, updates)
    except Exception as e:
        log_error("hooks", "hook.error", e, hook="sessionend", instance=instance_name)

    # Stop instance (log life event + delete from DB)
    stop_instance(instance_name, initiated_by="session", reason=f"exit:{reason}")
