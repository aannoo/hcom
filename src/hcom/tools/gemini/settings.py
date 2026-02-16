"""Gemini CLI hook settings management.

Manages ~/.gemini/settings.json for hcom hook integration:
- Version detection (requires Gemini CLI v0.24.0+)
- Hook installation/removal with atomic writes
- Permission patterns for auto-approving safe hcom commands
- Verification of correct hook configuration

Key Functions:
    setup_gemini_hooks: Install all hcom hooks into settings.json
    remove_gemini_hooks: Clean removal of hcom hooks only
    verify_gemini_hooks_installed: Check hooks are correctly configured
    ensure_hooks_enabled: Set hooksConfig.enabled=true (version-aware)

Settings Structure:
    v0.24.0 - v0.25.x:
        {"hooks": {"enabled": true, "SessionStart": [...]}}
    v0.26.0+:
        {"hooksConfig": {"enabled": true}, "hooks": {"SessionStart": [...]}}
"""

from __future__ import annotations
import re
import copy
import json
import shutil
from pathlib import Path
from typing import Any

from ...core.paths import read_file_with_retry, atomic_write

# ==================== Version Detection ====================

GEMINI_MIN_VERSION = (0, 26, 0)  # Minimum supported version (hooksConfig.enabled schema)


def get_gemini_version() -> tuple[int, int, int] | None:
    """Get installed Gemini CLI version without subprocess.

    Resolves the gemini binary symlink and reads version from package.json.
    Fast: single file read, no process spawn.

    Returns (major, minor, patch) tuple or None if not found/parseable.
    """
    gemini_path = shutil.which("gemini")
    if not gemini_path:
        return None

    try:
        # Follow symlink to real location (e.g., /usr/local/bin/gemini -> .../dist/index.js)
        real_path = Path(gemini_path).resolve()

        # package.json is in same dir as dist/ for npm installs
        package_json = real_path.parent / "package.json"
        if not package_json.exists():
            # Try parent (dist/index.js -> package.json at package root)
            package_json = real_path.parent.parent / "package.json"

        if not package_json.exists():
            return None

        with open(package_json) as f:
            data = json.load(f)

        version_str = data.get("version", "")
        parts = version_str.split(".")
        if len(parts) >= 3:
            # Handle versions like "0.24.0-beta.1"
            patch = parts[2].split("-")[0]
            return (int(parts[0]), int(parts[1]), int(patch))
        return None
    except (OSError, ValueError):
        return None


def is_gemini_version_supported() -> bool:
    """Check if installed Gemini version supports hcom hooks (>= 0.26.0).

    Returns True if:
    - Version detected and >= 0.26.0
    - Version cannot be detected (optimistic fallback - don't block users)
    Returns False only if version detected AND too old.
    """
    version = get_gemini_version()
    if version is None:
        return True  # Can't determine version - allow (optimistic)
    return version >= GEMINI_MIN_VERSION


def _set_hooks_enabled(settings: dict[str, Any]) -> None:
    """Set hooksConfig.enabled = true and clean up legacy hooks.enabled if present."""
    if "hooksConfig" not in settings:
        settings["hooksConfig"] = {}
    settings["hooksConfig"]["enabled"] = True

    # Clean up legacy hooks.enabled from older hcom versions
    hooks = settings.get("hooks")
    if isinstance(hooks, dict) and isinstance(hooks.get("enabled"), bool):
        del settings["hooks"]["enabled"]
        if settings.get("hooks") == {}:
            del settings["hooks"]


def _is_hooks_enabled(settings: dict[str, Any]) -> bool:
    """Check if hooksConfig.enabled is set."""
    return settings.get("hooksConfig", {}).get("enabled") is True


def ensure_hooks_enabled() -> bool:
    """Ensure hooksConfig.enabled = true, migrating from legacy hooks.enabled if needed.

    Call this on any hcom gemini command to auto-fix settings.
    Returns True if already set or successfully fixed, False on error.
    Skips mutation if Gemini version < 0.26.0 (to avoid breaking old installs).
    """
    # Don't mutate settings for old Gemini versions
    version = get_gemini_version()
    if version is not None and version < GEMINI_MIN_VERSION:
        return False  # Old version - don't touch settings

    settings_path = get_gemini_settings_path()
    if not settings_path.exists():
        return True  # No settings yet, setup_gemini_hooks will handle it

    try:
        settings = load_gemini_settings(settings_path, default={})
        if settings is None:
            settings = {}

        # Check if migration needed (legacy hooks.enabled present)
        needs_migration = isinstance(settings.get("hooks", {}).get("enabled"), bool)

        if _is_hooks_enabled(settings) and not needs_migration:
            return True  # Already correct

        # Fix settings (sets hooksConfig.enabled, cleans up legacy hooks.enabled)
        _set_hooks_enabled(settings)

        return atomic_write(settings_path, json.dumps(settings, indent=2))
    except (json.JSONDecodeError, OSError):
        return False


from ...core.tool_utils import (
    build_hcom_command,
    build_permissions,
    build_hcom_hook_patterns,
    HCOM_ENV_VAR_PATTERNS,
    _build_all_permission_patterns,
)

# ==================== Permission Configuration ====================

# Generated from centralized SAFE_HCOM_COMMANDS in core/tool_utils.py
GEMINI_HCOM_PERMISSIONS = build_permissions("gemini")

# ==================== Hook Configuration ====================

# Hook configuration: (hook_type, matcher, command_suffix, timeout, description)
# Single source of truth - all hook properties derived from this
GEMINI_HOOK_CONFIGS = [
    ("SessionStart", "*", "gemini-sessionstart", 5000, "Connect to hcom network"),
    ("BeforeAgent", "*", "gemini-beforeagent", 5000, "Deliver pending messages"),
    ("AfterAgent", "*", "gemini-afteragent", 5000, "Signal ready for messages"),
    ("BeforeTool", ".*", "gemini-beforetool", 5000, "Track tool execution"),
    ("AfterTool", ".*", "gemini-aftertool", 5000, "Deliver messages after tools"),
    ("Notification", "ToolPermission", "gemini-notification", 5000, "Track approval prompts"),
    ("SessionEnd", "*", "gemini-sessionend", 5000, "Disconnect from hcom"),
]

# Derived from GEMINI_HOOK_CONFIGS - guaranteed to stay in sync
GEMINI_HOOK_TYPES = [cfg[0] for cfg in GEMINI_HOOK_CONFIGS]
GEMINI_HOOK_COMMANDS = [cfg[2] for cfg in GEMINI_HOOK_CONFIGS]

# Hook detection patterns for identifying hcom hooks in settings
GEMINI_HCOM_HOOK_PATTERNS = [
    *HCOM_ENV_VAR_PATTERNS,  # Env var patterns in commands
    *build_hcom_hook_patterns("gemini", GEMINI_HOOK_COMMANDS),  # Command patterns
    # Hook name patterns (name field in hook dict)
    re.compile(r"hcom-sessionstart|hcom-beforeagent|hcom-afteragent"),
    re.compile(r"hcom-beforetool|hcom-aftertool|hcom-notification"),
    re.compile(r"hcom-sessionend"),
]

# ==================== Gemini Settings Access ====================


def get_gemini_settings_path() -> Path:
    """Get path to Gemini settings file.

    If HCOM_DIR is set (sandbox), uses HCOM_DIR parent (e.g., /workspace/.gemini/settings.json).
    Otherwise uses global (~/.gemini/settings.json).
    """
    from ...core.paths import get_project_root

    return get_project_root() / ".gemini" / "settings.json"


def load_gemini_settings(settings_path: Path | None = None, default: Any = None) -> dict[str, Any] | None:
    """Load and parse Gemini settings JSON file with retry logic."""
    if settings_path is None:
        settings_path = get_gemini_settings_path()
    if not settings_path.exists():
        return default
    return read_file_with_retry(settings_path, lambda f: json.load(f), default=default)


def _is_hcom_hook(hook: dict) -> bool:
    """Check if a hook dict is an hcom hook."""
    command = hook.get("command", "")
    name = hook.get("name", "")
    return any(pattern.search(command) or pattern.search(name) for pattern in GEMINI_HCOM_HOOK_PATTERNS)


def _remove_hcom_hooks_from_gemini_settings(settings: dict[str, Any]) -> None:
    """Remove hcom hooks from Gemini settings dict (in-place).

    Only removes hcom-specific hooks, preserving all user hooks and settings.
    """
    if not isinstance(settings, dict) or "hooks" not in settings:
        return

    if not isinstance(settings["hooks"], dict):
        return

    for hook_type in list(settings["hooks"].keys()):
        matchers = settings["hooks"][hook_type]
        if not isinstance(matchers, list):
            continue

        updated_matchers = []
        for matcher in matchers:
            # Preserve non-dict entries (e.g., hooks.enabled = true)
            if not isinstance(matcher, dict):
                updated_matchers.append(matcher)
                continue

            if "hooks" in matcher and not isinstance(matcher["hooks"], list):
                # Unknown schema - preserve as-is
                updated_matchers.append(matcher)
                continue

            matcher_copy = copy.deepcopy(matcher)

            # Filter out only hcom hooks
            non_hcom_hooks = [hook for hook in matcher_copy.get("hooks", []) if not _is_hcom_hook(hook)]

            if non_hcom_hooks:
                # Has user hooks - keep matcher with filtered hooks
                matcher_copy["hooks"] = non_hcom_hooks
                updated_matchers.append(matcher_copy)
            elif "hooks" not in matcher:
                # Never had hooks field - preserve matcher
                updated_matchers.append(matcher_copy)
            # else: had only hcom hooks - drop this matcher

        if updated_matchers:
            settings["hooks"][hook_type] = updated_matchers
        else:
            del settings["hooks"][hook_type]

    # Clean up legacy hooks.enabled (schema-invalid in 0.26.0+)
    hooks = settings.get("hooks")
    if isinstance(hooks, dict) and isinstance(hooks.get("enabled"), bool):
        del settings["hooks"]["enabled"]

    # Clean up empty hooks dict
    hooks_dict = settings.get("hooks", {})
    if isinstance(hooks_dict, dict) and not hooks_dict:
        del settings["hooks"]

    # Remove hcom permission patterns from tools.allowed (both hcom and uvx hcom variants)
    if "tools" in settings and isinstance(settings["tools"], dict):
        allowed = settings["tools"].get("allowed")
        if isinstance(allowed, list):
            all_hcom_patterns = _build_all_permission_patterns("gemini")
            settings["tools"]["allowed"] = [p for p in allowed if p not in all_hcom_patterns]
            if not settings["tools"]["allowed"]:
                del settings["tools"]["allowed"]


# ==================== Setup and Verification ====================


def setup_gemini_hooks(include_permissions: bool = True) -> bool:
    """Set up hcom hooks in Gemini settings.json.

    Location determined by HCOM_DIR:
    - If HCOM_DIR set → install to HCOM_DIR_parent/.gemini/settings.json
    - Otherwise → install to ~/.gemini/settings.json

    - Removes existing hcom hooks first (clean slate)
    - Adds all hooks from GEMINI_HOOK_CONFIGS
    - Sets skipNextSpeakerCheck=false (AfterAgent bug workaround)
    - Uses atomic write for safety

    Args:
        include_permissions: If True, add GEMINI_HCOM_PERMISSIONS to tools.allowed.
                            If False, remove them.

    Returns True on success, False on failure.
    """
    # Guard: block only if version detected AND too old (schema incompatible)
    # If version can't be detected, proceed optimistically
    version = get_gemini_version()
    if version is not None and version < GEMINI_MIN_VERSION:
        # Version too old - don't install hooks (would corrupt settings)
        return False

    settings_path = get_gemini_settings_path()
    settings_path.parent.mkdir(parents=True, exist_ok=True)

    # Load existing settings
    try:
        settings = load_gemini_settings(settings_path, default={})
        if settings is None:
            settings = {}
    except (json.JSONDecodeError, PermissionError):
        settings = {}

    # Remove existing hcom hooks (clean slate)
    _remove_hcom_hooks_from_gemini_settings(settings)

    # Ensure hooks are enabled
    if "tools" not in settings:
        settings["tools"] = {}
    settings["tools"]["enableHooks"] = True

    # Handle hcom permission patterns based on include_permissions flag
    if include_permissions:
        if "allowed" not in settings["tools"]:
            settings["tools"]["allowed"] = []
        for pattern in GEMINI_HCOM_PERMISSIONS:
            if pattern not in settings["tools"]["allowed"]:
                settings["tools"]["allowed"].append(pattern)
    else:
        # Remove hcom permissions if disabled (both hcom and uvx hcom variants)
        if "allowed" in settings["tools"]:
            all_hcom_patterns = _build_all_permission_patterns("gemini")
            settings["tools"]["allowed"] = [p for p in settings["tools"]["allowed"] if p not in all_hcom_patterns]
            if not settings["tools"]["allowed"]:
                del settings["tools"]["allowed"]

    # Note: skipNextSpeakerCheck defaults to true in Gemini CLI v0.24+.
    # Previously we set it to false because early versions had a bug where
    # skipNextSpeakerCheck=true caused an early return that skipped AfterAgent.
    # This was fixed in commit 15c9f88da (Dec 2025) - AfterAgent now fires
    # after processTurn regardless of this setting. Leaving it at default (true)
    # prevents verbose output loops caused by the next speaker check triggering
    # "Please continue" when the model says "I will do X".

    # Find hcom command (respects uvx / python -m contexts)
    # HCOM_DIR is inherited from container/shell environment - not baked into command
    hcom_cmd = build_hcom_command()

    # Set hooksConfig.enabled and clean up legacy hooks.enabled if present
    _set_hooks_enabled(settings)

    # Ensure hooks dict exists for adding hook entries
    if not isinstance(settings.get("hooks"), dict):
        settings["hooks"] = {}

    for hook_type, matcher, cmd_suffix, timeout, description in GEMINI_HOOK_CONFIGS:
        hook_name = f"hcom-{hook_type.lower()}"
        hook_entry = {
            "matcher": matcher,
            "hooks": [
                {
                    "name": hook_name,
                    "type": "command",
                    "command": f"{hcom_cmd} {cmd_suffix}",
                    "timeout": timeout,
                    "description": description,
                }
            ],
        }

        if hook_type not in settings["hooks"] or not isinstance(settings["hooks"][hook_type], list):
            settings["hooks"][hook_type] = []
        settings["hooks"][hook_type].append(hook_entry)

    # Write settings atomically
    if not atomic_write(settings_path, json.dumps(settings, indent=2)):
        return False

    return verify_gemini_hooks_installed(check_permissions=include_permissions)


def _verify_gemini_hooks_at(settings_path: Path, check_permissions: bool = True) -> bool:
    """Verify hcom hooks at a specific settings path. Returns True if all checks pass."""
    try:
        settings = load_gemini_settings(settings_path, default=None)
        if not isinstance(settings, dict) or not settings:
            return False

        # Hooks must be enabled (Gemini schema uses tools.enableHooks; accept legacy enableHooks too)
        enable_hooks = None
        tools = settings.get("tools")
        if isinstance(tools, dict):
            enable_hooks = tools.get("enableHooks")
        if enable_hooks is None:
            enable_hooks = settings.get("enableHooks")  # legacy schema
        if enable_hooks is not True:
            return False

        # Note: skipNextSpeakerCheck no longer enforced (bug was fixed in Gemini CLI)
        # Previously required False, now left at default (true) to avoid verbose loops

        # Check hooksConfig.enabled is set
        if not _is_hooks_enabled(settings):
            return False

        # Check all hook types
        hooks = settings.get("hooks", {})
        if not isinstance(hooks, dict):
            return False

        # Build expected command (HCOM_DIR inherited from environment)
        expected_hcom_cmd = build_hcom_command()

        for (
            hook_type,
            expected_matcher,
            cmd_suffix,
            expected_timeout,
            _,
        ) in GEMINI_HOOK_CONFIGS:
            if hook_type not in hooks:
                return False

            # Find hcom hook for this type
            hcom_hook_found = False
            expected_command = f"{expected_hcom_cmd} {cmd_suffix}"
            expected_name = f"hcom-{hook_type.lower()}"

            hook_matchers = hooks.get(hook_type, [])
            if not isinstance(hook_matchers, list) or not hook_matchers:
                return False

            for matcher_dict in hook_matchers:
                if not isinstance(matcher_dict, dict):
                    continue

                actual_matcher = matcher_dict.get("matcher", "")
                matcher_hooks = matcher_dict.get("hooks", [])
                if not isinstance(matcher_hooks, list):
                    continue

                for hook in matcher_hooks:
                    if not isinstance(hook, dict):
                        continue
                    if _is_hcom_hook(hook):
                        if hcom_hook_found:
                            return False  # Duplicate
                        if actual_matcher != expected_matcher:
                            return False
                        if hook.get("type") != "command":
                            return False
                        if hook.get("name") != expected_name:
                            return False
                        if hook.get("timeout") != expected_timeout:
                            return False
                        if hook.get("command") != expected_command:
                            return False
                        hcom_hook_found = True

            if not hcom_hook_found:
                return False

        # Check permissions if enabled
        if check_permissions:
            allowed = settings.get("tools", {}).get("allowed", [])
            for pattern in GEMINI_HCOM_PERMISSIONS:
                if pattern not in allowed:
                    return False

        return True
    except (json.JSONDecodeError, OSError):
        return False


def verify_gemini_hooks_installed(check_permissions: bool = True) -> bool:
    """Verify that hcom hooks are correctly installed in Gemini settings.

    Checks:
    - Hooks enabled flag set in correct location (version-aware)
    - All hook types from GEMINI_HOOK_CONFIGS exist
    - Each has exactly one hcom hook with correct command
    - If check_permissions, all GEMINI_HCOM_PERMISSIONS are in tools.allowed

    Args:
        check_permissions: If True, verify permissions are installed too.

    Returns True if all checks pass.
    """
    return _verify_gemini_hooks_at(get_gemini_settings_path(), check_permissions=check_permissions)


def _remove_gemini_hooks_from_path(settings_path: Path) -> bool:
    """Remove hcom hooks from a specific settings path. Returns True on success."""
    if not settings_path.exists():
        return True

    try:
        settings = load_gemini_settings(settings_path, default=None)
        if not isinstance(settings, dict) or not settings:
            return True

        _remove_hcom_hooks_from_gemini_settings(settings)

        # Write atomically
        return atomic_write(settings_path, json.dumps(settings, indent=2))
    except (json.JSONDecodeError, OSError):
        return False


def remove_gemini_hooks() -> bool:
    """Remove hcom hooks from Gemini settings.

    Cleans both:
    - Global: ~/.gemini/settings.json
    - Local: get_gemini_settings_path() (same path as install)

    Only removes hcom-specific hooks, not the whole file.
    Returns True on success or if no changes needed.
    """
    global_path = Path.home() / ".gemini" / "settings.json"
    local_path = get_gemini_settings_path()  # Use same path as install

    global_ok = _remove_gemini_hooks_from_path(global_path)
    # Only remove local if different from global
    if local_path != global_path:
        local_ok = _remove_gemini_hooks_from_path(local_path)
    else:
        local_ok = True
    return global_ok and local_ok


__all__ = [
    "setup_gemini_hooks",
    "remove_gemini_hooks",
    "verify_gemini_hooks_installed",
    "get_gemini_settings_path",
    "load_gemini_settings",
    "ensure_hooks_enabled",
    "get_gemini_version",
    "is_gemini_version_supported",
    "GEMINI_MIN_VERSION",
    "GEMINI_HOOK_CONFIGS",
    "GEMINI_HOOK_TYPES",
]
