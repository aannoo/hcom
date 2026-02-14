"""Codex transcript parsing for hcom event detection.

Codex doesn't have per-tool hooks like Gemini. Instead, we parse the
transcript file (rollout-*.jsonl) to detect tool calls and user prompts.

Transcript Location:
    ~/.codex/sessions/<session>/rollout-*-<thread-id>.jsonl

Transcript Format (JSONL):
    {"type": "response_item", "payload": {"type": "function_call", ...}, "timestamp": "..."}
    {"type": "response_item", "payload": {"type": "message", "role": "user", ...}, ...}

Detected Events:
    - apply_patch: File edits → collision detection subscriptions
    - shell/shell_command/exec_command: Commands → cmd: subscriptions
    - user messages: Prompts → user_input subscriptions

Key Classes:
    TranscriptWatcher: Incremental parser that tracks file position
    run_transcript_watcher_thread: Background daemon for periodic sync

Event Timestamps:
    Uses original transcript timestamps (not current time) for accurate
    ordering when logging events to the hcom database.
"""

from __future__ import annotations

import json
import re
import sqlite3
import time
from pathlib import Path

from ...core.log import log_error
from ...shared import ST_ACTIVE

# Regex to extract file paths from apply_patch input
APPLY_PATCH_FILE_RE = re.compile(r"\*\*\* (?:Update|Add|Delete) File: (.+?)(?:\n|$)")


class TranscriptWatcher:
    """Watches Codex transcript for tool calls and user prompts.

    Parses transcript incrementally (seeks to last position). Logs:
    - apply_patch -> tool:apply_patch (file edits for collision detection)
    - shell/shell_command -> tool:shell (commands for cmd: subscriptions)
    - user prompts -> status_context='prompt' (for glue/user_input tracking)

    Uses original transcript timestamps for accurate event ordering.
    """

    def __init__(self, instance_name: str, transcript_path: str | None = None):
        self.instance_name = instance_name
        self.transcript_path = transcript_path
        self._file_pos = 0
        self._logged_call_ids: set[str] = set()

    def set_transcript_path(self, path: str) -> None:
        """Set/update transcript path (may not be known at init)."""
        if path != self.transcript_path:
            self.transcript_path = path
            self._file_pos = 0  # Reset position for new file

    def sync(self) -> int:
        """Parse new transcript entries, log tool calls and prompts to events DB.

        Returns number of file edits logged (apply_patch only).
        """
        if not self.transcript_path:
            return 0

        path = Path(self.transcript_path)
        if not path.exists():
            return 0

        edits_logged = 0
        try:
            # Reset position if file was truncated/replaced
            file_size = path.stat().st_size
            if file_size < self._file_pos:
                self._file_pos = 0

            with open(path, "r") as f:
                f.seek(self._file_pos)
                new_lines = f.readlines()
                self._file_pos = f.tell()

            for line in new_lines:
                if not line.strip():
                    continue
                try:
                    entry = json.loads(line)
                    edits_logged += self._process_entry(entry)
                except json.JSONDecodeError:
                    continue
        except Exception as e:
            log_error("pty", "pty.exit", e, instance=self.instance_name, tool="codex")

        return edits_logged

    def _process_entry(self, entry: dict) -> int:
        """Process a single transcript entry. Returns number of edits logged."""
        if entry.get("type") != "response_item":
            return 0

        payload = entry.get("payload", {})
        payload_type = payload.get("type", "")
        timestamp = entry.get("timestamp", "")

        # Handle user messages -> log active:prompt status (but filter hcom injections)
        if payload_type == "message" and payload.get("role") == "user":
            # Extract message text to check for hcom injection
            content = payload.get("content", [])
            text = ""
            for part in content:
                if isinstance(part, dict):
                    text += part.get("text", "")
                elif isinstance(part, str):
                    text += part
            text = text.strip()

            # Skip hcom-injected messages, only log real user prompts
            if not text.startswith("[hcom]"):
                self._log_user_prompt(timestamp)
            return 0

        # Handle both function_call and custom_tool_call formats
        if payload_type not in ("function_call", "custom_tool_call"):
            return 0

        tool_name = payload.get("name", "")
        call_id = payload.get("call_id", "")

        # Skip if already processed
        if call_id and call_id in self._logged_call_ids:
            return 0

        edits = 0

        if tool_name == "apply_patch":
            # Extract file paths from apply_patch input
            input_text = payload.get("input", "") or payload.get("arguments", "")
            files = APPLY_PATCH_FILE_RE.findall(input_text)
            for filepath in files:
                self._log_file_edit(filepath.strip(), timestamp)
                edits += 1

        elif tool_name in ("shell", "shell_command", "exec_command"):
            # Log shell commands for command subscriptions
            # Formats vary by Codex version:
            #   shell: {"command": ["bash", "-lc", "cmd"], "workdir": "..."}
            #   shell_command: {"command": "cmd string", "workdir": "..."}
            #   exec_command: {"cmd": "cmd string", "workdir": "..."}
            args_str = payload.get("arguments", "") or payload.get("input", "")
            try:
                args = json.loads(args_str) if isinstance(args_str, str) else args_str
                cmd = args.get("command", "") or args.get("cmd", "")
                # Handle both array and string formats
                if isinstance(cmd, list):
                    # Array format: ["bash", "-lc", "actual command"]
                    if len(cmd) >= 3 and cmd[0] == "bash" and cmd[1] == "-lc":
                        actual_cmd = cmd[2]
                    else:
                        actual_cmd = " ".join(cmd)
                else:
                    # String format: "actual command"
                    actual_cmd = str(cmd)
                self._log_shell_command(actual_cmd, timestamp)
            except (json.JSONDecodeError, TypeError, AttributeError):
                # Fallback: log raw arguments
                self._log_shell_command(str(args_str)[:500], timestamp)

        if call_id:
            # Bound memory: when too large, clear and start fresh
            # (Sets are unordered, so we can't keep "last N" - just trim periodically)
            if len(self._logged_call_ids) > 10000:
                self._logged_call_ids.clear()
            self._logged_call_ids.add(call_id)

        return edits

    def _log_status_retroactive(self, status: str, context: str, detail: str, timestamp: str) -> None:
        """Log status event and update instance cache if timestamp is newest.

        Events are always logged with original timestamp (for subscriptions/audit).
        Instance cache is only updated if event timestamp >= current status_time,
        preventing retroactive events from overwriting newer state.
        """
        from ...core.db import log_event, get_instance
        from ...core.instances import update_instance_position
        from ...shared import parse_iso_timestamp

        # Always log event with original timestamp
        log_event(
            event_type="status",
            instance=self.instance_name,
            data={"status": status, "context": context, "detail": detail}
            if detail
            else {"status": status, "context": context},
            timestamp=timestamp or None,
        )

        # Only update instance if this event is newer than current status
        if timestamp:
            try:
                event_dt = parse_iso_timestamp(timestamp)
                if event_dt:
                    event_time = int(event_dt.timestamp())
                    instance = get_instance(self.instance_name)
                    current_time = instance.get("status_time", 0) if instance else 0

                    if event_time >= current_time:
                        updates: dict[str, object] = {
                            "status": status,
                            "status_time": event_time,
                            "status_context": context,
                        }
                        if detail:
                            updates["status_detail"] = detail
                        update_instance_position(self.instance_name, updates)
                        # Wake delivery thread for terminal title update
                        from ...core.runtime import notify_instance

                        notify_instance(self.instance_name)
                        try:
                            from ...relay import trigger_push

                            trigger_push()
                        except Exception:
                            pass
            except (ValueError, TypeError):
                pass  # Don't fail on timestamp parse errors

    def _log_file_edit(self, filepath: str, timestamp: str) -> None:
        """Log a file edit status event for collision detection."""
        try:
            self._log_status_retroactive(ST_ACTIVE, "tool:apply_patch", filepath, timestamp)
        except (sqlite3.Error, OSError, KeyError) as e:
            log_error("pty", "pty.exit", e, instance=self.instance_name, tool="codex")

    def _log_shell_command(self, command: str, timestamp: str) -> None:
        """Log a shell command status event for command subscriptions."""
        try:
            self._log_status_retroactive(ST_ACTIVE, "tool:shell", command, timestamp)
        except (sqlite3.Error, OSError, KeyError) as e:
            log_error("pty", "pty.exit", e, instance=self.instance_name, tool="codex")

    def _log_user_prompt(self, timestamp: str) -> None:
        """Log user prompt status event (active:prompt)."""
        try:
            self._log_status_retroactive(ST_ACTIVE, "prompt", "", timestamp)
        except (sqlite3.Error, OSError, KeyError) as e:
            log_error("pty", "pty.exit", e, instance=self.instance_name, tool="codex")


def run_transcript_watcher_thread(
    *,
    instance_name: str,
    process_id: str | None,
    watcher: TranscriptWatcher,
    running_flag: list[bool],
    poll_interval: float = 5.0,
) -> None:
    """Background thread that periodically syncs transcript.

    This function is meant to be run in a daemon thread started by the PTY runner.
    """
    while running_flag[0]:
        if process_id:
            try:
                from ...core.db import get_process_binding

                binding = get_process_binding(process_id)
                bound_name = binding.get("instance_name") if binding else None
                if bound_name and bound_name != instance_name:
                    instance_name = bound_name
                    watcher.instance_name = bound_name
            except sqlite3.Error:
                pass

        # Get transcript path from instance DB (may be set by notify hook)
        try:
            from ...core.db import get_instance

            inst = get_instance(instance_name)
            if inst and inst.get("transcript_path"):
                watcher.set_transcript_path(inst["transcript_path"])
        except (sqlite3.Error, KeyError):
            pass

        # Sync any new edits
        try:
            watcher.sync()
        except Exception as e:
            log_error("pty", "pty.exit", e, instance=instance_name, tool="codex")

        # Sleep in small increments to check running flag
        sleep_until = time.monotonic() + poll_interval
        while running_flag[0] and time.monotonic() < sleep_until:
            time.sleep(0.5)


__all__ = [
    "TranscriptWatcher",
    "run_transcript_watcher_thread",
    "APPLY_PATCH_FILE_RE",
]
