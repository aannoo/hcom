#!/usr/bin/env python3
"""Terminal launching for HCOM"""

from __future__ import annotations

import enum
import os
import signal
import sys
import re
import shlex
import subprocess
import shutil
import platform
import random
import tempfile
import time
from pathlib import Path
from typing import Any

from .shared import (
    IS_WINDOWS,
    CREATE_NO_WINDOW,
    is_wsl,
    is_termux,
    HcomError,
    TOOL_MARKER_VARS,
    HCOM_IDENTITY_VARS,
)
from .core.paths import hcom_path, LAUNCH_DIR, read_file_with_retry
from .core.config import get_config


class KillResult(enum.Enum):
    """Result of kill_process()."""
    SENT = "sent"
    ALREADY_DEAD = "already_dead"
    PERMISSION_DENIED = "permission_denied"


class TerminalInfo:
    """Terminal info resolved for an instance."""
    __slots__ = ("preset_name", "pane_id", "process_id", "kitty_listen_on", "terminal_id")

    def __init__(self, preset_name: str = "", pane_id: str = "", process_id: str = "",
                 kitty_listen_on: str = "", terminal_id: str = ""):
        self.preset_name = preset_name
        self.pane_id = pane_id
        self.process_id = process_id
        self.kitty_listen_on = kitty_listen_on
        self.terminal_id = terminal_id


def resolve_terminal_info(name: str, pid: int) -> TerminalInfo:
    """Resolve terminal preset, pane_id, process_id, and kitty socket for an instance.

    Tries launch_context (DB) first, falls back to pidtrack, then process_bindings.
    """
    import json as _json

    info = TerminalInfo()

    # Primary: launch_context from DB (available for active instances)
    try:
        from .core.instances import load_instance_position
        pos = load_instance_position(name)
        if pos:
            lc = pos.get("launch_context", "")
            if lc:
                lc_data = _json.loads(lc)
                info.preset_name = lc_data.get("terminal_preset", "")
                info.pane_id = lc_data.get("pane_id", "")
                info.process_id = lc_data.get("process_id", "")
                info.terminal_id = lc_data.get("terminal_id", "")
                # Extract KITTY_LISTEN_ON from captured env
                lc_env = lc_data.get("env", {})
                info.kitty_listen_on = lc_env.get("KITTY_LISTEN_ON", "")
    except Exception:
        pass

    # Fallback: pidtrack (for orphans after stop deleted the DB row)
    if not info.preset_name:
        from .core.pidtrack import get_pane_id_for_pid, get_preset_for_pid, get_process_id_for_pid, get_terminal_id_for_pid, get_kitty_listen_on_for_pid
        info.preset_name = get_preset_for_pid(pid) or ""
        info.pane_id = get_pane_id_for_pid(pid)
        if not info.process_id:
            info.process_id = get_process_id_for_pid(pid)
        if not info.terminal_id:
            info.terminal_id = get_terminal_id_for_pid(pid)
        if not info.kitty_listen_on:
            info.kitty_listen_on = get_kitty_listen_on_for_pid(pid)

    # Fallback: process_bindings table (for process_id if not in launch_context)
    if not info.process_id:
        try:
            from .core.db import get_db as _get_db
            row = _get_db().execute(
                "SELECT process_id FROM process_bindings WHERE instance_name = ?",
                (name,),
            ).fetchone()
            if row:
                info.process_id = row["process_id"]
        except Exception:
            pass

    return info


def _kitty_listen_fd() -> int | None:
    """Extract fd number from KITTY_LISTEN_ON=fd:N, if present."""
    val = os.environ.get("KITTY_LISTEN_ON", "")
    if val.startswith("fd:"):
        try:
            return int(val[3:])
        except ValueError:
            pass
    return None

# ==================== Terminal Presets ====================

# Cache for available presets (computed once per process)
_available_presets_cache: list[tuple[str, bool]] | None = None

# macOS app bundle fallback commands for cross-platform terminals
# Used when CLI binary isn't in PATH but .app bundle is installed
_MACOS_APP_FALLBACKS: dict[str, str] = {
    "kitty": "open -n -a kitty.app --args {script}",
    "kitty-window": "open -n -a kitty.app --args {script}",
    "wezterm": "open -n -a WezTerm.app --args start -- bash {script}",
    "wezterm-window": "open -n -a WezTerm.app --args start -- bash {script}",
    "alacritty": "open -n -a Alacritty.app --args -e bash {script}",
}


def _find_macos_app(name: str) -> Path | None:
    """Find macOS .app bundle in common locations. Returns path if found."""
    app_name = name if name.endswith(".app") else f"{name}.app"
    for base in [
        Path("/Applications"),
        Path("/System/Applications"),
        Path("/System/Applications/Utilities"),
        Path.home() / "Applications",
    ]:
        app_path = base / app_name
        if app_path.exists():
            return app_path
    return None


def get_available_presets() -> list[tuple[str, bool]]:
    """Get terminal presets for current platform with availability status.

    Returns list of (preset_name, is_available) tuples.
    Cached after first call. Order: 'default' first, presets, 'custom' last.

    On macOS, cross-platform terminals (kitty, WezTerm, Alacritty) are marked
    available if either CLI is in PATH or .app bundle is installed.
    """
    global _available_presets_cache
    if _available_presets_cache is not None:
        return _available_presets_cache

    system = platform.system()
    result: list[tuple[str, bool]] = [("default", True)]  # Always available

    from .core.settings import get_merged_presets
    merged = get_merged_presets()

    for name, preset in merged.items():
        binary = preset.get("binary")
        platforms = preset.get("platforms", [])
        # Skip if not for current platform
        if system not in platforms:
            continue

        # Check availability
        available = False
        if binary:
            # Check if binary exists in PATH or resolvable via app bundle
            available = shutil.which(binary) is not None
            if not available and system == "Darwin":
                available = _resolve_binary_path(binary, preset, name) is not None
        else:
            # For macOS apps (binary=None), check app bundle locations
            if system == "Darwin":
                available = _find_macos_app(preset.get("app_name", name)) is not None
            else:
                available = True  # Assume available if no binary check

        result.append((name, available))

    result.append(("custom", True))  # Always available
    _available_presets_cache = result
    return result


def _resolve_binary_path(binary: str, preset: dict, preset_name: str) -> str | None:
    """Resolve binary to full path. Returns None if already on PATH or not found."""
    if shutil.which(binary):
        return None
    if platform.system() != "Darwin":
        return None
    app = _find_macos_app(preset.get("app_name", preset_name))
    if not app:
        return None
    full_path = app / "Contents" / "MacOS" / binary
    return str(full_path) if full_path.exists() else None


def resolve_terminal_preset(preset_name: str) -> str | None:
    """Resolve preset name to command template.

    On macOS, if CLI binary isn't in PATH but .app bundle exists,
    uses a hardcoded fallback (new window presets) or substitutes
    the full binary path into the open command (tab/split presets).
    """
    from .core.settings import get_merged_preset

    preset = get_merged_preset(preset_name)
    if not preset:
        return None

    binary = preset.get("binary")
    open_cmd = preset["open"]

    if binary and not shutil.which(binary) and platform.system() == "Darwin":
        # New-window presets have hardcoded fallbacks using `open -a`
        if preset_name in _MACOS_APP_FALLBACKS:
            if _find_macos_app(preset.get("app_name", preset_name)) is not None:
                return _MACOS_APP_FALLBACKS[preset_name]
        # Tab/split presets: substitute leading binary with full path
        full_path = _resolve_binary_path(binary, preset, preset_name)
        if full_path and open_cmd.startswith(binary):
            open_cmd = full_path + open_cmd[len(binary):]

    return open_cmd


def _find_kitten_binary() -> str | None:
    """Find kitten binary — PATH first, then macOS app bundle."""
    path_kitten = shutil.which("kitten")
    if path_kitten:
        return path_kitten
    if platform.system() == "Darwin":
        app = _find_macos_app("kitty")
        if app:
            full = app / "Contents" / "MacOS" / "kitten"
            if full.exists():
                return str(full)
    return None


def _find_kitty_socket() -> str:
    """Find a reachable kitty remote control socket. Returns 'unix:<path>' or ''."""
    import glob as _glob
    import stat

    kitten = _find_kitten_binary()
    if not kitten:
        return ""

    candidates = sorted(_glob.glob("/tmp/kitty*"), reverse=True)
    for sock_path in candidates:
        try:
            if not stat.S_ISSOCK(os.stat(sock_path).st_mode):
                continue
        except OSError:
            continue
        socket_uri = f"unix:{sock_path}"
        try:
            result = subprocess.run(
                [kitten, "@", "--to", socket_uri, "ls"],
                capture_output=True, timeout=2,
            )
            if result.returncode == 0:
                return socket_uri
        except Exception:
            pass
    return ""


def _wezterm_reachable() -> bool:
    """Check if a wezterm mux server is reachable."""
    try:
        result = subprocess.run(
            ["wezterm", "cli", "list"], capture_output=True, timeout=2,
        )
        return result.returncode == 0
    except Exception:
        return False


def detect_terminal_from_env() -> str | None:
    """Detect terminal preset from inherited environment variables.

    Used for same-terminal PTY launches (run_here=True) to enable close-on-kill.
    Returns preset name if a recognized terminal env var is set, None otherwise.
    """
    from .shared import TERMINAL_ENV_MAP

    for env_var, preset_name in TERMINAL_ENV_MAP.items():
        if os.environ.get(env_var):
            return preset_name
    return None


def close_terminal_pane(pid: int, preset_name: str, pane_id: str = "", process_id: str = "",
                        kitty_listen_on: str = "", terminal_id: str = "") -> bool:
    """Run terminal-specific close command before SIGTERM.

    Must run before SIGTERM because terminal CLIs match panes by PID, pane_id, process_id, or terminal_id.
    Returns True if close command ran successfully, False otherwise.
    Non-fatal: caller should always proceed with SIGTERM regardless.
    """
    from .core.log import log_info, log_warn
    from .core.settings import get_merged_preset

    log_info("terminal", "close_pane_attempt", preset=preset_name, pane_id=pane_id, process_id=process_id, pid=pid)

    preset = get_merged_preset(preset_name)
    if not preset:
        return False

    close_cmd = preset.get("close")
    if not close_cmd:
        return False

    # Skip if command needs a placeholder we don't have
    if "{pane_id}" in close_cmd and not pane_id:
        return False
    if "{process_id}" in close_cmd and not process_id:
        return False
    if "{id}" in close_cmd and not terminal_id:
        return False

    close_cmd = close_cmd.replace("{pid}", str(pid))
    close_cmd = close_cmd.replace("{pane_id}", pane_id)
    close_cmd = close_cmd.replace("{process_id}", process_id)
    close_cmd = close_cmd.replace("{id}", terminal_id)

    # Resolve binary path (app bundle fallback on macOS)
    binary = preset.get("binary")
    if binary:
        full_path = _resolve_binary_path(binary, preset, preset_name)
        if full_path and close_cmd.startswith(binary):
            close_cmd = full_path + close_cmd[len(binary):]

    # Inject --to for kitten commands when we have the socket path
    # (daemon doesn't inherit KITTY_LISTEN_ON, so use the value from launch_context)
    if ("kitten @" in close_cmd and kitty_listen_on and "--to" not in close_cmd
            and not kitty_listen_on.startswith("fd:")):
        close_cmd = close_cmd.replace("kitten @", f"kitten @ --to {shlex.quote(kitty_listen_on)}")

    try:
        popen_kwargs: dict[str, Any] = {}
        kitty_fd = _kitty_listen_fd()
        if kitty_fd is not None and "kitten" in close_cmd:
            popen_kwargs["pass_fds"] = (kitty_fd,)
        result = subprocess.run(
            close_cmd,
            shell=True,
            timeout=5,
            capture_output=True,
            text=True,
            **popen_kwargs,
        )
        _log = log_warn if result.returncode != 0 else log_info
        _log("terminal", "close_pane", cmd=close_cmd, rc=result.returncode,
             stdout=result.stdout.strip(), stderr=result.stderr.strip())
        return result.returncode == 0
    except subprocess.TimeoutExpired:
        return False
    except Exception:
        return False


def kill_process(pid: int, preset_name: str = "", pane_id: str = "", process_id: str = "",
                 kitty_listen_on: str = "", terminal_id: str = "") -> tuple[KillResult, bool]:
    """Close terminal pane (if applicable) then SIGTERM the process group.

    Returns (kill_result, pane_closed) tuple.
    """
    pane_closed = False
    if preset_name:
        pane_closed = close_terminal_pane(pid, preset_name, pane_id=pane_id, process_id=process_id,
                                          kitty_listen_on=kitty_listen_on, terminal_id=terminal_id)
    try:
        os.killpg(pid, signal.SIGTERM)
        return KillResult.SENT, pane_closed
    except ProcessLookupError:
        return KillResult.ALREADY_DEAD, pane_closed
    except PermissionError:
        return KillResult.PERMISSION_DENIED, pane_closed


# ==================== Environment Building ====================


def build_env_string(env_vars: dict[str, Any], format_type: str = "bash") -> str:
    """Build environment variable string for bash shells"""
    # Filter out invalid bash variable names (must be letters, digits, underscores only)
    valid_vars = {k: v for k, v in env_vars.items() if re.match(r"^[a-zA-Z_][a-zA-Z0-9_]*$", k)}

    # On Windows, exclude PATH (let Git Bash handle it to avoid Windows vs Unix path format issues)
    if platform.system() == "Windows":
        valid_vars = {k: v for k, v in valid_vars.items() if k != "PATH"}

    if format_type == "bash_export":
        # Properly escape values for bash
        return " ".join(f"export {k}={shlex.quote(str(v))};" for k, v in valid_vars.items())
    else:
        return " ".join(f"{k}={shlex.quote(str(v))}" for k, v in valid_vars.items())


# ==================== Script Creation ====================


def find_bash_on_windows() -> str | None:
    """Find Git Bash on Windows, avoiding WSL's bash launcher"""
    # 0. User-specified path via env var (highest priority)
    if user_bash := os.environ.get("CLAUDE_CODE_GIT_BASH_PATH"):
        if Path(user_bash).exists():
            return user_bash
    # Build prioritized list of bash candidates
    candidates = []
    # 1. Common Git Bash locations
    for base in [
        os.environ.get("PROGRAMFILES", r"C:\Program Files"),
        os.environ.get("PROGRAMFILES(X86)", r"C:\Program Files (x86)"),
    ]:
        if base:
            candidates.extend(
                [
                    str(Path(base) / "Git" / "usr" / "bin" / "bash.exe"),  # usr/bin is more common
                    str(Path(base) / "Git" / "bin" / "bash.exe"),
                ]
            )
    # 2. Portable Git installation
    if local_appdata := os.environ.get("LOCALAPPDATA", ""):
        git_portable = Path(local_appdata) / "Programs" / "Git"
        candidates.extend(
            [
                str(git_portable / "usr" / "bin" / "bash.exe"),
                str(git_portable / "bin" / "bash.exe"),
            ]
        )
    # 3. PATH bash (if not WSL's launcher)
    if (path_bash := shutil.which("bash")) and not path_bash.lower().endswith(r"system32\bash.exe"):
        candidates.append(path_bash)
    # 4. Hardcoded fallbacks (last resort)
    candidates.extend(
        [
            r"C:\Program Files\Git\usr\bin\bash.exe",
            r"C:\Program Files\Git\bin\bash.exe",
            r"C:\Program Files (x86)\Git\usr\bin\bash.exe",
            r"C:\Program Files (x86)\Git\bin\bash.exe",
        ]
    )
    # Find first existing bash
    for bash in candidates:
        if bash and Path(bash).exists():
            return bash

    return None


def create_bash_script(
    script_file: str,
    env: dict[str, Any],
    cwd: str | None,
    command_str: str,
    background: bool = False,
    tool_name: str | None = None,
    opens_new_window: bool = False,
) -> None:
    """Create a bash script for terminal launch
    Scripts provide uniform execution across all platforms/terminals.
    Cleanup behavior:
    - Normal scripts: append 'rm -f' command for self-deletion
    - Background scripts: persist until `hcom reset logs` cleanup (24 hours)
    """
    # Detect tool from command if not specified
    if not tool_name:
        cmd_lower = command_str.lower()
        if "gemini" in cmd_lower:
            tool_name = "Gemini"
        elif "codex" in cmd_lower:
            tool_name = "Codex"
        else:
            tool_name = "Claude Code"

    with open(script_file, "w", encoding="utf-8") as f:
        f.write("#!/bin/bash\n")
        # Set temporary title before tool starts
        f.write(f'printf "\\033]0;hcom: starting {tool_name}...\\007"\n')
        f.write(f'echo "Starting {tool_name}..."\n')

        # Unset tool markers and HCOM identity vars to prevent inheritance
        # Tool markers: prevent false tool detection in children
        # Identity vars: prevent parent identity leakage (critical for env=None fork inheritance)
        f.write(f"unset {' '.join(TOOL_MARKER_VARS)}\n")
        f.write(f"unset {' '.join(HCOM_IDENTITY_VARS)}\n")

        if platform.system() != "Windows":
            from .core.tool_utils import find_tool_path

            # Discover paths for minimal environments (kitty splits, etc.)
            paths_to_add: list[str] = []

            def _add_path(binary_path: str | None) -> None:
                if binary_path:
                    dir_path = str(Path(binary_path).resolve().parent)
                    if dir_path not in paths_to_add:
                        paths_to_add.append(dir_path)

            # Always add hcom's own directory so 'hcom' is findable
            _add_path(shutil.which("hcom"))

            # Detect tool from command and add its path
            cmd_stripped = command_str.lstrip()
            tool_cmd = cmd_stripped.split()[0] if cmd_stripped else ""
            tool_path = find_tool_path(tool_cmd) if tool_cmd else None
            _add_path(tool_path)

            # Claude needs node for Termux and general operation
            is_claude_command = tool_cmd == "claude"
            node_path = shutil.which("node") if is_claude_command else None
            _add_path(node_path)

            # Write PATH additions
            if paths_to_add:
                f.write(f'export PATH="{":".join(paths_to_add)}:$PATH"\n')

            # Write environment variables
            f.write(build_env_string(env, "bash_export") + "\n")

            if cwd:
                f.write(f"cd {shlex.quote(cwd)}\n")

            # Platform-specific command modifications
            if is_claude_command and tool_path:
                if is_termux():
                    # Termux: explicit node to bypass shebang issues
                    final_node = node_path or "/data/data/com.termux/files/usr/bin/node"
                    command_str = command_str.replace(
                        "claude ",
                        f"{shlex.quote(final_node)} {shlex.quote(tool_path)} ",
                        1,
                    )
                else:
                    # Mac/Linux: use full path (PATH now has node if needed)
                    command_str = command_str.replace("claude ", f"{shlex.quote(tool_path)} ", 1)
        else:
            # Windows: no PATH modification needed
            f.write(build_env_string(env, "bash_export") + "\n")
            if cwd:
                f.write(f"cd {shlex.quote(cwd)}\n")

        f.write(f"{command_str}\n")

        # For new terminal windows: clean up identity env vars and keep
        # terminal open with a login shell after the tool exits
        if opens_new_window:
            f.write("unset HCOM_PROCESS_ID HCOM_LAUNCHED HCOM_PTY_MODE HCOM_TAG HCOM_CODEX_SANDBOX_MODE\n")
            f.write(f"rm -f {shlex.quote(script_file)}\n")
            # Terminal close now runs from Python cmd_kill, not the bash script
            f.write("exec bash -l\n")
        elif not background:
            # Self-delete for normal mode (not background)
            f.write("hcom_status=$?\n")
            f.write(f"rm -f {shlex.quote(script_file)}\n")
            f.write("exit $hcom_status\n")

    # Make executable on Unix
    if platform.system() != "Windows":
        os.chmod(script_file, 0o755)


# ==================== Terminal Launching ====================


def get_macos_terminal_argv() -> list[str]:
    """Return macOS Terminal.app launch command as argv list.
    Uses 'open -a Terminal' with .command files to avoid AppleScript permission popup.
    """
    return ["open", "-a", "Terminal", "{script}"]


def get_windows_terminal_argv() -> list[str]:
    """Return Windows terminal launcher as argv list."""
    from .commands.utils import format_error

    if not (bash_exe := find_bash_on_windows()):
        raise Exception(format_error("Git Bash not found"))

    if shutil.which("wt"):
        return ["wt", bash_exe, "{script}"]
    return ["cmd", "/c", "start", "Claude Code", bash_exe, "{script}"]


def get_linux_terminal_argv() -> list[str] | None:
    """Return first available standard Linux terminal as argv list.

    Only checks the 3 standard/default Linux terminals.
    Users wanting other terminals (kitty, tilix, etc.) should select them explicitly.
    """
    terminals = [
        ("gnome-terminal", ["gnome-terminal", "--", "bash", "{script}"]),
        ("konsole", ["konsole", "-e", "bash", "{script}"]),
        ("xterm", ["xterm", "-e", "bash", "{script}"]),
    ]
    for term_name, argv_template in terminals:
        if shutil.which(term_name):
            return argv_template

    # WSL fallback
    if is_wsl() and shutil.which("cmd.exe"):
        if shutil.which("wt.exe"):
            return ["cmd.exe", "/c", "start", "wt.exe", "bash", "{script}"]
        return ["cmd.exe", "/c", "start", "bash", "{script}"]

    return None


def windows_hidden_popen(
    argv: list[str],
    *,
    env: dict[str, str] | None = None,
    cwd: str | None = None,
    stdout: Any = None,
) -> subprocess.Popen:
    """Create hidden Windows process without console window."""
    if IS_WINDOWS:
        startupinfo = subprocess.STARTUPINFO()  # type: ignore[attr-defined]
        startupinfo.dwFlags |= subprocess.STARTF_USESHOWWINDOW  # type: ignore[attr-defined]
        startupinfo.wShowWindow = subprocess.SW_HIDE  # type: ignore[attr-defined]

        return subprocess.Popen(
            argv,
            env=env,
            cwd=cwd,
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=subprocess.STDOUT,
            startupinfo=startupinfo,
            creationflags=CREATE_NO_WINDOW,
        )
    else:
        raise RuntimeError("windows_hidden_popen called on non-Windows platform")


# Platform dispatch map
PLATFORM_TERMINAL_GETTERS = {
    "Darwin": get_macos_terminal_argv,
    "Windows": get_windows_terminal_argv,
    "Linux": get_linux_terminal_argv,
}


def _parse_terminal_command(template: str, script_file: str, process_id: str = "") -> list[str]:
    """Parse terminal command template safely to prevent shell injection.
    Parses the template FIRST, then replaces {script} and {process_id}
    placeholders in the parsed tokens.
    Args:
        template: Terminal command template with {script} placeholder
        script_file: Path to script file to substitute
        process_id: HCOM process ID for {process_id} substitution
    Returns:
        list: Parsed command as argv array
    Raises:
        ValueError: If template is invalid or missing {script} placeholder
    """
    from .commands.utils import format_error

    if "{script}" not in template:
        raise ValueError(
            format_error(
                "Custom terminal command must include {script} placeholder",
                'Example: open -n -a kitty.app --args bash "{script}"',
            )
        )

    try:
        parts = shlex.split(template)
    except ValueError as e:
        raise ValueError(
            format_error(
                f"Invalid terminal command syntax: {e}",
                "Check for unmatched quotes or invalid shell syntax",
            )
        )

    # Replace {script} and {process_id} in parsed tokens
    replaced = []
    placeholder_found = False
    for part in parts:
        if "{process_id}" in part:
            part = part.replace("{process_id}", process_id)
        if "{script}" in part:
            part = part.replace("{script}", script_file)
            placeholder_found = True
        replaced.append(part)

    if not placeholder_found:
        raise ValueError(
            format_error(
                "{script} placeholder not found after parsing",
                "Ensure {script} is not inside environment variables",
            )
        )

    return replaced


def launch_terminal(
    command: str,
    env: dict[str, str],
    cwd: str | None = None,
    background: bool = False,
    run_here: bool = False,
) -> str | bool | None | tuple[str, int]:
    """Launch terminal with command using unified script-first approach

    Environment precedence: config.env < shell environment
    Internal hcom vars (HCOM_LAUNCHED, etc) don't conflict with user vars.

    Args:
        command: Command string from build_claude_command
        env: Contains config.env defaults + hcom internal vars
        cwd: Working directory
        background: Launch as background process
        run_here: If True, run in current terminal (blocking). Used for count=1 launches.

    Returns:
        - background=True: (log_file_path, pid) tuple on success, None on failure
        - run_here=True: True on success, False on failure
        - new terminal: True on success (async), False on failure
    """
    from .commands.utils import format_error
    from .core.paths import LOGS_DIR
    import time

    # env param contains config.env + instance vars (from launcher)
    # We'll build different env sets based on launch mode
    config_and_instance_env = env.copy()

    # For same-terminal modes, we need full env (config + instance + shell)
    # Build this by adding filtered shell env
    PROPAGATE_HCOM_VARS = {"HCOM_DIR", "HCOM_VIA_SHIM"}
    NO_PROPAGATE_CONFIG_KEYS = {"HCOM_TERMINAL"}
    from .core.config import KNOWN_CONFIG_KEYS

    def should_propagate(key: str) -> bool:
        """Determine if shell env var should propagate to child."""
        if key in TOOL_MARKER_VARS:
            return False
        if key in NO_PROPAGATE_CONFIG_KEYS:
            return False
        if not key.startswith("HCOM_"):
            return True
        return key in KNOWN_CONFIG_KEYS or key in PROPAGATE_HCOM_VARS

    full_env_vars = config_and_instance_env.copy()
    full_env_vars.update({k: v for k, v in os.environ.items() if should_propagate(k)})

    # Ensure SHELL for Termux (launches in clean env via Activity Manager)
    if "SHELL" not in full_env_vars:
        shell_path = os.environ.get("SHELL") or shutil.which("bash") or shutil.which("sh")
        if not shell_path:
            shell_path = "/data/data/com.termux/files/usr/bin/bash" if is_termux() else "/bin/bash"
        if shell_path:
            full_env_vars["SHELL"] = shell_path
            config_and_instance_env["SHELL"] = shell_path

    command_str = command

    # 1) Determine script extension
    # macOS default mode uses .command
    # All other cases (custom terminal, other platforms, background) use .sh
    terminal_mode = get_config().terminal
    use_command_ext = not background and platform.system() == "Darwin" and terminal_mode == "default"
    extension = ".command" if use_command_ext else ".sh"
    script_file = str(hcom_path(LAUNCH_DIR, f"hcom_{os.getpid()}_{random.randint(1000, 9999)}{extension}"))

    # Detect tool from command for terminal title
    cmd_lower = command_str.lower()
    if "gemini" in cmd_lower:
        tool_name = "Gemini"
    elif "codex" in cmd_lower:
        tool_name = "Codex"
    else:
        tool_name = "Claude Code"

    opens_new_window = not background and not run_here

    # Build script_env based on launch mode
    # Principle: new windows get ONLY config.env + instance vars (nothing from shell)
    if opens_new_window:
        # New window: launched by terminal app, no shell inheritance
        # Want env var in new window? Put it in config.env
        script_env = config_and_instance_env
    elif run_here:
        # Run-here: os.execve REPLACES env entirely, needs full env
        script_env = full_env_vars
    else:
        # Background (same terminal): subprocess inherits via fork
        # Script only exports deltas (vars not already in shell)
        script_env = {k: v for k, v in full_env_vars.items() if os.environ.get(k) != v}

    create_bash_script(
        script_file,
        script_env,
        cwd,
        command_str,
        background,
        tool_name,
        opens_new_window,
    )

    # 2) Background mode
    if background:
        logs_dir = hcom_path(LOGS_DIR)
        log_file = logs_dir / env["HCOM_BACKGROUND"]

        try:
            with open(log_file, "w", encoding="utf-8") as log_handle:
                if IS_WINDOWS:
                    # Windows: hidden bash execution with Python-piped logs
                    # Windows needs explicit env (no fork() semantics)
                    bash_exe = find_bash_on_windows()
                    if not bash_exe:
                        raise Exception("Git Bash not found")

                    process = windows_hidden_popen(
                        [bash_exe, script_file],
                        env=full_env_vars,
                        cwd=cwd,
                        stdout=log_handle,
                    )
                else:
                    # Unix: subprocess inherits shell env via fork()
                    # Script exports only deltas (script_env has vars not in shell)
                    process = subprocess.Popen(
                        ["bash", script_file],
                        cwd=cwd,
                        stdin=subprocess.DEVNULL,
                        stdout=log_handle,
                        stderr=subprocess.STDOUT,
                        start_new_session=True,
                    )

        except OSError as e:
            print(format_error(f"Failed to launch headless: {e}"), file=sys.stderr)
            return None

        # Health check
        time.sleep(0.2)
        if process.poll() is not None:
            error_output = read_file_with_retry(log_file, lambda f: f.read()[:1000], default="")
            print(format_error("Headless failed immediately"), file=sys.stderr)
            if error_output:
                print(f"  Output: {error_output}", file=sys.stderr)
            return None

        # Return (log_file, pid) tuple for background mode
        return (str(log_file), process.pid)

    # 3) Terminal modes
    # 'print': internal/debug - show script without executing
    if terminal_mode == "print":
        try:
            with open(script_file, "r", encoding="utf-8") as f:
                script_content = f.read()
            print(f"# Script: {script_file}")
            print(script_content)
            Path(script_file).unlink()  # Clean up immediately
            return True
        except Exception as e:
            print(format_error(f"Failed to read script: {e}"), file=sys.stderr)
            return False

    # 3b) Run in current terminal (blocking) - used for count=1 launches
    if run_here:
        if IS_WINDOWS:
            bash_exe = find_bash_on_windows()
            if not bash_exe:
                print(format_error("Git Bash not found"), file=sys.stderr)
                return False
            # Windows: can't exec, use subprocess with full env
            result = subprocess.run([bash_exe, script_file], env=full_env_vars, cwd=cwd)
            if result.returncode != 0:
                raise HcomError(format_error("Terminal launch failed"))
            return True
        else:
            # Unix: exec REPLACES this process entirely
            # execve needs full env dict (not inherited like fork)
            if cwd:
                os.chdir(cwd)
            os.execve("/bin/bash", ["bash", script_file], full_env_vars)
            # Never reaches here - execve replaces the process

    # 4) New window or custom command mode
    # Resolve terminal_mode: 'default' → platform auto-detect, preset name → command, else custom
    custom_cmd: str | list[str] | None
    from .core.settings import get_merged_presets as _get_merged

    # Smart terminal: "kitty" auto-detects split/tab/window, "wezterm" same pattern
    kitty_socket = ""
    if terminal_mode == "kitty":
        if os.environ.get("KITTY_WINDOW_ID"):
            terminal_mode = "kitty-split"
        else:
            kitty_socket = _find_kitty_socket()
            terminal_mode = "kitty-tab" if kitty_socket else "kitty-window"
    elif terminal_mode == "wezterm":
        if os.environ.get("WEZTERM_PANE"):
            terminal_mode = "wezterm-split"
        elif _wezterm_reachable():
            terminal_mode = "wezterm-tab"
        else:
            terminal_mode = "wezterm-window"

    # Update HCOM_LAUNCHED_PRESET so launch_context captures the resolved preset
    if terminal_mode != get_config().terminal:
        env["HCOM_LAUNCHED_PRESET"] = terminal_mode

    if terminal_mode == "default":
        custom_cmd = None  # Will use platform default
    elif terminal_mode in _get_merged():
        # Kitty split/tab presets need remote control (kitten @)
        if terminal_mode in ("kitty-tab", "kitty-split"):
            listen_on = os.environ.get("KITTY_LISTEN_ON", "") or kitty_socket
            if not listen_on:
                raise HcomError(
                    f"{terminal_mode} requires remote control.\n"
                    "  Add to ~/.config/kitty/kitty.conf:\n"
                    "    allow_remote_control yes\n"
                    "    listen_on unix:/tmp/kitty\n"
                    "  Then restart kitty."
                )
        custom_cmd = resolve_terminal_preset(terminal_mode)  # Handles app bundle fallback
        # Inject --to for kitty commands launched from outside kitty (no KITTY_LISTEN_ON in env)
        if kitty_socket and custom_cmd and isinstance(custom_cmd, str) and "kitten @" in custom_cmd and "--to" not in custom_cmd:
            custom_cmd = custom_cmd.replace("kitten @", f"kitten @ --to {shlex.quote(kitty_socket)}")
        # Target the launcher's tab so splits/tabs open next to the launching instance
        if terminal_mode in ("kitty-tab", "kitty-split"):
            kitty_wid = os.environ.get("KITTY_WINDOW_ID")
            if kitty_wid and custom_cmd and " -- " in custom_cmd:
                custom_cmd = custom_cmd.replace(" -- ", f" --match window_id:{kitty_wid} -- ", 1)
    else:
        custom_cmd = terminal_mode  # Custom command with {script}

    if not custom_cmd:  # Platform default mode
        if is_termux():
            # Keep Termux as special case
            am_cmd = [
                "am",
                "startservice",
                "--user",
                "0",
                "-n",
                "com.termux/com.termux.app.RunCommandService",
                "-a",
                "com.termux.RUN_COMMAND",
                "--es",
                "com.termux.RUN_COMMAND_PATH",
                script_file,
                "--ez",
                "com.termux.RUN_COMMAND_BACKGROUND",
                "false",
            ]
            try:
                subprocess.run(am_cmd, check=False)
                return True
            except Exception as e:
                raise HcomError(format_error(f"Failed to launch Termux: {e}"))

        # Unified platform handling via helpers
        system = platform.system()
        if not (terminal_getter := PLATFORM_TERMINAL_GETTERS.get(system)):
            raise HcomError(format_error(f"Unsupported platform: {system}"))

        custom_cmd = terminal_getter()
        if not custom_cmd:  # e.g., Linux with no terminals
            raise HcomError(
                format_error(
                    "No supported terminal emulator found",
                    "Install gnome-terminal, konsole, or xterm",
                )
            )

    # Type-based dispatch for execution
    if isinstance(custom_cmd, list):
        # Our argv commands - safe execution without shell
        final_argv = [arg.replace("{script}", script_file) for arg in custom_cmd]
        try:
            success, captured_id = _spawn_terminal_process(final_argv, format_error)
            _write_terminal_id(env, captured_id)
            return success
        except HcomError:
            raise
        except Exception as e:
            raise HcomError(format_error(f"Failed to launch terminal: {e}"))
    else:
        # User-provided string commands - parse safely without shell=True
        try:
            final_argv = _parse_terminal_command(custom_cmd, script_file, process_id=env.get("HCOM_PROCESS_ID", ""))
        except ValueError as e:
            print(str(e), file=sys.stderr)
            return False

        try:
            success, captured_id = _spawn_terminal_process(final_argv, format_error)
            _write_terminal_id(env, captured_id)
            return success
        except HcomError:
            raise
        except Exception as e:
            raise HcomError(format_error(f"Failed to execute terminal command: {e}"))


def _write_terminal_id(env: dict[str, str], captured_id: str) -> None:
    """Write captured terminal ID (stdout from open command) to temp file for child to read."""
    if not captured_id:
        return
    process_id = env.get("HCOM_PROCESS_ID", "")
    if not process_id:
        return
    ids_dir = Path(hcom_path(".tmp", "terminal_ids"))
    ids_dir.mkdir(parents=True, exist_ok=True)
    try:
        (ids_dir / process_id).write_text(captured_id)
    except OSError:
        pass


def _spawn_terminal_process(argv: list[str], format_error) -> tuple[bool, str]:
    """Spawn terminal process, detached when inside AI tools.

    When running inside Gemini/Codex/Claude, their PTY wrappers capture
    subprocess output and render it in their TUI (blocking the screen).
    Solution: fully detach with Popen + start_new_session + DEVNULL.

    Returns (success, stdout_first_line) — stdout captured for {id} in close commands.
    """
    from .shared import is_inside_ai_tool
    from .core.log import log_info, log_warn

    if platform.system() == "Windows":
        # Windows needs non-blocking for parallel launches
        process = subprocess.Popen(argv)
        log_info(
            "terminal",
            "launch.detached",
            terminal_cmd=" ".join(argv),
            pid=process.pid,
            platform="Windows",
        )
        return True, ""  # Popen is non-blocking, can't check success

    if is_inside_ai_tool():
        # Fully detach: don't let AI tool's PTY capture our output
        stderr_path = None
        stderr_handle = None
        stdout_path = None
        stdout_handle = None
        try:
            stderr_handle = tempfile.NamedTemporaryFile(
                prefix="hcom_terminal_launch_",
                suffix=".log",
                dir=str(hcom_path(LAUNCH_DIR)),
                delete=False,
            )
            stderr_path = stderr_handle.name
        except Exception:
            stderr_handle = None
            stderr_path = None
        try:
            stdout_handle = tempfile.NamedTemporaryFile(
                prefix="hcom_terminal_stdout_",
                suffix=".txt",
                dir=str(hcom_path(LAUNCH_DIR)),
                delete=False,
            )
            stdout_path = stdout_handle.name
        except Exception:
            stdout_handle = None
            stdout_path = None

        popen_kwargs: dict[str, Any] = {}
        kitty_fd = _kitty_listen_fd()
        if kitty_fd is not None and any("kitten" in a for a in argv):
            popen_kwargs["pass_fds"] = (kitty_fd,)
        process = subprocess.Popen(
            argv,
            stdin=subprocess.DEVNULL,
            stdout=stdout_handle or subprocess.DEVNULL,
            stderr=stderr_handle or subprocess.DEVNULL,
            start_new_session=True,
            **popen_kwargs,
        )
        if stderr_handle:
            stderr_handle.close()
        if stdout_handle:
            stdout_handle.close()

        deadline = time.time() + 0.5
        returncode = None
        while time.time() < deadline:
            returncode = process.poll()
            if returncode is not None:
                break
            time.sleep(0.05)

        captured_stdout = ""
        if returncode is not None:
            stderr_text = ""
            if stderr_path:
                stderr_text = read_file_with_retry(
                    Path(stderr_path),
                    lambda f: f.read()[:1000],
                    default="",
                )
                try:
                    Path(stderr_path).unlink()
                except OSError:
                    pass
            if stdout_path:
                raw = read_file_with_retry(
                    Path(stdout_path),
                    lambda f: f.read()[:1000],
                    default="",
                )
                lines = raw.strip().splitlines()
                captured_stdout = lines[0] if lines else ""
                try:
                    Path(stdout_path).unlink()
                except OSError:
                    pass
            if returncode == 0:
                log_info(
                    "terminal",
                    "launch.detached.exit",
                    terminal_cmd=" ".join(argv),
                    returncode=returncode,
                    stderr=stderr_text,
                    captured_stdout=captured_stdout,
                )
                return True, captured_stdout

            log_warn(
                "terminal",
                "launch.detached.exit",
                terminal_cmd=" ".join(argv),
                returncode=returncode,
                stderr=stderr_text,
            )
            error_msg = f"Terminal launch failed (exit code {returncode})" + (f": {stderr_text}" if stderr_text else "")
            from .core.thread_context import get_is_codex
            if argv and argv[0] == "open" and get_is_codex():
                error_msg += " (Codex sandbox blocks LaunchServices; set permissions -> full access in codex or set HCOM_TERMINAL to kitty or tmux)"
            raise HcomError(error_msg)

        # Process still running after 0.5s — read stdout so far
        if stdout_path:
            raw = read_file_with_retry(
                Path(stdout_path),
                lambda f: f.read()[:1000],
                default="",
            )
            lines = raw.strip().splitlines()
            captured_stdout = lines[0] if lines else ""
            # Don't delete — process may still write to it; it's in LAUNCH_DIR (cleaned on reset)

        log_info(
            "terminal",
            "launch.detached",
            terminal_cmd=" ".join(argv),
            pid=process.pid,
            stderr_path=stderr_path or "",
            captured_stdout=captured_stdout,
        )
        return True, captured_stdout  # Fire and forget

    # Normal case: wait for terminal launcher to complete
    popen_kwargs_normal: dict[str, Any] = {}
    kitty_fd_normal = _kitty_listen_fd()
    if kitty_fd_normal is not None and any("kitten" in a for a in argv):
        popen_kwargs_normal["pass_fds"] = (kitty_fd_normal,)
    result = subprocess.run(argv, capture_output=True, text=True, **popen_kwargs_normal)
    if result.returncode != 0:
        stderr_text = (result.stderr or "").strip()
        error_msg = f"Terminal launch failed (exit code {result.returncode})" + (
            f": {stderr_text}" if stderr_text else ""
        )
        raise HcomError(error_msg)
    lines = (result.stdout or "").strip().splitlines()
    captured_stdout = lines[0] if lines else ""
    return True, captured_stdout


# ==================== Exports ====================

__all__ = [
    # Terminal presets
    "get_available_presets",
    "resolve_terminal_preset",
    # Environment building
    "build_env_string",
    # Script creation
    "find_bash_on_windows",
    "create_bash_script",
    # Terminal launching
    "get_macos_terminal_argv",
    "get_windows_terminal_argv",
    "get_linux_terminal_argv",
    "windows_hidden_popen",
    "PLATFORM_TERMINAL_GETTERS",
    "launch_terminal",
]
