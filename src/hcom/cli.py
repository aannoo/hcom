#!/usr/bin/env python3
"""
hcom
CLI tool for launching multiple Claude Code terminals with interactive subagents, headless persistence, and real-time communication via hooks
"""

import os
import sys
import json
import io
import tempfile
import shutil
import shlex
import re
import subprocess
import time
import select
import platform
import random
from pathlib import Path
from datetime import datetime, timedelta
from typing import Any, Callable, NamedTuple, TextIO
from dataclasses import dataclass
from contextlib import contextmanager

if os.name == 'nt':
    import msvcrt
else:
    import fcntl

# Import from shared module
from .shared import (
    __version__,
    IS_WINDOWS,
    is_wsl,
)

# Import terminal launching
from .terminal import launch_terminal

# Import core utilities
from .core.paths import (
    hcom_path,
    ensure_hcom_directories,
    atomic_write,
    FLAGS_DIR,
)
from .hooks import (
    handle_hook,
    HOOK_CONFIGS,
    get_claude_settings_path,
    load_settings_json,
    _remove_hcom_hooks_from_settings,
)
from .hooks.utils import (
    build_hcom_command,
    _build_quoted_invocation,
)
from .core.runtime import build_claude_env

# Import command implementations
from .commands import (
    cmd_launch,
    cmd_stop,
    cmd_start,
    cmd_send,
    cmd_events,
    cmd_reset,
    cmd_help,
    cmd_list,
    cmd_relay,
    cmd_config,
    CLIError,
    format_error,
)

# Commands that support --help (maps to cmd_* functions)
COMMANDS = ('events', 'send', 'stop', 'start', 'reset', 'list', 'config', 'relay')


def _run_command(name: str, argv: list[str]) -> int:
    """Run command with --help support."""
    if argv and argv[0] in ('--help', '-h'):
        from .commands.utils import get_command_help
        print(get_command_help(name))
        return 0
    return globals()[f'cmd_{name}'](argv)

# Note: Removed backward compat aliases (_parse_env_value, etc.) - not used by any code or tests

if sys.version_info < (3, 10):
    sys.exit("Error: hcom requires Python 3.10 or higher")

# ==================== Constants ====================
# Platform detection, message patterns, sender constants moved to shared.py (imported above)
# STATUS_MAP and status constants in shared.py (imported above)
# ANSI codes in shared.py (imported above)

# ==================== Error Handling Strategy ====================
# CLI: Can raise exceptions for user feedback. Check return values.
# Critical I/O: atomic_write

# ==================== CLI Errors ====================
# CLIError and get_help_text moved to commands/utils.py


# ==================== Logging ====================

# ==================== Config Defaults ====================
# Config precedence: env var > ~/.hcom/config.env > defaults
# All config via HcomConfig dataclass (timeout, terminal, hints, tag, claude_args)

# Hook configuration now in hooks/settings.py

# ==================== Configuration System ====================
# Config classes and functions now in core/config.py

def get_hook_command() -> tuple[str, dict[str, Any]]:
    """Get hook command - hooks always run, Python code gates participation

    Uses ${HCOM} environment variable set in settings.json, with fallback to direct python invocation.
    Participation is controlled by enabled flag in instance JSON files.

    Windows uses direct invocation because hooks in settings.json run in CMD/PowerShell context,
    not Git Bash, so ${HCOM} shell variable expansion doesn't work (would need %HCOM% syntax).
    """
    if IS_WINDOWS:
        # Windows: hooks run in CMD context, can't use ${HCOM} syntax
        return _build_quoted_invocation(), {}
    else:
        # Unix: Use HCOM env var from settings.json
        return '${HCOM}', {}

def _parse_version(v: str) -> tuple:
    """Parse version string to comparable tuple"""
    return tuple(int(x) for x in v.split('.') if x.isdigit())

def get_update_notice() -> str | None:
    """Check PyPI for updates (once daily), return message if available"""
    flag = hcom_path(FLAGS_DIR, 'update_available')

    # Check PyPI if flag missing or >24hrs old
    should_check = not flag.exists() or time.time() - flag.stat().st_mtime > 86400

    if should_check:
        try:
            import urllib.request
            with urllib.request.urlopen('https://pypi.org/pypi/hcom/json', timeout=2) as f:
                latest = json.load(f)['info']['version']

            if _parse_version(latest) > _parse_version(__version__):
                atomic_write(flag, latest)  # mtime = cache timestamp
            else:
                flag.unlink(missing_ok=True)
                return None
        except Exception:
            pass  # Network error, use cached value if exists

    # Return message if update available
    if not flag.exists():
        return None

    try:
        latest = flag.read_text().strip()
        # Double-check version (handles manual upgrades)
        if _parse_version(__version__) >= _parse_version(latest):
            flag.unlink(missing_ok=True)
            return None

        # Inline check: if running from uv-managed Python with uvx available
        cmd = "uv tool upgrade hcom" if ('uv' in Path(sys.executable).resolve().parts and shutil.which('uvx')) else "pip install -U hcom"
        return f"→ Update available: hcom v{latest} ({cmd})"
    except Exception:
        return None

def _build_hcom_env_value() -> str:
    """Build the value for settings['env']['HCOM'] based on current execution context
    Uses build_hcom_command() without caching for fresh detection on every call.
    """
    return build_hcom_command()

# ==================== Message System ====================
# validate_message moved to commands/utils.py

# ==================== Identity Management ====================
# get_display_name, resolve_instance_name moved to core/instances.py

# ==================== Hook Management ====================
# get_claude_settings_path, load_settings_json, _remove_hcom_hooks_from_settings moved to hooks/settings.py

# build_env_string, build_claude_command moved to terminal.py
# create_bash_script, find_bash_on_windows moved to terminal.py
# get_macos_terminal_argv, get_windows_terminal_argv, get_linux_terminal_argv moved to terminal.py
# windows_hidden_popen, _parse_terminal_command, launch_terminal moved to terminal.py

def setup_hooks() -> bool:
    """Set up Claude hooks globally in ~/.claude/settings.json"""
    # Install to global user settings
    settings_path = get_claude_settings_path()
    settings_path.parent.mkdir(exist_ok=True)
    try:
        settings = load_settings_json(settings_path, default={})
        if settings is None:
            settings = {}
    except (json.JSONDecodeError, PermissionError) as e:
        raise Exception(format_error(f"Cannot read settings: {e}"))
    
    if 'hooks' not in settings:
        settings['hooks'] = {}

    _remove_hcom_hooks_from_settings(settings)
        
    # Get the hook command template
    hook_cmd_base, _ = get_hook_command()

    # Build hook commands from HOOK_CONFIGS
    hook_configs = [
        (hook_type, matcher, f'{hook_cmd_base} {cmd_suffix}', timeout)
        for hook_type, matcher, cmd_suffix, timeout in HOOK_CONFIGS
    ]

    for hook_type, matcher, command, timeout in hook_configs:
        if hook_type not in settings['hooks']:
            settings['hooks'][hook_type] = []

        hook_dict = {
            'hooks': [{
                'type': 'command',
                'command': command
            }]
        }

        # Only include matcher field if non-empty (PreToolUse/PostToolUse use matchers)
        if matcher:
            hook_dict['matcher'] = matcher

        if timeout is not None:
            hook_dict['hooks'][0]['timeout'] = timeout

        settings['hooks'][hook_type].append(hook_dict)

    # Set $HCOM environment variable for all Claude instances (vanilla + hcom-launched)
    if 'env' not in settings:
        settings['env'] = {}

    # Set HCOM based on current execution context (uvx, hcom binary, or full path)
    settings['env']['HCOM'] = _build_hcom_env_value()

    # Write settings atomically
    try:
        atomic_write(settings_path, json.dumps(settings, indent=2))
    except Exception as e:
        raise Exception(format_error(f"Cannot write settings: {e}"))
    
    # Quick verification
    if not verify_hooks_installed(settings_path):
        raise Exception(format_error("Hook installation failed"))
    
    return True

def verify_hooks_installed(settings_path: Path) -> bool:
    """Verify that HCOM hooks were installed correctly with correct commands, timeouts, and matchers"""
    try:
        settings = load_settings_json(settings_path, default=None)
        if not settings:
            return False

        # Check all hook types have correct commands, timeout values, and matchers (exactly one HCOM hook per type)
        # Derive from HOOK_CONFIGS (single source of truth)
        hooks = settings.get('hooks', {})
        for hook_type, expected_matcher, cmd_suffix, expected_timeout in HOOK_CONFIGS:
            hook_matchers = hooks.get(hook_type, [])
            if not hook_matchers:
                return False

            # Find and verify HCOM hook for this type
            hcom_hook_found = False
            for matcher_dict in hook_matchers:
                for hook in matcher_dict.get('hooks', []):
                    command = hook.get('command', '')
                    # Check for HCOM and the correct subcommand
                    if ('${HCOM}' in command or 'hcom' in command.lower()) and cmd_suffix in command:
                        # Found HCOM hook - verify all properties
                        if hcom_hook_found:
                            # Duplicate HCOM hook
                            return False

                        # Verify timeout matches
                        actual_timeout = hook.get('timeout')
                        if actual_timeout != expected_timeout:
                            return False

                        # Verify matcher matches. matcher_dict.get('matcher', '') returns '' if key missing
                        actual_matcher = matcher_dict.get('matcher', '')
                        if actual_matcher != expected_matcher:
                            return False

                        hcom_hook_found = True

            # Must have exactly one HCOM hook with correct properties
            if not hcom_hook_found:
                return False

        # Check that HCOM env var is set
        env = settings.get('env', {})
        if 'HCOM' not in env:
            return False

        return True
    except Exception:
        return False

# is_interactive, get_archive_timestamp, should_show_in_watch moved to commands/admin.py
# initialize_instance_in_position_file, enable_instance moved to core/instances.py

# ==================== Command Functions ====================
# Command functions moved to commands/ package

def ensure_hooks_current() -> bool:
    """Ensure hooks match current execution context - called on EVERY command.
    Auto-updates hooks if execution context changes (e.g., pip → uvx).
    Always returns True (warns but never blocks - Claude Code is fault-tolerant)."""

    # Skip global hook installation in web environments with project hooks
    # (prevents duplicate hook execution after restart)
    if os.environ.get('CLAUDE_CODE_REMOTE') == 'true':
        project_settings = Path('.claude/settings.json')
        if project_settings.exists() and verify_hooks_installed(project_settings):
            # Project has HCOM hooks configured - don't install global hooks
            return True

    # Verify hooks exist and match current execution context
    global_settings = get_claude_settings_path()

    # Check if hooks are valid (exist + env var matches current context)
    hooks_exist = verify_hooks_installed(global_settings)
    env_var_matches = False

    if hooks_exist:
        try:
            settings = load_settings_json(global_settings, default={})
            if settings is None:
                settings = {}
            current_hcom = _build_hcom_env_value()
            installed_hcom = settings.get('env', {}).get('HCOM')
            env_var_matches = (installed_hcom == current_hcom)
        except Exception:
            # Failed to read settings - try to fix by updating
            env_var_matches = False

    # Install/update hooks if missing or env var wrong
    if not hooks_exist or not env_var_matches:
        try:
            setup_hooks()
            if os.environ.get('CLAUDECODE') == '1':
                print("hcom hooks updated. Please restart Claude Code to apply changes.", file=sys.stderr)
                print("=" * 60, file=sys.stderr)
        except Exception as e:
            # Failed to verify/update hooks, but they might still work
            # Claude Code is fault-tolerant with malformed JSON
            print(f"Warning: Could not verify/update hooks: {e}", file=sys.stderr)
            print("If hcom doesn't work, check ~/.claude/settings.json", file=sys.stderr)

    return True

# ==================== Main Entry Point ====================

def check_and_migrate_legacy_messages() -> None:
    """Check for messages without scope field and automatically migrate.

    Automatically archives and clears old messages, then allows command to proceed.
    Wrapped in try/except to not crash on DB errors.
    """
    try:
        from .core.db import get_db
        from .commands.admin import clear

        conn = get_db()

        # Query for messages with old format:
        # - Missing scope field (pre-0.6.4)
        # - Old field names: mentioned_instances (now: mentions), recipients (now: delivered_to)
        result = conn.execute("""
            SELECT COUNT(*) as count
            FROM events
            WHERE type = 'message'
              AND (json_extract(data, '$.scope') IS NULL
                   OR json_extract(data, '$.mentioned_instances') IS NOT NULL
                   OR json_extract(data, '$.recipients') IS NOT NULL)
        """).fetchone()

        if result and result['count'] > 0:
            print("\n" + "=" * 60, file=sys.stderr)
            print("hcom v0.6.4 Auto-Migration", file=sys.stderr)
            print("=" * 60, file=sys.stderr)
            print(f"\nFound {result['count']} message(s) in old format.", file=sys.stderr)
            print("Archiving and clearing old database...", file=sys.stderr)

            # Perform automatic reset (calls same logic as 'hcom reset logs')
            clear()

            print("✓ Migration complete. Continuing with command...", file=sys.stderr)
            print("=" * 60 + "\n", file=sys.stderr)

    except Exception as e:
        # DB error - don't block CLI, just warn
        print(f"Warning: Could not migrate legacy messages: {e}", file=sys.stderr)

def main(argv: list[str] | None = None) -> int | None:
    """Main command dispatcher"""
    # Apply UTF-8 encoding for Windows and WSL (Git Bash, MSYS use cp1252 by default)
    if IS_WINDOWS or is_wsl():
        try:
            if not isinstance(sys.stdout, io.TextIOWrapper) or sys.stdout.encoding != 'utf-8':
                sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8')
            if not isinstance(sys.stderr, io.TextIOWrapper) or sys.stderr.encoding != 'utf-8':
                sys.stderr = io.TextIOWrapper(sys.stderr.buffer, encoding='utf-8')
        except (AttributeError, OSError):
            pass  # Fallback if stream redirection fails

    if argv is None:
        argv = sys.argv[1:]
    else:
        argv = argv[1:] if len(argv) > 0 and argv[0].endswith('hcom.py') else argv

    # Hook handlers only (called BY hooks, not users)
    if argv and argv[0] in ('poll', 'notify', 'pre', 'post', 'sessionstart', 'userpromptsubmit', 'sessionend', 'subagent-start', 'subagent-stop'):
        handle_hook(argv[0])
        return 0

    # Ensure directories exist first (required for version check cache)
    if not ensure_hcom_directories():
        print(format_error("Failed to create HCOM directories"), file=sys.stderr)
        return 1

    # Check for updates and show message if available (once daily check, persists until upgrade)
    if msg := get_update_notice():
        print(msg, file=sys.stderr)

    # Ensure hooks current (warns but never blocks)
    ensure_hooks_current()

    # Auto-migrate legacy messages (v0.6.3 → v0.6.4)
    check_and_migrate_legacy_messages()

    # Subagent context: require --agentid for all commands
    if argv and '--agentid' not in argv and os.environ.get('CLAUDECODE') == '1':
        try:
            from .commands.utils import resolve_identity
            from .hooks.subagent import in_subagent_context
            identity = resolve_identity()
            if identity.name and in_subagent_context(identity.name):
                print(format_error(
                    "Task subagent must provide --agentid",
                    f"Use: hcom {argv[0]} --agentid <agent_id> ..."
                ), file=sys.stderr)
                return 1
        except ValueError:
            pass  # Can't resolve identity - not relevant

    # Launch TUI in a new terminal window (equivalent to legacy 'watch --launch')
    if len(argv) == 1 and argv[0] == '--new-terminal':
        env = build_claude_env()
        hcom_cmd = build_hcom_command()
        success = launch_terminal(hcom_cmd, env, cwd=os.getcwd())
        return 0 if success else 1

    # Route to commands
    try:
        if not argv:
            # Launch interactive TUI
            from .ui import run_tui
            return run_tui(hcom_path())
        elif argv[0] in ('help', '--help', '-h'):
            return cmd_help()
        elif argv[0] in ('--version', '-v'):
            print(f"hcom {__version__}")
            return 0
        elif argv[0] in COMMANDS:
            return _run_command(argv[0], argv[1:])
        elif argv[0].isdigit() or argv[0] == 'claude':
            # Launch instances: hcom <1-100> [args] or hcom claude [args]
            return cmd_launch(argv)
        else:
            print(format_error(
                f"Unknown command: {argv[0]}",
                "Run 'hcom --help' for usage"
            ), file=sys.stderr)
            return 1
    except (CLIError, ValueError) as exc:
        print(str(exc), file=sys.stderr)
        return 1

# ==================== Exports for UI Module ====================
# Command functions (cmd_launch, cmd_start, cmd_stop, cmd_reset) now in commands/ package
# Utility functions (should_show_in_watch) now in commands/admin.py

__all__ = [
    # CLI-only functions (hook setup/verification)
    'setup_hooks',
    'verify_hooks_installed',
    'ensure_hooks_current',
    'get_hook_command',
]

if __name__ == '__main__':
    sys.exit(main())
