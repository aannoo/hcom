#!/usr/bin/env python3
"""Terminal launching for HCOM"""

from __future__ import annotations

import os
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
    TERMINAL_PRESETS,
    HcomError,
)
from .core.paths import hcom_path, LAUNCH_DIR, read_file_with_retry
from .core.config import get_config

# ==================== Terminal Presets ====================

# Cache for available presets (computed once per process)
_available_presets_cache: list[tuple[str, bool]] | None = None

# macOS app bundle fallback commands for cross-platform terminals
# Used when CLI binary isn't in PATH but .app bundle is installed
_MACOS_APP_FALLBACKS: dict[str, str] = {
    "kitty": "open -n -a kitty.app --args {script}",
    "WezTerm": "open -n -a WezTerm.app --args start -- bash {script}",
    "Alacritty": "open -n -a Alacritty.app --args -e bash {script}",
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

    for name, (binary, cmd, platforms) in TERMINAL_PRESETS.items():
        # Skip if not for current platform
        if system not in platforms:
            continue

        # Check availability
        available = False
        if binary:
            # Check if binary exists in PATH
            available = shutil.which(binary) is not None
            # On macOS, also check for .app bundle fallback
            if not available and system == "Darwin" and name in _MACOS_APP_FALLBACKS:
                available = _find_macos_app(name) is not None
        else:
            # For macOS apps (binary=None), check app bundle locations
            if system == "Darwin":
                available = _find_macos_app(name) is not None
            else:
                available = True  # Assume available if no binary check

        result.append((name, available))

    result.append(("custom", True))  # Always available
    _available_presets_cache = result
    return result


def resolve_terminal_preset(preset_name: str) -> str | None:
    """Resolve preset name to command template.

    On macOS, if CLI binary isn't in PATH but .app bundle exists,
    returns the app bundle command instead.
    """
    if preset_name not in TERMINAL_PRESETS:
        return None

    binary, cmd, platforms = TERMINAL_PRESETS[preset_name]

    # On macOS, check if we need to use app bundle fallback
    if platform.system() == "Darwin" and binary and preset_name in _MACOS_APP_FALLBACKS:
        if shutil.which(binary) is None and _find_macos_app(preset_name) is not None:
            return _MACOS_APP_FALLBACKS[preset_name]

    return cmd


# ==================== Environment Building ====================


def build_env_string(env_vars: dict[str, Any], format_type: str = "bash") -> str:
    """Build environment variable string for bash shells"""
    # Filter out invalid bash variable names (must be letters, digits, underscores only)
    valid_vars = {
        k: v for k, v in env_vars.items() if re.match(r"^[a-zA-Z_][a-zA-Z0-9_]*$", k)
    }

    # On Windows, exclude PATH (let Git Bash handle it to avoid Windows vs Unix path format issues)
    if platform.system() == "Windows":
        valid_vars = {k: v for k, v in valid_vars.items() if k != "PATH"}

    if format_type == "bash_export":
        # Properly escape values for bash
        return " ".join(
            f"export {k}={shlex.quote(str(v))};" for k, v in valid_vars.items()
        )
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
                    str(
                        Path(base) / "Git" / "usr" / "bin" / "bash.exe"
                    ),  # usr/bin is more common
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
    if (path_bash := shutil.which("bash")) and not path_bash.lower().endswith(
        r"system32\bash.exe"
    ):
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

        # Unset tool markers when opening new windows to prevent identity inheritance
        # Custom terminals (Alacritty, etc.) inherit env vars from parent process
        if opens_new_window:
            f.write("unset CLAUDECODE GEMINI_CLI GEMINI_SYSTEM_MD ")
            f.write("CODEX_SANDBOX CODEX_SANDBOX_NETWORK_DISABLED ")
            f.write("CODEX_MANAGED_BY_NPM CODEX_MANAGED_BY_BUN\n")

        if platform.system() != "Windows":
            # Check for 'claude' command (with or without args)
            cmd_stripped = command_str.lstrip()
            is_claude_command = cmd_stripped == "claude" or cmd_stripped.startswith(
                "claude "
            )
            if is_claude_command:
                # 1. Discover paths once
                claude_path = shutil.which("claude")
                if not claude_path:
                    # Fallback for native installer (alias-based, not in PATH)
                    for fallback in [
                        Path.home() / ".claude" / "local" / "claude",
                        Path.home() / ".local" / "bin" / "claude",
                        Path.home() / ".claude" / "bin" / "claude",
                    ]:
                        if fallback.exists() and fallback.is_file():
                            claude_path = str(fallback)
                            break
                node_path = shutil.which("node")

                # 2. Add to PATH for minimal environments
                paths_to_add = []
                for p in [node_path, claude_path]:
                    if p:
                        dir_path = str(Path(p).resolve().parent)
                        if dir_path not in paths_to_add:
                            paths_to_add.append(dir_path)

                if paths_to_add:
                    path_addition = ":".join(paths_to_add)
                    f.write(f'export PATH="{path_addition}:$PATH"\n')
                elif not claude_path:
                    # Warning for debugging
                    print("Warning: Could not locate 'claude' in PATH", file=sys.stderr)

            # 3. Write environment variables
            f.write(build_env_string(env, "bash_export") + "\n")

            if cwd:
                f.write(f"cd {shlex.quote(cwd)}\n")

            # 4. Platform-specific command modifications
            if is_claude_command and claude_path:
                if is_termux():
                    # Termux: explicit node to bypass shebang issues
                    final_node = node_path or "/data/data/com.termux/files/usr/bin/node"
                    # Quote paths for safety
                    command_str = command_str.replace(
                        "claude ",
                        f"{shlex.quote(final_node)} {shlex.quote(claude_path)} ",
                        1,
                    )
                else:
                    # Mac/Linux: use full path (PATH now has node if needed)
                    command_str = command_str.replace(
                        "claude ", f"{shlex.quote(claude_path)} ", 1
                    )
        else:
            # Windows: no PATH modification needed
            f.write(build_env_string(env, "bash_export") + "\n")
            if cwd:
                f.write(f"cd {shlex.quote(cwd)}\n")

        f.write(f"{command_str}\n")

        # Self-delete for normal mode (not background)
        if not background:
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


def _parse_terminal_command(template: str, script_file: str) -> list[str]:
    """Parse terminal command template safely to prevent shell injection.
    Parses the template FIRST, then replaces {script} placeholder in the
    parsed tokens. This avoids shell injection and handles paths with spaces.
    Args:
        template: Terminal command template with {script} placeholder
        script_file: Path to script file to substitute
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

    # Replace {script} in parsed tokens
    replaced = []
    placeholder_found = False
    for part in parts:
        if "{script}" in part:
            replaced.append(part.replace("{script}", script_file))
            placeholder_found = True
        else:
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

    # config.env defaults + internal vars, then shell env overrides
    env_vars = env.copy()

    # Ensure SHELL is in env dict BEFORE os.environ update
    # (Critical for Termux Activity Manager which launches scripts in clean environment)
    if "SHELL" not in env_vars:
        shell_path = os.environ.get("SHELL")
        if not shell_path:
            shell_path = shutil.which("bash") or shutil.which("sh")
        if not shell_path:
            # Platform-specific fallback
            if is_termux():
                shell_path = "/data/data/com.termux/files/usr/bin/bash"
            else:
                shell_path = "/bin/bash"
        if shell_path:
            env_vars["SHELL"] = shell_path

    # Filter instance-specific vars to prevent identity inheritance from parent AI tool
    # - CLAUDECODE: new instances get their own from Claude Code
    # - GEMINI_SYSTEM_MD: hcom-set system prompt file for Gemini (shouldn't leak to children)
    # - CODEX_*: Codex sandbox vars that would cause is_inside_ai_tool() loop in children
    # - GEMINI_CLI: Gemini CLI marker (shouldn't leak to children)
    # - HCOM_* not in KNOWN_CONFIG_KEYS: identity/internal vars (HCOM_PROCESS_ID, etc.)
    # Config vars (HCOM_TAG, etc.) are inherited - surfaced in launch context for override
    # Shell env still overrides config.env for non-HCOM user vars (ANTHROPIC_MODEL, etc)
    # HCOM_DIR is special: always propagate for sandbox/project-local support
    TOOL_SPECIFIC_VARS = {
        "CLAUDECODE",
        "GEMINI_SYSTEM_MD",
        "GEMINI_CLI",
        "CODEX_SANDBOX",
        "CODEX_SANDBOX_NETWORK_DISABLED",
        "CODEX_MANAGED_BY_NPM",
        "CODEX_MANAGED_BY_BUN",
    }
    PROPAGATE_HCOM_VARS = {
        "HCOM_DIR",
        "HCOM_VIA_SHIM",
    }  # Always propagate these HCOM_* vars
    # Config keys that should NOT propagate - each launch uses its own config
    NO_PROPAGATE_CONFIG_KEYS = {
        "HCOM_TERMINAL",  # Terminal preference is per-user, not inherited
    }
    from .core.config import KNOWN_CONFIG_KEYS

    def should_propagate(key: str) -> bool:
        """Determine if an env var should be passed to child processes."""
        if key in TOOL_SPECIFIC_VARS:
            return False
        if key in NO_PROPAGATE_CONFIG_KEYS:
            return False
        if not key.startswith("HCOM_"):
            return True  # Non-HCOM vars pass through
        # HCOM_* vars: only propagate if known config or explicitly listed
        return key in KNOWN_CONFIG_KEYS or key in PROPAGATE_HCOM_VARS

    env_vars.update({k: v for k, v in os.environ.items() if should_propagate(k)})
    command_str = command

    # 1) Determine script extension
    # macOS default mode uses .command
    # All other cases (custom terminal, other platforms, background) use .sh
    terminal_mode = get_config().terminal
    use_command_ext = (
        not background and platform.system() == "Darwin" and terminal_mode == "default"
    )
    extension = ".command" if use_command_ext else ".sh"
    script_file = str(
        hcom_path(
            LAUNCH_DIR, f"hcom_{os.getpid()}_{random.randint(1000, 9999)}{extension}"
        )
    )

    # Detect tool from command for terminal title
    cmd_lower = command_str.lower()
    if "gemini" in cmd_lower:
        tool_name = "Gemini"
    elif "codex" in cmd_lower:
        tool_name = "Codex"
    else:
        tool_name = "Claude Code"

    opens_new_window = not background and not run_here
    create_bash_script(
        script_file,
        env_vars,
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
                    bash_exe = find_bash_on_windows()
                    if not bash_exe:
                        raise Exception("Git Bash not found")

                    process = windows_hidden_popen(
                        [bash_exe, script_file],
                        env=env_vars,
                        cwd=cwd,
                        stdout=log_handle,
                    )
                else:
                    # Unix(Mac/Linux/Termux): detached bash execution with Python-piped logs
                    process = subprocess.Popen(
                        ["bash", script_file],
                        env=env_vars,
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
            error_output = read_file_with_retry(
                log_file, lambda f: f.read()[:1000], default=""
            )
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
            # Windows: can't exec, use subprocess
            result = subprocess.run([bash_exe, script_file], env=env_vars, cwd=cwd)
            if result.returncode != 0:
                raise HcomError(format_error("Terminal launch failed"))
            return True
        else:
            # Unix: exec replaces this process entirely, saving ~5MB per agent
            # The bash script's exit code becomes this process's exit code
            if cwd:
                os.chdir(cwd)
            os.execve("/bin/bash", ["bash", script_file], env_vars)
            # Never reaches here - execve replaces the process

    # 4) New window or custom command mode
    # Resolve terminal_mode: 'default' → platform auto-detect, preset name → command, else custom
    custom_cmd: str | list[str] | None
    if terminal_mode == "default":
        custom_cmd = None  # Will use platform default
    elif terminal_mode in TERMINAL_PRESETS:
        custom_cmd = resolve_terminal_preset(
            terminal_mode
        )  # Handles app bundle fallback
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
            return _spawn_terminal_process(final_argv, format_error)
        except HcomError:
            raise
        except Exception as e:
            raise HcomError(format_error(f"Failed to launch terminal: {e}"))
    else:
        # User-provided string commands - parse safely without shell=True
        try:
            final_argv = _parse_terminal_command(custom_cmd, script_file)
        except ValueError as e:
            print(str(e), file=sys.stderr)
            return False

        try:
            return _spawn_terminal_process(final_argv, format_error)
        except HcomError:
            raise
        except Exception as e:
            raise HcomError(format_error(f"Failed to execute terminal command: {e}"))


def _spawn_terminal_process(argv: list[str], format_error) -> bool:
    """Spawn terminal process, detached when inside AI tools.

    When running inside Gemini/Codex/Claude, their PTY wrappers capture
    subprocess output and render it in their TUI (blocking the screen).
    Solution: fully detach with Popen + start_new_session + DEVNULL.
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
        return True  # Popen is non-blocking, can't check success

    if is_inside_ai_tool():
        # Fully detach: don't let AI tool's PTY capture our output
        stderr_path = None
        stderr_handle = None
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

        process = subprocess.Popen(
            argv,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=stderr_handle or subprocess.DEVNULL,
            start_new_session=True,
        )
        if stderr_handle:
            stderr_handle.close()

        deadline = time.time() + 0.5
        returncode = None
        while time.time() < deadline:
            returncode = process.poll()
            if returncode is not None:
                break
            time.sleep(0.05)
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
            if returncode == 0:
                log_info(
                    "terminal",
                    "launch.detached.exit",
                    terminal_cmd=" ".join(argv),
                    returncode=returncode,
                    stderr=stderr_text,
                )
                return True

            log_warn(
                "terminal",
                "launch.detached.exit",
                terminal_cmd=" ".join(argv),
                returncode=returncode,
                stderr=stderr_text,
            )
            error_msg = (
                f"Terminal launch failed (exit code {returncode})"
                + (f": {stderr_text}" if stderr_text else "")
            )
            if argv and argv[0] == "open" and os.environ.get("CODEX_SANDBOX"):
                error_msg += (
                    " (Codex sandbox blocks LaunchServices; use Agent full access or run outside sandbox)"
                )
            raise HcomError(error_msg)

        log_info(
            "terminal",
            "launch.detached",
            terminal_cmd=" ".join(argv),
            pid=process.pid,
            stderr_path=stderr_path or "",
        )
        return True  # Fire and forget

    # Normal case: wait for terminal launcher to complete
    result = subprocess.run(argv, capture_output=True, text=True)
    if result.returncode != 0:
        stderr_text = (result.stderr or "").strip()
        error_msg = (
            f"Terminal launch failed (exit code {result.returncode})"
            + (f": {stderr_text}" if stderr_text else "")
        )
        raise HcomError(error_msg)
    return True


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
