"""Codex CLI hook settings management.

Manages ~/.codex/config.toml for hcom hook integration.
Codex uses TOML format (not JSON like Gemini) with simpler structure.

Config Structure:
    # ~/.codex/config.toml
    notify = ["hcom", "codex-notify"]  # Must be at root level, before [sections]

    [model]
    ...

Execpolicy Rules (~/.codex/rules/hcom.rules):
    Auto-approve safe hcom commands (list, events, send, etc.)
    to reduce approval prompts during normal hcom usage.

Key Functions:
    setup_codex_hooks: Install notify hook and execpolicy rules
    remove_codex_hooks: Clean removal from config and rules
    verify_codex_hooks_installed: Check correct configuration

Important: Codex only supports ONE notify command. If user has existing
notify configuration, setup will fail with instructions to remove it first.
"""

from __future__ import annotations
import re
import sys
import shlex
from pathlib import Path

from ...core.tool_utils import (
    build_hcom_command,
    build_codex_rules,
)
from ...core.paths import atomic_write

# ==================== Hook Configuration ====================

# Hook configuration: (hook_name, command_suffix, description)
# Single source of truth - Codex only supports one notify hook
CODEX_HOOK_CONFIGS = [
    ("notify", "codex-notify", "Signal turn completion for message injection"),
]

# Derived from CODEX_HOOK_CONFIGS
CODEX_HOOK_COMMANDS = [cfg[1] for cfg in CODEX_HOOK_CONFIGS]


# ==================== Codex Settings Access ====================


def get_codex_config_path() -> Path:
    """Get path to Codex config file.

    If HCOM_DIR is set (sandbox), uses HCOM_DIR parent (e.g., /workspace/.codex/config.toml).
    Otherwise uses global (~/.codex/config.toml).
    """
    from ...core.paths import get_project_root

    return get_project_root() / ".codex" / "config.toml"


def _is_hcom_notify_line(line: str) -> bool:
    """Check if a TOML line is an hcom notify configuration.

    Handles both TOML formats:
    - Array: notify = ["hcom", "codex-notify"]
    - String: notify = "hcom codex-notify"

    Detection strategy:
    - "hcom" as exact token catches current command
    - "codex-notify" catches stale commands (renamed/moved hcom)
    """
    stripped = line.strip()
    if not stripped.startswith("notify"):
        return False
    lower = stripped.lower()
    # Check for "hcom" as exact token (array: "hcom", string: hcom )
    if '"hcom"' in lower or "'hcom'" in lower or "hcom " in lower:
        return True
    # Fallback: codex-notify is hcom-specific subcommand (catches stale commands)
    return "codex-notify" in lower


# ==================== Execpolicy Configuration ====================


def get_codex_rules_path() -> Path:
    """Get path to Codex execpolicy rules directory.

    If HCOM_DIR is set (sandbox), uses HCOM_DIR parent.
    Otherwise uses global (~/.codex/rules).
    """
    from ...core.paths import get_project_root

    return get_project_root() / ".codex" / "rules"


def setup_codex_execpolicy() -> bool:
    """Create execpolicy rule to auto-approve safe hcom commands.

    Location determined by HCOM_DIR:
    - If HCOM_DIR set → install to HCOM_DIR_parent/.codex/rules/hcom.rules
    - Otherwise → install to ~/.codex/rules/hcom.rules

    Not allowed (will prompt): reset, N codex/gemini (spawning)

    Returns True on success, False on failure.
    """
    rules_dir = get_codex_rules_path()
    rules_file = rules_dir / "hcom.rules"

    # Use centralized rule generation from core/tool_utils.py
    rule_content = "\n".join(build_codex_rules()) + "\n"

    try:
        # Check if already configured correctly (exact match)
        if rules_file.exists():
            existing = rules_file.read_text()
            if existing == rule_content:
                return True  # Already configured correctly
            # Wrong format - regenerate

        rules_dir.mkdir(parents=True, exist_ok=True)
        return atomic_write(rules_file, rule_content)
    except Exception as e:
        print(f"WARN: Failed to setup Codex execpolicy: {e}", file=sys.stderr)
        return False


def _remove_codex_execpolicy_from_path(rules_dir: Path) -> bool:
    """Remove hcom execpolicy rule from a specific path. Returns True on success."""
    rules_file = rules_dir / "hcom.rules"
    try:
        if rules_file.exists():
            rules_file.unlink()
        return True
    except Exception:
        return False


def remove_codex_execpolicy() -> bool:
    """Remove hcom execpolicy rule.

    Location determined by HCOM_DIR.
    Returns True on success or if no changes needed.
    """
    return _remove_codex_execpolicy_from_path(get_codex_rules_path())


# ==================== Setup and Verification ====================


def _build_expected_notify_line() -> str:
    """Build the expected notify line based on current execution context.

    Codex execs notify as argv array (no shell).
    HCOM_DIR is inherited from container/shell environment - not baked into command.
    """
    hcom_cmd = build_hcom_command()

    # Split into argv array
    cmd_parts = shlex.split(hcom_cmd)
    cmd_parts.append("codex-notify")

    # Build TOML array string
    notify_array = ", ".join(f'"{p}"' for p in cmd_parts)
    return f"notify = [{notify_array}]"


def _extract_notify_line(content: str) -> str | None:
    """Extract the notify line from config content, or None if not found."""
    for line in content.splitlines():
        line_stripped = line.strip()
        if line_stripped.startswith("#") or line_stripped.startswith("["):
            continue
        if line_stripped.startswith("notify") and "=" in line_stripped:
            return line_stripped
    return None


def setup_codex_hooks(include_permissions: bool = True) -> bool:
    """Set up Codex notify hook in config.

    Location determined by HCOM_DIR:
    - If HCOM_DIR set → install to HCOM_DIR_parent/.codex/config.toml
    - Otherwise → install to ~/.codex/config.toml

    Adds to config.toml:
        notify = ["hcom", "codex-notify"]  # or ["uvx", "hcom", "codex-notify"] for uvx

    IMPORTANT: notify must be at root level BEFORE any [section] headers,
    otherwise TOML parser treats it as part of the section.

    If hcom notify exists but command doesn't match current context, updates it.

    Args:
        include_permissions: If True, setup execpolicy rules for auto-approval.
                            If False, remove them.

    Returns:
        True if setup successful, False otherwise
    """
    config_path = get_codex_config_path()
    notify_line = _build_expected_notify_line()

    try:
        if config_path.exists():
            content = config_path.read_text()

            # Check if notify already configured
            existing_notify = _extract_notify_line(content)
            if existing_notify:
                # Check if it's our hook with correct command
                if "codex-notify" in existing_notify:
                    if existing_notify == notify_line:
                        # Notify already correct, but still need to handle execpolicy
                        if include_permissions:
                            setup_codex_execpolicy()
                        else:
                            remove_codex_execpolicy()
                        return True
                    # Our hook but stale command - remove and re-add
                    remove_codex_hooks()
                    content = config_path.read_text()  # Re-read after removal
                else:
                    # Different notify configured - error and don't override
                    print(
                        f"ERROR: {config_path} already has a notify hook configured.",
                        file=sys.stderr,
                    )
                    print(
                        "Codex only supports one notify command. To use hcom:",
                        file=sys.stderr,
                    )
                    print(
                        "  1. Remove or comment out the existing 'notify = ...' line",
                        file=sys.stderr,
                    )
                    print("  2. Run 'hcom 1 codex' again", file=sys.stderr)
                    return False

            # Insert notify BEFORE first [section] header (critical for TOML parsing)
            section_match = re.search(r"^\[", content, re.MULTILINE)
            if section_match:
                # Insert before first section
                pos = section_match.start()
                content = content[:pos] + f"# hcom integration\n{notify_line}\n\n" + content[pos:]
            else:
                # No sections, append at end
                content = content.rstrip() + f"\n\n# hcom integration\n{notify_line}\n"
            atomic_write(config_path, content)
        else:
            # Create new config
            config_path.parent.mkdir(parents=True, exist_ok=True)
            atomic_write(config_path, f"# Codex config\n\n# hcom integration\n{notify_line}\n")

        # Handle execpolicy based on include_permissions flag
        if include_permissions:
            setup_codex_execpolicy()
        else:
            remove_codex_execpolicy()

        return True
    except Exception as e:
        print(f"ERROR: Failed to setup Codex hooks: {e}", file=sys.stderr)
        return False


def _verify_codex_hooks_at(config_path: Path, check_permissions: bool = True) -> bool:
    """Verify hcom hooks at a specific config path. Returns True if all checks pass."""
    if not config_path.exists():
        return False

    try:
        content = config_path.read_text()
        existing_notify = _extract_notify_line(content)

        if not existing_notify:
            return False

        # Must be our hook AND match current command exactly
        if "codex-notify" not in existing_notify:
            return False

        expected_notify = _build_expected_notify_line()
        if existing_notify != expected_notify:
            return False

        # Check execpolicy rules if permissions enabled
        if check_permissions:
            rules_path = get_codex_rules_path() / "hcom.rules"
            if not rules_path.exists():
                return False

        return True
    except Exception:
        return False


def verify_codex_hooks_installed(check_permissions: bool = True) -> bool:
    """Verify that hcom hooks are correctly installed in Codex config.

    Checks:
    - Config file exists
    - notify key exists at root level
    - notify command matches current execution context exactly
    - If check_permissions, execpolicy rules file exists

    Args:
        check_permissions: If True, verify execpolicy rules are installed too.

    Returns True if all checks pass.
    """
    return _verify_codex_hooks_at(get_codex_config_path(), check_permissions=check_permissions)


def _remove_codex_hooks_from_path(config_path: Path) -> bool:
    """Remove hcom hooks from a specific config path. Returns True on success."""
    if not config_path.exists():
        return True

    try:
        content = config_path.read_text()
        lines = content.splitlines()
        new_lines = []
        skip_next_blank = False

        for line in lines:
            # Check if this is our notify line or comment
            if _is_hcom_notify_line(line):
                skip_next_blank = True
                continue
            if line.strip() == "# hcom integration":
                skip_next_blank = True
                continue
            if skip_next_blank and not line.strip():
                skip_next_blank = False
                continue
            skip_next_blank = False
            new_lines.append(line)

        # Write back atomically
        return atomic_write(config_path, "\n".join(new_lines))
    except Exception:
        return False


def remove_codex_hooks() -> bool:
    """Remove hcom hooks from Codex config.

    Cleans both:
    - Global: ~/.codex/config.toml
    - Local: get_codex_config_path() (same path as install)

    Also removes execpolicy rules from both locations.
    Returns True on success or if no changes needed.
    """
    global_config = Path.home() / ".codex" / "config.toml"
    local_config = get_codex_config_path()  # Use same path as install
    global_rules = Path.home() / ".codex" / "rules"
    local_rules = get_codex_rules_path()  # Use same path as install

    # Remove execpolicy from global
    _remove_codex_execpolicy_from_path(global_rules)
    # Remove execpolicy from local if different
    if local_rules != global_rules:
        _remove_codex_execpolicy_from_path(local_rules)

    # Remove hooks from global
    global_ok = _remove_codex_hooks_from_path(global_config)
    # Remove hooks from local if different
    if local_config != global_config:
        local_ok = _remove_codex_hooks_from_path(local_config)
    else:
        local_ok = True
    return global_ok and local_ok


__all__ = [
    "setup_codex_hooks",
    "verify_codex_hooks_installed",
    "remove_codex_hooks",
    "get_codex_config_path",
    "setup_codex_execpolicy",
    "remove_codex_execpolicy",
    "get_codex_rules_path",
    "CODEX_HOOK_CONFIGS",
    "CODEX_HOOK_COMMANDS",
]
