#!/usr/bin/env python3
"""
hcom
CLI tool for launching multiple AI terminals (Claude Code, Gemini, Codex) with
interactive subagents, headless persistence, and real-time communication via hooks
"""

import os
import sys

# ==================== Shim Detection (argv[0]) ====================
# If invoked as 'claude', 'gemini', or 'codex' (via symlink), rewrite argv.
# This enables `hcom shim install` to create symlinks that transparently wrap tools.
#
# BLACKLIST APPROACH: Only intercept session-starting invocations.
# Admin/config subcommands pass through to real binary (future-proof).
_SHIM_TOOLS = frozenset({"claude", "gemini", "codex"})

# Per-tool admin subcommands that should passthrough.
# NOTE: Duplicated from tools/*/args.py for performance (avoid import latency in hook path).
# Must be kept in sync with _SUBCOMMANDS in respective args.py files.
_SHIM_ADMIN_SUBCOMMANDS = {
    "claude": {"doctor", "mcp", "update", "install", "plugin", "setup-token"},
    "gemini": {"mcp", "extensions", "extension", "hooks", "hook"},
    "codex": {
        "mcp",
        "mcp-server",
        "app-server",
        "login",
        "logout",
        "completion",
        "sandbox",
        "debug",
        "apply",
        "a",
        "cloud",
        "features",
        "help",
        # Note: "resume" is interactive (resumes session in PTY), so NOT in admin list
        "review",  # review might be interactive too but keeping for now
    },
}

# Admin flags that should passthrough (all tools)
_SHIM_ADMIN_FLAGS = frozenset(
    {
        "--version",
        "-v",
        "--help",
        "-h",
        "--list-extensions",
        "-l",
        "--list-sessions",
        "--delete-session",  # Gemini
    }
)

_invoked_as = os.path.basename(sys.argv[0]) if sys.argv else ""
if _invoked_as in _SHIM_TOOLS:
    import shutil

    _shim_dir = str(os.path.dirname(sys.argv[0]))
    _first_arg = sys.argv[1] if len(sys.argv) > 1 else ""
    _first_arg_lower = _first_arg.lower()

    def _find_real_binary(tool: str) -> str | None:
        """Find real binary by excluding shim directory from PATH."""
        # Normalize shim_dir (resolve symlinks, remove trailing slash)
        _norm_shim = os.path.realpath(_shim_dir).rstrip("/")
        _path_parts = os.environ.get("PATH", "").split(":")
        # Exclude any path that resolves to shim dir
        _clean_path = ":".join(
            p for p in _path_parts if os.path.realpath(p).rstrip("/") != _norm_shim
        )
        _old_path = os.environ.get("PATH", "")
        os.environ["PATH"] = _clean_path
        _real = shutil.which(tool)
        os.environ["PATH"] = _old_path
        return _real

    def _passthrough() -> None:
        """Pass through to real binary."""
        _real = _find_real_binary(_invoked_as)
        if _real:
            os.execv(_real, [_real] + sys.argv[1:])
        else:
            print(
                f"Error: Real {_invoked_as} binary not found in PATH", file=sys.stderr
            )
            sys.exit(1)

    def _should_passthrough() -> bool:
        """Check if invocation should passthrough to real binary.

        Passthrough (real binary): admin/config commands
        Intercept (hcom wrap): session-starting invocations
        """
        if not _first_arg:
            return False  # No args = interactive session → intercept

        # Admin flags (--version, --help, etc.)
        if _first_arg in _SHIM_ADMIN_FLAGS or _first_arg_lower in _SHIM_ADMIN_FLAGS:
            return True

        # Tool-specific admin subcommands
        admin_cmds = _SHIM_ADMIN_SUBCOMMANDS.get(_invoked_as, set())
        if _first_arg_lower in admin_cmds:
            return True

        # Session-starting flags like --model, --resume, --continue should be intercepted
        # Only specific admin flags passthrough (already checked in _SHIM_ADMIN_FLAGS)
        return False  # Session-starting → intercept

    # Recursion guard: if HCOM_VIA_SHIM is set, we're being called from
    # a child process (e.g., PTY wrapper running 'claude'). Pass through to real binary.
    # Note: This guard is INSIDE `if _invoked_as in _SHIM_TOOLS:` block, so it only triggers
    # when invoked as claude/gemini/codex. Running `hcom send` won't hit this because
    # _invoked_as='hcom' skips the entire block.
    if os.environ.get("HCOM_VIA_SHIM"):
        _passthrough()

    # Check if should passthrough to real binary
    if _should_passthrough():
        _passthrough()

    # Intercept: mark with guard, bypass preview, rewrite args
    os.environ["HCOM_VIA_SHIM"] = "1"
    os.environ["HCOM_GO"] = "1"
    sys.argv = ["hcom", _invoked_as] + sys.argv[1:]

# ==================== Early Hook Routing ====================
# Gate and route hooks BEFORE heavy imports to save ~90ms for non-participants.
# Gate check uses only stdlib (sqlite3), dispatcher only loaded if gate passes.

_CLAUDE_HOOKS = frozenset(
    {
        "poll",
        "notify",
        "pre",
        "post",
        "sessionstart",
        "userpromptsubmit",
        "sessionend",
        "subagent-start",
        "subagent-stop",
    }
)
_GEMINI_HOOKS = frozenset(
    {
        "gemini-sessionstart",
        "gemini-beforeagent",
        "gemini-afteragent",
        "gemini-beforetool",
        "gemini-aftertool",
        "gemini-notification",
        "gemini-sessionend",
    }
)


def _hook_gate_check() -> bool:
    """Fast gate: should hooks proceed? Uses only stdlib."""
    # Fast path: hcom-launched always proceed
    if os.environ.get("HCOM_PROCESS_ID") or os.environ.get("HCOM_LAUNCHED") == "1":
        return True

    # Check if DB exists
    from pathlib import Path

    hcom_dir = os.environ.get("HCOM_DIR")
    if hcom_dir:
        db_path = Path(hcom_dir) / "hcom.db"
    else:
        db_path = Path.home() / ".hcom" / "hcom.db"

    if not db_path.exists():
        return False

    # Check if any instances registered
    try:
        import sqlite3

        conn = sqlite3.connect(str(db_path), timeout=1)
        cursor = conn.execute("SELECT 1 FROM instances LIMIT 1")
        has_instances = cursor.fetchone() is not None
        conn.close()
        return has_instances
    except Exception:
        return True  # DB error - let full dispatcher handle it


def _route_hook_early() -> bool:
    """Route hook commands before heavy imports. Returns True if handled."""
    if len(sys.argv) < 2:
        return False
    cmd = sys.argv[1]

    # Check if it's a hook command
    is_hook = cmd in _CLAUDE_HOOKS or cmd in _GEMINI_HOOKS or cmd == "codex-notify"
    if not is_hook:
        return False

    # Gate check - exit early for non-participants
    if not _hook_gate_check():
        sys.exit(0)

    # Gate passed - import and run dispatcher
    if cmd in _CLAUDE_HOOKS:
        from .hooks.dispatcher import handle_hook

        handle_hook(cmd)
        sys.exit(0)
    elif cmd in _GEMINI_HOOKS:
        from .tools.gemini.hooks import handle_gemini_hook

        handle_gemini_hook(cmd)
        sys.exit(0)
    elif cmd == "codex-notify":
        from .tools.codex.hooks import handle_codex_hook

        handle_codex_hook(cmd)
        sys.exit(0)
    return False


# Route hooks immediately at module load time
_route_hook_early()

# ==================== Regular Imports ====================
# Only reached for non-hook commands

import json
import io
import shutil
import time
from pathlib import Path

if os.name == "nt":
    pass
else:
    pass

# Import from shared module
from .shared import (
    __version__,
    IS_WINDOWS,
    is_wsl,
    is_inside_ai_tool,
    HcomError,
    CommandContext,
)

# ==================== Identity Gating ====================
# Commands that don't require identity
IDENTITY_ALLOWLIST = frozenset(
    {
        "start",  # Creates identity
        "help",  # No identity needed
        "reset",  # Admin command
        "stop",  # Admin command (stop all/name works without identity, self-stop validated in cmd)
        "run",  # Scripts handle their own identity requirements
        "status",  # Diagnostic command
        "relay",  # Works without identity
        "archive",  # Query-only, no identity needed
        "events",  # Query-only mode works without identity
        "transcript",  # Query-only (search, timeline, @name all work without identity)
        "config",  # View/edit config, no identity needed
        "list",  # Query-only, no identity needed
        "shim",  # System setup, no identity needed
        "hooks",  # Hook management, no identity needed
        "bundle",  # Query-only without identity
    }
)

# Flags that bypass identity requirement
IDENTITY_BYPASS_FLAGS = frozenset(
    {
        "--help",
        "-h",
        "--version",
        "-v",
        "--new-terminal",  # TUI launch in new terminal (same as bare 'hcom')
    }
)


def _extract_name_flag(argv: list[str]) -> tuple[list[str], str | None]:
    """Extract and strip a single global --name flag.

    Enforces: exactly one --name (errors if multiple).
    """
    if not argv:
        return argv, None

    argv = argv.copy()  # Don't mutate original
    name_idxs = [i for i, a in enumerate(argv) if a == "--name"]
    if len(name_idxs) > 1:
        raise CLIError("Multiple --name values provided; use exactly one.")
    if not name_idxs:
        return argv, None

    idx = name_idxs[0]
    if idx + 1 >= len(argv) or argv[idx + 1].startswith("-"):
        raise CLIError("--name requires a value")
    name_value = argv[idx + 1]
    del argv[idx : idx + 2]
    return argv, name_value


def _find_command(argv: list[str]) -> str | None:
    """Find command name in argv, skipping identity flags but returning global flags.

    Global flags (--help, --version, --new-terminal) are returned as commands.
    Identity flags (--name VALUE) are skipped.
    """
    # Global flags that should be treated as commands
    GLOBAL_FLAGS = {"--help", "-h", "--version", "-v", "--new-terminal"}

    i = 0
    while i < len(argv):
        arg = argv[i]
        if arg in GLOBAL_FLAGS:
            return arg  # Return global flag as command
        elif arg == "--name" and i + 1 < len(argv):
            i += 2  # skip --name and its value
        elif arg.startswith("-"):
            return None  # Unknown flag before command - let main() handle
        else:
            return arg  # Found command
    return None


def _get_command_args(argv: list[str], cmd: str) -> list[str]:
    """Get args for command, removing the command itself but keeping flags."""
    # ['--name', 'X', 'list', '-v'] → ['--name', 'X', '-v']
    if cmd in argv:
        idx = argv.index(cmd)
        return argv[:idx] + argv[idx + 1 :]
    return argv


def _strip_identity_flags(argv: list[str]) -> list[str]:
    """Remove identity flags (--name VALUE) from argv.

    Identity flags are handled by CLI layer, so launch commands receive clean argv.
    This allows flags to appear anywhere without order restrictions.
    Note: -b/--from are send-only (not global) and are not stripped here.
    """
    clean = []
    i = 0
    while i < len(argv):
        if argv[i] == "--name" and i + 1 < len(argv):
            i += 2  # Skip --name and its value
        else:
            clean.append(argv[i])
            i += 1
    return clean


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
    get_claude_settings_path,
    load_claude_settings,
    setup_claude_hooks,
    verify_claude_hooks_installed,
    remove_claude_hooks,
)
from .core.tool_utils import (
    build_hcom_command,
)
from .core.runtime import build_claude_env

# Import command implementations
from .commands import (
    cmd_launch,
    cmd_stop,  # noqa: F401 (used dynamically via globals())
    cmd_start,  # noqa: F401 (used dynamically via globals())
    cmd_kill,  # noqa: F401 (used dynamically via globals())
    cmd_send,  # noqa: F401 (used dynamically via globals())
    cmd_listen,  # noqa: F401 (used dynamically via globals())
    cmd_events,  # noqa: F401 (used dynamically via globals())
    cmd_reset,  # noqa: F401 (used dynamically via globals())
    cmd_help,
    cmd_list,  # noqa: F401 (used dynamically via globals())
    cmd_relay,  # noqa: F401 (used dynamically via globals())
    cmd_config,  # noqa: F401 (used dynamically via globals())
    cmd_transcript,  # noqa: F401 (used dynamically via globals())
    cmd_archive,  # noqa: F401 (used dynamically via globals())
    cmd_status,  # noqa: F401 (used dynamically via globals())
    cmd_shim,  # noqa: F401 (used dynamically via globals())
    cmd_hooks,  # noqa: F401 (used dynamically via globals())
    cmd_bundle,  # noqa: F401 (used dynamically via globals())
    CLIError,
    format_error,
)


def cmd_run(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Run a script from ~/.hcom/scripts/"""
    from .scripts import run_script

    # Scripts historically parse --name themselves; CLI strips global --name.
    # Re-inject for `hcom run` so scripts keep seeing it.
    if ctx and ctx.explicit_name:
        argv = ["--name", ctx.explicit_name] + argv
    return run_script(argv)


def _build_ctx_for_command(
    cmd: str | None, *, explicit_name: str | None
) -> CommandContext:
    """Build a CommandContext for this invocation (best-effort identity resolution).

    `start` is special: it may be invoked with `--name <agent_id>` before the
    instance exists (subagent registration), so the CLI must not resolve it.
    """
    identity = None
    if explicit_name and cmd != "start":
        from .core.identity import resolve_identity

        identity = resolve_identity(name=explicit_name)
    else:
        try:
            from .core.identity import resolve_identity

            identity = resolve_identity()
        except Exception:
            identity = None
    return CommandContext(explicit_name=explicit_name, identity=identity)


# Commands that support --help (maps to cmd_* functions)
COMMANDS = (
    "events",
    "send",
    "listen",
    "bundle",
    "stop",
    "start",
    "kill",
    "reset",
    "list",
    "config",
    "relay",
    "transcript",
    "archive",
    "run",
    "status",
    "shim",
    "hooks",
)

# Commands that should NOT trigger status update (handled internally or lifecycle)
_STATUS_SKIP_COMMANDS = frozenset(
    {"listen", "start", "stop", "kill", "reset", "status"}
)


def _set_hookless_command_status(
    cmd_name: str, *, ctx: CommandContext | None = None
) -> None:
    """Set status for instances without PreToolUse hooks before command runs.

    Claude/Gemini main instances have PreToolUse hooks that set active:tool:*.
    These instance types need explicit status updates here:
    - Subagent: has Claude hooks but not for hcom Bash commands
    - Codex: has notify hook (turn-end) but no pre-tool hook
    - Adhoc: no hooks at all

    Status model:
    - Adhoc: inactive:tool:* (no hooks to reset, just records "this happened")
    - Others: active:tool:* (hooks will reset to idle when turn ends)
    """
    if cmd_name in _STATUS_SKIP_COMMANDS:
        return

    try:
        from .core.instances import set_status

        identity = ctx.identity if ctx else None
        if identity is None:
            from .core.identity import resolve_identity

            identity = resolve_identity()

        if identity.kind != "instance" or not identity.instance_data:
            return

        tool = identity.instance_data.get("tool")
        has_parent = identity.instance_data.get("parent_name")

        # Only set status for hookless instances:
        # - subagent (has parent_name)
        # - codex
        # - adhoc
        # Claude/Gemini main have PreToolUse hooks
        is_hookless = has_parent or tool in ("codex", "adhoc")
        if not is_hookless:
            return

        if tool == "adhoc":
            # No hooks - can only claim "this event happened"
            set_status(identity.name, "inactive", f"tool:{cmd_name}")
        else:
            # Has hooks - will reset to idle when turn ends
            set_status(identity.name, "active", f"tool:{cmd_name}")

    except Exception:
        pass  # Best effort - don't break commands


def _run_command(
    name: str, argv: list[str], *, ctx: CommandContext | None = None
) -> int:
    """Run command with --help support."""
    # Check for --help anywhere in argv (not just position 0, since --name may precede it)
    # Exception: 'run' passes --help through to scripts
    if name != "run" and ("--help" in argv or "-h" in argv):
        from .commands.utils import get_command_help

        print(get_command_help(name))
        _maybe_deliver_pending_messages(argv, ctx=ctx)
        return 0
    _set_hookless_command_status(name, ctx=ctx)
    if name == "run":
        result = cmd_run(argv, ctx=ctx)
    else:
        result = globals()[f"cmd_{name}"](argv, ctx=ctx)
    _maybe_deliver_pending_messages(argv, ctx=ctx)
    return result


def _maybe_deliver_pending_messages(
    argv: list[str] | None = None, *, ctx: CommandContext | None = None
) -> None:
    """For hookless instances (codex/adhoc): append unread messages after command output.

    Codex and adhoc instances have no delivery hooks, so messages are delivered
    via CLI command output. This provides reliable delivery with proper read receipts.

    Called after most CLI commands. Not called for:
    - TUI mode (has own message handling)
    - Error paths (no point delivering on errors)
    - Hook handlers (internal, not user-facing)

    Skips appending for --json output to preserve machine-readable format.
    """
    # Skip for JSON output - would corrupt machine-readable format
    if argv and "--json" in argv:
        return

    from .shared import HcomError

    try:
        from .core.identity import resolve_identity

        identity = ctx.identity if ctx else None
        if identity is None:
            identity = resolve_identity()
    except (ValueError, HcomError):
        return  # No identity - expected when running from non-participant terminal

    if identity.kind != "instance" or not identity.instance_data:
        return

    instance_name = identity.name
    if identity.instance_data.get("tool") not in ("codex", "adhoc"):
        return  # Not codex/adhoc, silent skip

    # Get unread messages
    try:
        from .core.messages import get_unread_messages, format_hook_messages

        messages, _ = get_unread_messages(instance_name, update_position=True)
        if not messages:
            return

        # Format and print with divider
        print("\n" + "─" * 40)
        print("[hcom]")
        print("─" * 40)
        print(format_hook_messages(messages, instance_name))

        # Update status to show delivery
        # Codex: active (notify hook will set idle when done)
        # Adhoc: inactive (no hooks - just records "this happened")
        from .core.instances import set_status

        msg_ts = messages[-1].get("timestamp", "")
        tool = identity.instance_data.get("tool")
        if tool == "codex":
            set_status(
                instance_name, "active", f"deliver:{messages[0]['from']}", msg_ts=msg_ts
            )
        else:
            set_status(
                instance_name,
                "inactive",
                f"deliver:{messages[0]['from']}",
                msg_ts=msg_ts,
            )
    except Exception as e:
        from .core.log import log_error

        log_error("cli", "deliver.fail", e, instance=instance_name)


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

# get_hook_command moved to hooks/settings.py as _get_hook_command


def _parse_version(v: str) -> tuple:
    """Parse version string to comparable tuple"""
    return tuple(int(x) for x in v.split(".") if x.isdigit())


def _get_update_cmd() -> str:
    """Get the appropriate update command based on install method."""
    if "uv" in Path(sys.executable).resolve().parts and shutil.which("uvx"):
        return "uv tool upgrade hcom"
    return "pip install -U hcom"


def get_update_info() -> tuple[str | None, str | None]:
    """Check PyPI for updates (once daily cached).

    Returns:
        (latest_version, update_cmd) if update available, (None, None) otherwise.
    """
    flag = hcom_path(FLAGS_DIR, "update_check")

    # Check PyPI only if flag missing or >24hrs old (caches both success and failure)
    should_check = not flag.exists() or time.time() - flag.stat().st_mtime > 86400

    if should_check:
        latest = None
        try:
            import urllib.request

            with urllib.request.urlopen(
                "https://pypi.org/pypi/hcom/json", timeout=2
            ) as f:
                latest = json.load(f)["info"]["version"]
        except Exception:
            pass  # Network error - cache empty result

        # Cache result: version if update available, empty if not or failed
        if latest and _parse_version(latest) > _parse_version(__version__):
            atomic_write(flag, latest)
        else:
            atomic_write(flag, "")  # Touch file to cache check attempt

    # Return info if update available
    try:
        latest = flag.read_text().strip()
        if not latest:
            return None, None
        # Double-check version (handles manual upgrades)
        if _parse_version(__version__) >= _parse_version(latest):
            atomic_write(flag, "")  # Clear stale update notice
            return None, None

        return latest, _get_update_cmd()
    except Exception:
        return None, None


def get_update_notice() -> str | None:
    """Check PyPI for updates (once daily), return message if available"""
    latest, cmd = get_update_info()
    if latest:
        return f"→ Update available: hcom v{latest} ({cmd})"
    return None


def _prompt_and_install_update(latest: str, cmd: str) -> bool:
    """Prompt user to install update and run if confirmed.

    Returns True if update was installed, False if skipped/failed.
    """
    import subprocess

    print(f"Update available: hcom v{__version__} → v{latest}")
    try:
        response = input("Update now? [y/N] ").strip().lower()
    except (EOFError, KeyboardInterrupt):
        print()  # Newline after ^C
        return False

    if response not in ("y", "yes"):
        return False

    print(f"\nRunning: {cmd}")
    print("-" * 40)

    try:
        result = subprocess.run(cmd.split(), check=False)
        print("-" * 40)

        if result.returncode == 0:
            print(f"Updated to hcom v{latest}")
            print("Please restart hcom to use the new version.")
            return True
        else:
            print(f"Update failed (exit code {result.returncode})", file=sys.stderr)
            return False
    except Exception as e:
        print(f"Update failed: {e}", file=sys.stderr)
        return False


# _build_hcom_env_value removed - use build_hcom_command() directly

# ==================== Message System ====================
# validate_message moved to commands/utils.py

# ==================== Identity Management ====================
# resolve_instance_name moved to core/instances.py

# ==================== Hook Management ====================
# get_claude_settings_path, load_claude_settings, _remove_claude_hcom_hooks moved to hooks/settings.py

# build_env_string, build_claude_command moved to terminal.py
# create_bash_script, find_bash_on_windows moved to terminal.py
# get_macos_terminal_argv, get_windows_terminal_argv, get_linux_terminal_argv moved to terminal.py
# windows_hidden_popen, _parse_terminal_command, launch_terminal moved to terminal.py

# setup_claude_hooks, verify_claude_hooks_installed moved to hooks/settings.py

# is_interactive, get_archive_timestamp, should_show_in_watch moved to commands/admin.py
# initialize_instance_in_position_file, enable_instance moved to core/instances.py

# ==================== Command Functions ====================
# Command functions moved to commands/ package


def ensure_hooks_current() -> bool:
    """Ensure hooks match current execution context - called on EVERY command.
    Auto-updates hooks if execution context changes (e.g., pip → uvx).
    Always returns True (warns but never blocks - Claude Code is fault-tolerant)."""
    from .core.config import get_config

    settings_path = get_claude_settings_path()
    current_hcom = build_hcom_command()

    # Get config for auto_approve setting
    try:
        config = get_config()
        include_permissions = config.auto_approve
    except Exception:
        include_permissions = True  # Default to enabled

    # Check if hooks exist and are current
    needs_update = False
    if not verify_claude_hooks_installed(
        settings_path, check_permissions=include_permissions
    ):
        needs_update = True
    else:
        try:
            settings = load_claude_settings(settings_path, default={})
            if settings is None:
                needs_update = True
            else:
                installed_hcom = settings.get("env", {}).get("HCOM")
                needs_update = installed_hcom != current_hcom
        except Exception:
            needs_update = True

    if needs_update:
        try:
            setup_claude_hooks(include_permissions=include_permissions)
            if os.environ.get("CLAUDECODE") == "1":
                print(
                    "hcom hooks updated. Please restart Claude Code to apply changes.",
                    file=sys.stderr,
                )
                print("=" * 60, file=sys.stderr)
        except Exception as e:
            print(f"Warning: Could not verify/update hooks: {e}", file=sys.stderr)
            print(f"Check {settings_path}", file=sys.stderr)

    # Gemini/Codex hooks are setup at launch time (cmd_launch_gemini/cmd_launch_codex)
    # But we do self-healing to ensure hooks.enabled = true (required for v0.24.0+)
    try:
        from .tools.gemini.settings import ensure_hooks_enabled

        ensure_hooks_enabled()
    except Exception:
        pass  # Non-critical, gemini may not be installed

    return True


# ==================== Main Entry Point ====================


def main(argv: list[str] | None = None) -> int | None:
    """Main command dispatcher"""
    # Apply UTF-8 encoding for Windows and WSL (Git Bash, MSYS use cp1252 by default)
    if IS_WINDOWS or is_wsl():
        try:
            if (
                not isinstance(sys.stdout, io.TextIOWrapper)
                or sys.stdout.encoding != "utf-8"
            ):
                sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8")
            if (
                not isinstance(sys.stderr, io.TextIOWrapper)
                or sys.stderr.encoding != "utf-8"
            ):
                sys.stderr = io.TextIOWrapper(sys.stderr.buffer, encoding="utf-8")
        except (AttributeError, OSError):
            pass  # Fallback if stream redirection fails

    if argv is None:
        argv = sys.argv[1:]
    else:
        argv = argv[1:] if len(argv) > 0 and argv[0].endswith("hcom.py") else argv

    # Hook handlers only (called BY hooks, not users)
    if argv and argv[0] in (
        "poll",
        "notify",
        "pre",
        "post",
        "sessionstart",
        "userpromptsubmit",
        "sessionend",
        "subagent-start",
        "subagent-stop",
    ):
        handle_hook(argv[0])
        return 0

    # Gemini hook handlers (called BY Gemini hooks, not users)
    gemini_hooks = (
        "gemini-sessionstart",
        "gemini-beforeagent",
        "gemini-afteragent",
        "gemini-beforetool",
        "gemini-aftertool",
        "gemini-notification",
        "gemini-sessionend",
    )
    if argv and argv[0] in gemini_hooks:
        from .tools.gemini.hooks import handle_gemini_hook

        handle_gemini_hook(argv[0])
        return 0

    # Codex hook handlers (called BY Codex hooks, not users)
    if argv and argv[0] == "codex-notify":
        from .tools.codex.hooks import handle_codex_hook

        handle_codex_hook(argv[0])
        return 0

    # Ensure directories exist first (required for version check cache)
    if not ensure_hcom_directories():
        print(format_error("Failed to create HCOM directories"), file=sys.stderr)
        return 1

    # Check for updates and show message if available (once daily check, persists until upgrade)
    if msg := get_update_notice():
        print(msg, file=sys.stderr)

    # ==================== Identity Gating ====================
    # Determine command (skipping identity flags like --name)
    cmd = _find_command(argv)

    # If flags were provided but no command was found, don't fall through to TUI.
    # Only `--name` is allowed without an explicit command (TUI mode with explicit identity).
    if cmd is None and argv:
        i = 0
        unexpected_flag = None
        while i < len(argv):
            if argv[i] == "--name" and i + 1 < len(argv):
                i += 2
                continue
            if argv[i].startswith("-"):
                unexpected_flag = argv[i]
                break
            i += 1
        if unexpected_flag:
            print(
                format_error(
                    f"Unknown flag '{unexpected_flag}'", "Run 'hcom --help' for usage"
                ),
                file=sys.stderr,
            )
            return 1

    # Help/version should never fail due to identity parsing/resolution.
    help_requested = bool(argv) and (
        "--help" in argv or "-h" in argv or cmd in ("help", "--help", "-h")
    )
    version_requested = cmd in ("--version", "-v")

    explicit_name: str | None = None
    ctx: CommandContext | None = None
    if not (help_requested or version_requested):
        try:
            argv, explicit_name = _extract_name_flag(argv)
            ctx = _build_ctx_for_command(cmd, explicit_name=explicit_name)
        except (CLIError, HcomError) as exc:
            print(format_error(str(exc)), file=sys.stderr)
            return 1
    else:
        ctx = CommandContext(explicit_name=None, identity=None)

    # Launch commands: numeric (hcom 1) or tool names (hcom gemini, hcom codex, hcom claude) - always allowed
    is_launch_cmd = cmd and (cmd.isdigit() or cmd in ("gemini", "codex", "claude"))

    # Gate: require identity unless allowlisted, bypass flag, or explicit identity provided
    if (
        cmd
        and not is_launch_cmd
        and cmd not in IDENTITY_ALLOWLIST
        and cmd not in IDENTITY_BYPASS_FLAGS
    ):
        # Check if send external sender flags present (send command uses these)
        has_from = (
            cmd == "send" and (("-b" in argv) or ("--from" in argv)) if argv else False
        )
        if not explicit_name and not has_from:
            # No explicit identity provided - check if registered instance exists
            try:
                identity = ctx.identity if ctx else None
                if identity is None:
                    from .core.identity import resolve_identity

                    identity = resolve_identity()
                # Must be a registered, enabled instance
                if not (
                    identity.kind == "instance" and identity.instance_data
                ):  # Row exists = participating
                    from .core.tool_utils import build_hcom_command

                    hcom_cmd = build_hcom_command()
                    print(
                        format_error(
                            f"hcom identity not found, you need to run '{hcom_cmd} start' first, then use '{hcom_cmd} {argv[0]}'"
                        ),
                        file=sys.stderr,
                    )
                    if is_inside_ai_tool():
                        print("Usage:", file=sys.stderr)
                        print(
                            f"  {hcom_cmd} start              # New hcom identity (assigns new name)",
                            file=sys.stderr,
                        )
                        print(
                            f"  {hcom_cmd} start --as <name>  # Rebind to existing identity",
                            file=sys.stderr,
                        )
                        print(
                            f"  Then use the command: {hcom_cmd} {argv[0]} --name <name>",
                            file=sys.stderr,
                        )
                    else:
                        print(f"Usage: {hcom_cmd} start", file=sys.stderr)
                    return 1
            except HcomError:
                from .core.tool_utils import build_hcom_command

                hcom_cmd = build_hcom_command()
                print(
                    format_error(
                        f"hcom identity not found, you need to run '{hcom_cmd} start' first, then use '{hcom_cmd} {argv[0]}'"
                    ),
                    file=sys.stderr,
                )
                if is_inside_ai_tool():
                    print("Usage:", file=sys.stderr)
                    print(
                        f"  {hcom_cmd} start              # New hcom identity (assigns new name)",
                        file=sys.stderr,
                    )
                    print(
                        f"  {hcom_cmd} start --as <name>  # Rebind to existing identity",
                        file=sys.stderr,
                    )
                    print(
                        f"  Then use the command: {hcom_cmd} {argv[0]} --name <name>",
                        file=sys.stderr,
                    )
                else:
                    print(f"Usage: {hcom_cmd} start", file=sys.stderr)
                return 1

    # Subagent context: require explicit --name for all commands
    # Both subagents (--name <uuid>) and parent (--name parent) must identify
    # Skip for version/help flags and allowlisted commands - they don't need identity
    if (
        cmd
        and not explicit_name
        and cmd not in IDENTITY_ALLOWLIST
        and cmd not in ("-v", "--version", "-h", "--help")
        and os.environ.get("CLAUDECODE") == "1"
    ):
        try:
            from .core.identity import resolve_identity
            from .hooks.subagent import in_subagent_context, cleanup_dead_subagents
            from .core.instances import load_instance_position

            identity = ctx.identity if ctx else None
            if identity is None:
                identity = resolve_identity()
            if identity.name and in_subagent_context(identity.name):
                # Cleanup stale subagents before blocking (mtime check catches session-ended cases)
                instance_data = load_instance_position(identity.name)
                transcript_path = (
                    instance_data.get("transcript_path", "") if instance_data else ""
                )
                if transcript_path and identity.session_id:
                    cleanup_dead_subagents(identity.session_id, transcript_path)
                # Re-check after cleanup
                if in_subagent_context(identity.name):
                    print(
                        format_error(
                            "Subagent context active - explicit identity required",
                            f"Use: hcom {cmd} --name parent (for parent) or --name <uuid> (for subagent)",
                        ),
                        file=sys.stderr,
                    )
                    return 1
        except (ValueError, HcomError):
            pass  # Can't resolve identity - not relevant

    # Route to commands
    try:
        # Normalize shorthands: hcom gemini → hcom 1 gemini, hcom codex → hcom 1 codex
        from .shared import RELEASED_TOOLS

        if cmd in ("gemini", "codex") and cmd in RELEASED_TOOLS:
            argv = ["1"] + argv
            cmd = "1"  # Now it's a numeric launch

        # Handle --new-terminal flag: force launch TUI in new terminal window
        if cmd == "--new-terminal":
            from .core.tool_utils import build_hcom_command

            env = build_claude_env()
            hcom_cmd = build_hcom_command()
            success = launch_terminal(hcom_cmd, env, cwd=os.getcwd())
            _maybe_deliver_pending_messages(ctx=ctx)
            return 0 if success else 1

        if not cmd:
            # TTY detection: launch TUI if interactive, otherwise launch in new terminal
            # If inside AI tool (Gemini/Claude), force new terminal to avoid hijacking the session
            if sys.stdin.isatty() and sys.stdout.isatty() and not is_inside_ai_tool():
                # Check for updates and prompt before TUI launch
                latest, update_cmd = get_update_info()
                if latest:
                    if _prompt_and_install_update(latest, update_cmd):
                        return 0  # Updated - user should restart

                # Interactive terminal - run TUI directly
                from .ui import run_tui

                return run_tui(hcom_path())
            else:
                # No TTY (pipe, redirect, etc) - launch in new terminal window
                from .core.tool_utils import build_hcom_command

                env = build_claude_env()
                hcom_cmd = build_hcom_command()
                success = launch_terminal(hcom_cmd, env, cwd=os.getcwd())
                _maybe_deliver_pending_messages(ctx=ctx)
                return 0 if success else 1
        elif cmd in ("help", "--help", "-h"):
            result = cmd_help()
            _maybe_deliver_pending_messages(ctx=ctx)
            return result
        elif cmd in ("--version", "-v"):
            print(f"hcom {__version__}")
            _maybe_deliver_pending_messages(ctx=ctx)
            return 0
        elif cmd in COMMANDS:
            return _run_command(cmd, _get_command_args(argv, cmd), ctx=ctx)
        elif cmd.isdigit():
            # Launch instances: hcom [N] [tool] [args]
            # Check for gemini/codex after the number
            cmd_args = _get_command_args(argv, cmd)
            next_cmd = _find_command(cmd_args)
            launcher_name = (
                ctx.identity.name
                if (ctx and ctx.identity and ctx.identity.kind == "instance")
                else None
            )

            if next_cmd == "gemini" and "gemini" in RELEASED_TOOLS:
                if "--help" in argv or "-h" in argv:
                    from .commands.utils import get_command_help

                    print(get_command_help("gemini"))
                    _maybe_deliver_pending_messages(ctx=ctx)
                    return 0
                from .commands.lifecycle import cmd_launch_gemini

                result = cmd_launch_gemini(
                    _strip_identity_flags(argv), launcher_name=launcher_name, ctx=ctx
                )
                _maybe_deliver_pending_messages(ctx=ctx)
                return result
            elif next_cmd == "codex" and "codex" in RELEASED_TOOLS:
                if "--help" in argv or "-h" in argv:
                    from .commands.utils import get_command_help

                    print(get_command_help("codex"))
                    _maybe_deliver_pending_messages(ctx=ctx)
                    return 0
                from .commands.lifecycle import cmd_launch_codex

                result = cmd_launch_codex(
                    _strip_identity_flags(argv), launcher_name=launcher_name, ctx=ctx
                )
                _maybe_deliver_pending_messages(ctx=ctx)
                return result
            else:
                # Default: Claude launch
                if "--help" in argv or "-h" in argv:
                    from .commands.utils import get_command_help

                    print(get_command_help("claude"))
                    _maybe_deliver_pending_messages(ctx=ctx)
                    return 0
                result = cmd_launch(
                    _strip_identity_flags(argv), launcher_name=launcher_name, ctx=ctx
                )
                _maybe_deliver_pending_messages(ctx=ctx)
                return result
        elif cmd == "claude":
            # hcom claude [args]
            if "--help" in argv or "-h" in argv:
                from .commands.utils import get_command_help

                print(get_command_help("claude"))
                _maybe_deliver_pending_messages(ctx=ctx)
                return 0
            launcher_name = (
                ctx.identity.name
                if (ctx and ctx.identity and ctx.identity.kind == "instance")
                else None
            )
            result = cmd_launch(
                _strip_identity_flags(argv), launcher_name=launcher_name, ctx=ctx
            )
            _maybe_deliver_pending_messages(ctx=ctx)
            return result
        else:
            print(
                format_error(f"Unknown command: {cmd}", "Run 'hcom --help' for usage"),
                file=sys.stderr,
            )
            return 1
    except (CLIError, ValueError, RuntimeError, HcomError) as exc:
        print(str(exc), file=sys.stderr)
        return 1


# ==================== Exports for UI Module ====================
# Command functions (cmd_launch, cmd_start, cmd_stop, cmd_reset) now in commands/ package
# Utility functions (should_show_in_watch) now in commands/admin.py

__all__ = [
    # CLI entry points
    "main",
    "ensure_hooks_current",
    # Hook management (from hooks/settings.py)
    "setup_claude_hooks",
    "verify_claude_hooks_installed",
    "remove_claude_hooks",
]

if __name__ == "__main__":
    sys.exit(main())
