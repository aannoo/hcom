"""Shared tool utilities for hcom.

Cross-tool utilities that don't belong in a specific tool's package.
Extracted from hooks/utils.py to reduce cross-package coupling.
"""

from __future__ import annotations

import os
import re
import sys
import shlex
from pathlib import Path

# Platform detection
IS_WINDOWS = sys.platform == "win32"

# ==================== Safe Command Configuration ====================
# Single source of truth for commands that can run without user approval.
# NOT safe: reset (destructive), N codex/gemini (spawns instances)

SAFE_HCOM_COMMANDS = [
    "send",
    "start",
    "help",
    "--help",
    "-h",
    "list",
    "events",
    "listen",
    "relay",
    "config",
    "transcript",
    "archive",
    "bundle",
    "status",
    "--version",
    "-v",
    "--new-terminal",
]

# Tools that support --help/-h flags (for auto-approval of help commands)
HCOM_TOOL_NAMES = ["claude", "gemini", "codex"]


def build_gemini_permissions() -> list[str]:
    """Generate Gemini permission patterns from safe commands.

    Returns patterns for tools.allowed in settings.json.
    Includes both 'hcom' and 'uvx hcom' variants.
    """
    permissions = []
    for cmd in SAFE_HCOM_COMMANDS:
        permissions.append(f"run_shell_command(hcom {cmd})")
        permissions.append(f"run_shell_command(uvx hcom {cmd})")
    return permissions


def build_claude_permissions() -> list[str]:
    """Generate Claude Code permission patterns from safe commands.

    Returns patterns for permissions.allow in settings.json.
    Uses Bash() format with :* wildcard for commands that take args.
    Includes both 'hcom' and 'uvx hcom' variants.
    """
    permissions = []
    for cmd in SAFE_HCOM_COMMANDS:
        # Commands starting with - are flags (no args), others use :* wildcard
        suffix = "" if cmd.startswith("-") else ":*"
        permissions.append(f"Bash(hcom {cmd}{suffix})")
        permissions.append(f"Bash(uvx hcom {cmd}{suffix})")
    return permissions


def build_codex_rules() -> list[str]:
    """Generate Codex execpolicy rules from safe commands.

    Returns prefix_rule lines for hcom.rules file.
    Includes both 'hcom' and 'uvx hcom' variants.
    """
    rules = [
        "# hcom integration - auto-approve safe commands",
    ]
    for cmd in SAFE_HCOM_COMMANDS:
        rules.append(f'prefix_rule(pattern=["hcom", "{cmd}"], decision="allow")')
        rules.append(f'prefix_rule(pattern=["uvx", "hcom", "{cmd}"], decision="allow")')

    # Tool help commands (hcom claude/gemini/codex --help/-h)
    for tool in HCOM_TOOL_NAMES:
        rules.append(
            f'prefix_rule(pattern=["hcom", "{tool}", "--help"], decision="allow")'
        )
        rules.append(f'prefix_rule(pattern=["hcom", "{tool}", "-h"], decision="allow")')
        rules.append(
            f'prefix_rule(pattern=["uvx", "hcom", "{tool}", "--help"], decision="allow")'
        )
        rules.append(
            f'prefix_rule(pattern=["uvx", "hcom", "{tool}", "-h"], decision="allow")'
        )

    return rules


def build_hcom_hook_patterns(tool: str, hook_commands: list[str]) -> list[re.Pattern]:
    """Build regex patterns for detecting hcom hooks in config files.

    Generates common patterns used to identify and remove hcom hooks:
    - Direct hcom command with specific hook args
    - Any tool-specific subcommand (hcom {tool}-*)
    - uvx hcom variants

    Args:
        tool: Tool name ('claude', 'gemini', 'codex')
        hook_commands: List of hook command suffixes (e.g., ['post', 'pre', 'notify'])

    Returns:
        List of compiled regex patterns for hook detection.
    """
    args_pattern = "|".join(hook_commands)
    return [
        re.compile(rf"\bhcom\s+({args_pattern})\b"),  # Direct: hcom post
        re.compile(rf"\bhcom\s+{tool}-"),  # Tool prefix: hcom gemini-*
        re.compile(rf"\buvx\s+hcom\s+{tool}-"),  # uvx: uvx hcom gemini-*
    ]


# Legacy patterns for env var detection (used by Claude and Gemini)
HCOM_ENV_VAR_PATTERNS = [
    re.compile(r"\$\{?HCOM"),  # Unix: $HCOM or ${HCOM}
    re.compile(r"%HCOM%"),  # Windows: %HCOM%
]


def _detect_hcom_command_type() -> str:
    """Detect how to invoke hcom based on execution context

    Priority:
    1. uvx - If running in uv-managed Python and uvx available
           (works for both temporary uvx runs and permanent uv tool install)
    2. short - If hcom binary in PATH
    3. full - Fallback to full python invocation
    """
    import shutil

    if "uv" in Path(sys.executable).resolve().parts and shutil.which("uvx"):
        return "uvx"
    elif shutil.which("hcom"):
        return "short"
    else:
        return "full"


def _build_quoted_invocation() -> str:
    """Build invocation for fallback case - handles packages and pyz

    For packages (pip/uvx/uv tool), uses 'python -m hcom'.
    For pyz/zipapp, uses direct file path to re-invoke the same archive.
    """
    python_path = sys.executable

    # Detect if running inside a pyz/zipapp
    import zipimport

    loader = getattr(sys.modules[__name__], "__loader__", None)
    is_zipapp = isinstance(loader, zipimport.zipimporter)

    # For pyz, use __file__ path; for packages, use -m
    if is_zipapp or not __package__:
        # Standalone pyz or script - use direct file path
        script_path = str(Path(__file__).resolve())
        if IS_WINDOWS:
            py = f'"{python_path}"' if " " in python_path else python_path
            sp = f'"{script_path}"' if " " in script_path else script_path
            return f"{py} {sp}"
        else:
            return f"{shlex.quote(python_path)} {shlex.quote(script_path)}"
    else:
        # Package install (pip/uv tool/editable) - use -m
        if IS_WINDOWS:
            py = f'"{python_path}"' if " " in python_path else python_path
            return f"{py} -m hcom"
        else:
            return f"{shlex.quote(python_path)} -m hcom"


def build_hcom_command() -> str:
    """Build base hcom command based on execution context.

    Detection always runs fresh to avoid staleness when installation method changes.

    Returns just the command (e.g., 'hcom', 'uvx hcom').
    HCOM_DIR is inherited from environment - no need to bake into command.
    """
    cmd_type = _detect_hcom_command_type()

    if cmd_type == "short":
        return "hcom"
    elif cmd_type == "uvx":
        return "uvx hcom"
    else:
        return _build_quoted_invocation()


def find_tool_path(tool: str) -> str | None:
    """Find tool executable path with fallbacks.

    Returns full path if found, None if not installed.
    Claude has special fallback locations; gemini/codex just use PATH.
    """
    import shutil

    # First check PATH
    path = shutil.which(tool)
    if path:
        return path

    # Claude fallback locations (native installer, alias-based)
    if tool == "claude":
        for fallback in [
            Path.home() / ".claude" / "local" / "claude",
            Path.home() / ".local" / "bin" / "claude",
            Path.home() / ".claude" / "bin" / "claude",
        ]:
            if fallback.exists() and fallback.is_file():
                return str(fallback)

    return None


def is_tool_installed(tool: str) -> bool:
    """Check if tool CLI is installed (PATH + fallbacks)."""
    return find_tool_path(tool) is not None


def build_claude_command(claude_args: list[str] | None = None) -> str:
    """Build Claude command string from args.

    All args (including --agent, --model, etc) are passed through to claude CLI.
    """
    cmd_parts = ["claude"]
    if claude_args:
        for arg in claude_args:
            cmd_parts.append(shlex.quote(arg))
    return " ".join(cmd_parts)


def stop_instance(
    instance_name: str, initiated_by: str = "unknown", reason: str = ""
) -> None:
    """Stop instance: kill if headless, notify if PTY/hooks, log snapshot, delete.

    Args:
        instance_name: Instance to stop
        initiated_by: Who initiated (from resolve_identity())
        reason: Context (e.g., 'self', 'timeout', 'orphaned', 'external', 'stop_all', 'remote')

    Row deleted on stop. Snapshot in life event preserves data for transcript access.
    Session/process bindings also deleted to allow clean restart.
    Subagents are recursively stopped when parent stops.
    """
    from .db import get_instance, delete_instance, log_event
    from .runtime import notify_instance

    instance_data = get_instance(instance_name)
    if not instance_data:
        return

    # Kill process only for headless instances (background=True)
    # PTY instances (Gemini/Codex) also have PID but shouldn't be killed here
    pid = instance_data.get("pid")
    is_headless = instance_data.get("background")
    if pid and is_headless and not IS_WINDOWS:
        import signal
        import time

        try:
            os.killpg(pid, signal.SIGTERM)
            # Wait up to 2s for graceful exit
            for _ in range(20):
                time.sleep(0.1)
                try:
                    os.kill(pid, 0)  # Check alive
                except ProcessLookupError:
                    break  # Dead
            else:
                # Still alive after 2s, force kill
                os.killpg(pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass

    # Notify PTY/hooks instances via TCP (wake polling loop)
    tool = instance_data.get("tool", "claude")
    if tool == "claude":
        notify_instance(instance_name)

    # Prepare snapshot before delete (for forensics/transcript access)
    snapshot = {
        "transcript_path": instance_data.get("transcript_path", ""),
        "session_id": instance_data.get("session_id", ""),
        "tool": tool,
        "directory": instance_data.get("directory", ""),
        "parent_name": instance_data.get("parent_name", ""),
        "tag": instance_data.get("tag", ""),
        "wait_timeout": instance_data.get("wait_timeout"),
        "subagent_timeout": instance_data.get("subagent_timeout"),
        "hints": instance_data.get("hints", ""),
        "pid": instance_data.get("pid"),
        "created_at": instance_data.get("created_at"),
        "background": instance_data.get("background", 0),
        "agent_id": instance_data.get("agent_id", ""),
        "launch_args": instance_data.get("launch_args", ""),
        "origin_device_id": instance_data.get("origin_device_id", ""),
        "background_log_file": instance_data.get("background_log_file", ""),
    }

    # Cleanup bindings and stop subagents
    session_id = instance_data.get("session_id")
    _cleanup_session_bindings(session_id, initiated_by)

    # Cleanup event subscriptions for this instance
    # Escape _ and % in instance name (they're LIKE wildcards)
    try:
        from .db import get_db

        escaped_name = instance_name.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")
        conn = get_db()
        conn.execute(
            "DELETE FROM kv WHERE key LIKE ? ESCAPE '\\'", (f"events_sub:%\\_{escaped_name}",)
        )
    except Exception:
        pass

    # Delete first, then log - prevents duplicate stopped events on race conditions
    if not delete_instance(instance_name):
        return

    # Log stopped event only after successful delete
    try:
        log_event(
            "life",
            instance_name,
            {"action": "stopped", "by": initiated_by, "reason": reason, "snapshot": snapshot},
        )
        from ..relay import push

        push()
    except Exception as e:
        from .log import log_error

        log_error("core", "db.error", e, op="stop_instance")


def create_orphaned_pty_identity(
    session_id: str, process_id: str, tool: str = "claude"
) -> str | None:
    """Create fresh identity for orphaned hcom-launched PTY (after /clear or similar).

    When an hcom-launched instance runs /clear:
    - SessionEnd destroys the instance (row + bindings deleted)
    - New session starts with SessionStart
    - HCOM_PROCESS_ID env still exists but no binding
    - This creates a fresh identity with new bindings

    Args:
        session_id: The new session's ID
        process_id: HCOM_PROCESS_ID from environment
        tool: Tool type (claude, gemini, codex)

    Returns:
        New instance name if created, None on failure
    """
    from .instances import (
        generate_unique_name,
        initialize_instance_in_position_file,
        set_status,
        capture_and_store_launch_context,
    )
    from .db import log_event, set_process_binding, rebind_session
    from .log import log_info

    try:
        instance_name = generate_unique_name()

        # Create instance with session binding
        initialize_instance_in_position_file(
            instance_name,
            session_id=session_id,
            tool=tool,
        )

        # Create bindings
        set_process_binding(process_id, session_id, instance_name)
        rebind_session(session_id, instance_name)

        # Capture launch context
        capture_and_store_launch_context(instance_name)

        # Log life event
        log_event(
            "life",
            instance_name,
            {"action": "created", "by": "hook", "reason": "clear_restart"},
        )

        set_status(instance_name, "listening", "start")

        log_info(
            "hooks",
            "orphan.created",
            instance=instance_name,
            process_id=process_id,
            tool=tool,
        )

        return instance_name
    except Exception as e:
        from .log import log_error

        log_error("hooks", "orphan.fail", e, process_id=process_id, tool=tool)
        return None


def _cleanup_session_bindings(session_id: str | None, initiated_by: str) -> None:
    """Delete session/process bindings and recursively stop subagents.

    Called when parent instance stops. Cleans up:
    - Session binding (allows clean restart)
    - All process bindings for session
    - Subagents (recursive stop)
    """
    if not session_id:
        return

    from .db import delete_session_binding, delete_process_binding, get_db

    delete_session_binding(session_id)

    conn = get_db()

    # Delete all process bindings for this session
    proc_rows = conn.execute(
        "SELECT process_id FROM process_bindings WHERE session_id = ?", (session_id,)
    ).fetchall()
    for proc_row in proc_rows:
        delete_process_binding(proc_row["process_id"])

    # Stop subagents that have this instance as parent
    subagents = conn.execute(
        "SELECT name FROM instances WHERE parent_session_id = ?", (session_id,)
    ).fetchall()
    for sub in subagents:
        stop_instance(sub["name"], initiated_by=initiated_by, reason="parent_stopped")


__all__ = [
    "SAFE_HCOM_COMMANDS",
    "HCOM_TOOL_NAMES",
    "HCOM_ENV_VAR_PATTERNS",
    "build_claude_permissions",
    "build_gemini_permissions",
    "build_codex_rules",
    "build_hcom_hook_patterns",
    "build_hcom_command",
    "build_claude_command",
    "find_tool_path",
    "is_tool_installed",
    "stop_instance",
    "create_orphaned_pty_identity",
    "_detect_hcom_command_type",
    "_build_quoted_invocation",
]
