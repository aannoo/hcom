"""Claude Code settings.json management for hook installation.

This module handles installation and removal of hcom hooks in Claude's
~/.claude/settings.json configuration file. It mirrors patterns from
tools/gemini/settings.py and tools/codex/settings.py.

Hook Configuration:
    CLAUDE_HOOK_CONFIGS defines all 9 hook types with their:
    - hook_type: Claude hook event name (SessionStart, PreToolUse, etc.)
    - matcher: Tool name filter (e.g., "Bash|Task|Write|Edit" for PreToolUse)
    - command_suffix: hcom subcommand (sessionstart, pre, post, poll, etc.)
    - timeout: Max hook execution time in seconds (None = default)

Permissions:
    CLAUDE_HCOM_PERMISSIONS contains safe hcom command patterns auto-approved
    in settings.json permissions.allow. Generated from core/tool_utils.py.

Detection Patterns:
    CLAUDE_HCOM_HOOK_PATTERNS contains regex patterns to detect existing hcom
    hooks during removal. Includes legacy patterns for backwards compatibility.

Public Functions:
    setup_claude_hooks()           - Install all hcom hooks
    verify_claude_hooks_installed() - Check if hooks are correctly installed
    remove_claude_hooks()          - Remove all hcom hooks
    get_claude_settings_path()     - Get settings.json path (respects HCOM_DIR)
    load_claude_settings()         - Load and parse settings.json
"""

from __future__ import annotations
import json
import re
import copy
from pathlib import Path
from typing import Any

from ...core.paths import read_file_with_retry, atomic_write
from ...core.tool_utils import (
    build_hcom_command,
    build_permissions,
    build_hcom_hook_patterns,
    HCOM_ENV_VAR_PATTERNS,
    _build_quoted_invocation,
    _build_all_permission_patterns,
)
from ...shared import IS_WINDOWS

# ==================== Permission Configuration ====================

# Generated from centralized SAFE_HCOM_COMMANDS in core/tool_utils.py
CLAUDE_HCOM_PERMISSIONS = build_permissions("claude")

# ==================== Hook Configuration ====================

# Hook configuration: (hook_type, tool_matcher, command_suffix, timeout)
# Single source of truth - all hook properties derived from this
CLAUDE_HOOK_CONFIGS = [
    ("SessionStart", "", "sessionstart", None),
    ("UserPromptSubmit", "", "userpromptsubmit", None),
    ("PreToolUse", "Bash|Task|Write|Edit", "pre", None),
    ("PostToolUse", "", "post", 86400),
    ("Stop", "", "poll", 86400),  # Poll for messages (24hr max timeout)
    ("SubagentStart", "", "subagent-start", None),  # Subagent birth hook (test)
    ("SubagentStop", "", "subagent-stop", 86400),  # Subagent coordination (24hr max)
    ("Notification", "", "notify", None),
    ("SessionEnd", "", "sessionend", None),
]

# Derived from CLAUDE_HOOK_CONFIGS - guaranteed to stay in sync
CLAUDE_HOOK_TYPES = [cfg[0] for cfg in CLAUDE_HOOK_CONFIGS]
CLAUDE_HOOK_COMMANDS = [cfg[2] for cfg in CLAUDE_HOOK_CONFIGS]

# NOTE: If you remove a hook type from CLAUDE_HOOK_CONFIGS in the future, add it to a
# LEGACY_HOOK_TYPES list for cleanup: LEGACY_HOOK_TYPES = CLAUDE_HOOK_TYPES + ['RemovedHook']
# Then use LEGACY_HOOK_TYPES in _remove_claude_hcom_hooks() to clean up old installations.

# Hook detection patterns (shared base + Claude-specific legacy patterns)
_CLAUDE_HOOK_ARGS_PATTERN = "|".join(CLAUDE_HOOK_COMMANDS)
CLAUDE_HCOM_HOOK_PATTERNS = [
    *HCOM_ENV_VAR_PATTERNS,  # Shared env var patterns
    *build_hcom_hook_patterns("claude", CLAUDE_HOOK_COMMANDS),  # Standard patterns
    # Claude-specific legacy patterns
    re.compile(r"\bHCOM_ACTIVE.*hcom\.py"),  # LEGACY: Unix HCOM_ACTIVE conditional
    re.compile(r'IF\s+"%HCOM_ACTIVE%"'),  # LEGACY: Windows HCOM_ACTIVE conditional
    re.compile(rf'hcom\.py["\']?\s+({_CLAUDE_HOOK_ARGS_PATTERN})\b'),  # LEGACY: hcom.py with optional quote
    re.compile(rf'["\'][^"\']*hcom\.py["\']?\s+({_CLAUDE_HOOK_ARGS_PATTERN})\b(?=\s|$)'),  # LEGACY: Quoted path
    re.compile(r"sh\s+-c.*hcom"),  # LEGACY: Shell wrapper
]

# ==================== Claude Settings Access ====================


def get_claude_settings_path() -> Path:
    """Get path to Claude settings file.

    If HCOM_DIR is set (sandbox), uses HCOM_DIR parent (e.g., /workspace/.claude/settings.json).
    Otherwise uses global (~/.claude/settings.json).
    """
    from ...core.paths import get_project_root

    return get_project_root() / ".claude" / "settings.json"


def load_claude_settings(settings_path: Path, default: Any = None) -> dict[str, Any] | None:
    """Load and parse Claude settings JSON file with retry logic."""
    return read_file_with_retry(settings_path, lambda f: json.load(f), default=default)


def _remove_claude_hcom_hooks(settings: dict[str, Any]) -> bool:
    """Remove all hcom hooks from a Claude settings dictionary.

    Scans all hook types in CLAUDE_HOOK_TYPES and removes any hooks whose
    command matches CLAUDE_HCOM_HOOK_PATTERNS. Also removes HCOM from env
    and hcom permission patterns from permissions.allow.

    Args:
        settings: Mutable settings dict to modify in-place

    Returns:
        True if any hcom hooks/env/permissions were removed, False otherwise.

    Raises:
        ValueError: If settings structure is malformed (non-dict matchers, etc.)
    """
    removed_any = False

    if not isinstance(settings, dict) or "hooks" not in settings:
        return False

    if not isinstance(settings["hooks"], dict):
        return False

    # Check all active hook types for cleanup
    for event in CLAUDE_HOOK_TYPES:
        if event not in settings["hooks"]:
            continue

        # Process each matcher
        updated_matchers = []
        for matcher in settings["hooks"][event]:
            # Fail fast on malformed settings - Claude won't run with broken settings anyway
            if not isinstance(matcher, dict):
                raise ValueError(f"Malformed settings: matcher in {event} is not a dict: {type(matcher).__name__}")

            # Validate hooks field if present
            if "hooks" in matcher and not isinstance(matcher["hooks"], list):
                raise ValueError(
                    f"Malformed settings: hooks in {event} matcher is not a list: {type(matcher['hooks']).__name__}"
                )

            # Work with a copy to avoid any potential reference issues
            matcher_copy = copy.deepcopy(matcher)

            # Filter out HCOM hooks from this matcher
            original_hooks = matcher_copy.get("hooks", [])
            non_hcom_hooks = [
                hook
                for hook in original_hooks
                if not any(pattern.search(hook.get("command", "")) for pattern in CLAUDE_HCOM_HOOK_PATTERNS)
            ]

            # Track if any hooks were removed
            if len(non_hcom_hooks) < len(original_hooks):
                removed_any = True

            # Only keep the matcher if it has non-HCOM hooks remaining
            if non_hcom_hooks:
                matcher_copy["hooks"] = non_hcom_hooks
                updated_matchers.append(matcher_copy)
            elif "hooks" not in matcher or matcher["hooks"] == []:
                # Preserve matchers that never had hooks (missing key or empty list only)
                updated_matchers.append(matcher_copy)

        # Update or remove the event
        if updated_matchers:
            settings["hooks"][event] = updated_matchers
        else:
            del settings["hooks"][event]

    # Remove HCOM from env section
    if "env" in settings and isinstance(settings["env"], dict):
        if "HCOM" in settings["env"]:
            removed_any = True
        settings["env"].pop("HCOM", None)
        # Clean up empty env dict
        if not settings["env"]:
            del settings["env"]

    # Remove hcom permission patterns (both hcom and uvx hcom variants)
    if "permissions" in settings and isinstance(settings["permissions"], dict):
        if "allow" in settings["permissions"] and isinstance(settings["permissions"]["allow"], list):
            all_hcom_patterns = _build_all_permission_patterns("claude")
            original_len = len(settings["permissions"]["allow"])
            settings["permissions"]["allow"] = [
                p for p in settings["permissions"]["allow"] if p not in all_hcom_patterns
            ]
            if len(settings["permissions"]["allow"]) < original_len:
                removed_any = True
            # Clean up empty allow list
            if not settings["permissions"]["allow"]:
                del settings["permissions"]["allow"]
        # Clean up empty permissions dict
        if not settings["permissions"]:
            del settings["permissions"]

    return removed_any


# ==================== Hook Command Building ====================


def _get_hook_command() -> str:
    """Get hook command template.

    Uses ${HCOM} environment variable set in settings.json, with fallback to direct python invocation.
    Windows uses direct invocation because hooks run in CMD/PowerShell context.
    """
    if IS_WINDOWS:
        # Windows: hooks run in CMD context, can't use ${HCOM} syntax
        return _build_quoted_invocation()
    else:
        # Unix: Use HCOM env var from settings.json
        return "${HCOM}"


# ==================== Setup, Verify, Remove ====================


def setup_claude_hooks(include_permissions: bool = True) -> bool:
    """Set up hcom hooks in Claude settings.json.

    Location determined by HCOM_DIR:
    - If HCOM_DIR is set → install to HCOM_DIR_parent/.claude/settings.json
    - Otherwise → install to ~/.claude/settings.json

    - Removes existing hcom hooks first (clean slate)
    - Adds all hooks from CLAUDE_HOOK_CONFIGS
    - Sets HCOM environment variable
    - Uses atomic write for safety

    Args:
        include_permissions: If True, add CLAUDE_HCOM_PERMISSIONS to permissions.allow.
                            If False, remove them.

    Returns True on success.
    Raises Exception on failure.
    """
    settings_path = get_claude_settings_path()
    settings_path.parent.mkdir(parents=True, exist_ok=True)

    try:
        settings = load_claude_settings(settings_path, default={})
        if settings is None:
            settings = {}
    except (json.JSONDecodeError, PermissionError) as e:
        raise Exception(f"Cannot read settings: {e}")

    # Normalize hooks dict (handle malformed hooks gracefully)
    if not isinstance(settings.get("hooks"), dict):
        settings["hooks"] = {}

    # Try to remove existing hcom hooks, skip if malformed
    try:
        _remove_claude_hcom_hooks(settings)
    except ValueError:
        # Malformed hooks - reset to empty and continue
        settings["hooks"] = {}

    # Get the hook command template
    hook_cmd_base = _get_hook_command()

    # Build hook commands from CLAUDE_HOOK_CONFIGS
    for hook_type, matcher, cmd_suffix, timeout in CLAUDE_HOOK_CONFIGS:
        # Initialize or normalize hook_type to list (handle malformed values)
        if hook_type not in settings["hooks"] or not isinstance(settings["hooks"][hook_type], list):
            settings["hooks"][hook_type] = []

        hook_dict: dict[str, Any] = {"hooks": [{"type": "command", "command": f"{hook_cmd_base} {cmd_suffix}"}]}

        # Only include matcher field if non-empty (PreToolUse/PostToolUse use matchers)
        if matcher:
            hook_dict["matcher"] = matcher

        if timeout is not None:
            hook_dict["hooks"][0]["timeout"] = timeout

        settings["hooks"][hook_type].append(hook_dict)

    # Set $HCOM environment variable for all Claude instances (vanilla + hcom-launched)
    # HCOM_DIR is inherited from container/shell environment - not baked into settings
    if "env" not in settings:
        settings["env"] = {}
    settings["env"]["HCOM"] = build_hcom_command()
    # Remove stale HCOM_DIR from settings (now inherited from environment)
    settings["env"].pop("HCOM_DIR", None)

    # Handle hcom permission patterns based on include_permissions flag
    if include_permissions:
        if "permissions" not in settings:
            settings["permissions"] = {}
        if "allow" not in settings["permissions"]:
            settings["permissions"]["allow"] = []
        for pattern in CLAUDE_HCOM_PERMISSIONS:
            if pattern not in settings["permissions"]["allow"]:
                settings["permissions"]["allow"].append(pattern)
    else:
        # Remove hcom permissions if disabled
        if "permissions" in settings and "allow" in settings["permissions"]:
            settings["permissions"]["allow"] = [
                p for p in settings["permissions"]["allow"] if p not in CLAUDE_HCOM_PERMISSIONS
            ]
            if not settings["permissions"]["allow"]:
                del settings["permissions"]["allow"]
            if not settings["permissions"]:
                del settings["permissions"]

    # Write settings atomically
    try:
        atomic_write(settings_path, json.dumps(settings, indent=2))
    except OSError as e:
        raise Exception(f"Cannot write settings: {e}")

    # Quick verification
    if not verify_claude_hooks_installed(settings_path, check_permissions=include_permissions):
        raise Exception("Hook installation failed verification")

    return True


def _verify_claude_hooks_at(settings_path: Path, check_permissions: bool = True) -> bool:
    """Verify hcom hooks at a specific settings path. Returns True if all checks pass."""
    try:
        settings = load_claude_settings(settings_path, default=None)
        if not settings:
            return False

        # Check all hook types have correct commands, timeout values, and matchers
        hooks = settings.get("hooks", {})
        for (
            hook_type,
            expected_matcher,
            cmd_suffix,
            expected_timeout,
        ) in CLAUDE_HOOK_CONFIGS:
            hook_matchers = hooks.get(hook_type, [])
            if not hook_matchers:
                return False

            # Find and verify HCOM hook for this type
            hcom_hook_found = False
            for matcher_dict in hook_matchers:
                for hook in matcher_dict.get("hooks", []):
                    command = hook.get("command", "")
                    # Check for HCOM and the correct subcommand
                    if ("${HCOM}" in command or "hcom" in command.lower()) and cmd_suffix in command:
                        # Found HCOM hook - verify all properties
                        if hcom_hook_found:
                            # Duplicate HCOM hook
                            return False

                        # Verify timeout matches
                        actual_timeout = hook.get("timeout")
                        if actual_timeout != expected_timeout:
                            return False

                        # Verify matcher matches
                        actual_matcher = matcher_dict.get("matcher", "")
                        if actual_matcher != expected_matcher:
                            return False

                        hcom_hook_found = True

            # Must have exactly one HCOM hook with correct properties
            if not hcom_hook_found:
                return False

        # Check that HCOM env var is set
        env = settings.get("env", {})
        if "HCOM" not in env:
            return False

        # Check permissions if enabled
        if check_permissions:
            allow_list = settings.get("permissions", {}).get("allow", [])
            for pattern in CLAUDE_HCOM_PERMISSIONS:
                if pattern not in allow_list:
                    return False

        return True
    except (json.JSONDecodeError, OSError):
        return False


def verify_claude_hooks_installed(settings_path: Path | None = None, check_permissions: bool = True) -> bool:
    """Verify that hcom hooks are correctly installed in Claude settings.

    Checks:
    - All hook types from CLAUDE_HOOK_CONFIGS exist
    - Each has exactly one hcom hook with correct command, timeout, and matcher
    - HCOM env var is set
    - If check_permissions, all CLAUDE_HCOM_PERMISSIONS are in permissions.allow

    Args:
        settings_path: Path to settings file. If None, uses get_claude_settings_path().
        check_permissions: If True, verify permissions are installed too.

    Returns True if all checks pass.
    """
    if settings_path is None:
        settings_path = get_claude_settings_path()
    return _verify_claude_hooks_at(settings_path, check_permissions=check_permissions)


def _remove_claude_hooks_from_path(settings_path: Path) -> bool:
    """Remove hcom hooks from a specific settings path. Returns True on success."""
    if not settings_path.exists():
        return True

    try:
        settings = load_claude_settings(settings_path, default=None)
        if not isinstance(settings, dict) or not settings:
            return True

        _remove_claude_hcom_hooks(settings)

        # Write atomically
        return atomic_write(settings_path, json.dumps(settings, indent=2))
    except (json.JSONDecodeError, OSError):
        return False


def remove_claude_hooks() -> bool:
    """Remove hcom hooks from Claude settings.

    Cleans both:
    - Global: ~/.claude/settings.json
    - Local: get_claude_settings_path() (same path as install)

    Only removes hcom-specific hooks, not the whole file.
    Returns True on success or if no changes needed.
    """
    global_path = Path.home() / ".claude" / "settings.json"
    local_path = get_claude_settings_path()  # Use same path as install

    global_ok = _remove_claude_hooks_from_path(global_path)
    # Only remove local if different from global
    if local_path != global_path:
        local_ok = _remove_claude_hooks_from_path(local_path)
    else:
        local_ok = True
    return global_ok and local_ok
