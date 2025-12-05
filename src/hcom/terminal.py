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
from pathlib import Path
from typing import Any

from .shared import IS_WINDOWS, CREATE_NO_WINDOW, is_wsl, is_termux
from .core.paths import hcom_path, SCRIPTS_DIR, read_file_with_retry
from .core.config import get_config

# ==================== Claude Command Building ====================

def build_env_string(env_vars: dict[str, Any], format_type: str = "bash") -> str:
    """Build environment variable string for bash shells"""
    # Filter out invalid bash variable names (must be letters, digits, underscores only)
    valid_vars = {k: v for k, v in env_vars.items() if re.match(r'^[a-zA-Z_][a-zA-Z0-9_]*$', k)}

    # On Windows, exclude PATH (let Git Bash handle it to avoid Windows vs Unix path format issues)
    if platform.system() == 'Windows':
        valid_vars = {k: v for k, v in valid_vars.items() if k != 'PATH'}

    if format_type == "bash_export":
        # Properly escape values for bash
        return ' '.join(f'export {k}={shlex.quote(str(v))};' for k, v in valid_vars.items())
    else:
        return ' '.join(f'{k}={shlex.quote(str(v))}' for k, v in valid_vars.items())

def build_claude_command(claude_args: list[str] | None = None) -> str:
    """Build Claude command string from args.
    All args (including --agent, --model, etc) are passed through to claude CLI.
    """
    cmd_parts = ['claude']
    if claude_args:
        for arg in claude_args:
            cmd_parts.append(shlex.quote(arg))
    return ' '.join(cmd_parts)

# ==================== Script Creation ====================

def find_bash_on_windows() -> str | None:
    """Find Git Bash on Windows, avoiding WSL's bash launcher"""
    # 0. User-specified path via env var (highest priority)
    if user_bash := os.environ.get('CLAUDE_CODE_GIT_BASH_PATH'):
        if Path(user_bash).exists():
            return user_bash
    # Build prioritized list of bash candidates
    candidates = []
    # 1. Common Git Bash locations
    for base in [os.environ.get('PROGRAMFILES', r'C:\Program Files'),
                 os.environ.get('PROGRAMFILES(X86)', r'C:\Program Files (x86)')]:
        if base:
            candidates.extend([
                str(Path(base) / 'Git' / 'usr' / 'bin' / 'bash.exe'),  # usr/bin is more common
                str(Path(base) / 'Git' / 'bin' / 'bash.exe')
            ])
    # 2. Portable Git installation
    if local_appdata := os.environ.get('LOCALAPPDATA', ''):
        git_portable = Path(local_appdata) / 'Programs' / 'Git'
        candidates.extend([
            str(git_portable / 'usr' / 'bin' / 'bash.exe'),
            str(git_portable / 'bin' / 'bash.exe')
        ])
    # 3. PATH bash (if not WSL's launcher)
    if (path_bash := shutil.which('bash')) and not path_bash.lower().endswith(r'system32\bash.exe'):
        candidates.append(path_bash)
    # 4. Hardcoded fallbacks (last resort)
    candidates.extend([
        r'C:\Program Files\Git\usr\bin\bash.exe',
        r'C:\Program Files\Git\bin\bash.exe',
        r'C:\Program Files (x86)\Git\usr\bin\bash.exe',
        r'C:\Program Files (x86)\Git\bin\bash.exe'
    ])
    # Find first existing bash
    for bash in candidates:
        if bash and Path(bash).exists():
            return bash

    return None

def create_bash_script(script_file: str, env: dict[str, Any], cwd: str | None, command_str: str, background: bool = False) -> None:
    """Create a bash script for terminal launch
    Scripts provide uniform execution across all platforms/terminals.
    Cleanup behavior:
    - Normal scripts: append 'rm -f' command for self-deletion
    - Background scripts: persist until `hcom reset logs` cleanup (24 hours)
    """
    with open(script_file, 'w', encoding='utf-8') as f:
        f.write('#!/bin/bash\n')
        f.write('echo "Starting Claude Code..."\n')

        if platform.system() != 'Windows':
            # 1. Discover paths once
            claude_path = shutil.which('claude')
            if not claude_path:
                # Fallback for native installer (alias-based, not in PATH)
                for fallback in [
                    Path.home() / '.claude' / 'local' / 'claude',
                    Path.home() / '.local' / 'bin' / 'claude',
                    Path.home() / '.claude' / 'bin' / 'claude',
                ]:
                    if fallback.exists() and fallback.is_file():
                        claude_path = str(fallback)
                        break
            node_path = shutil.which('node')

            # 2. Add to PATH for minimal environments
            paths_to_add = []
            for p in [node_path, claude_path]:
                if p:
                    dir_path = str(Path(p).resolve().parent)
                    if dir_path not in paths_to_add:
                        paths_to_add.append(dir_path)

            if paths_to_add:
                path_addition = ':'.join(paths_to_add)
                f.write(f'export PATH="{path_addition}:$PATH"\n')
            elif not claude_path:
                # Warning for debugging
                print("Warning: Could not locate 'claude' in PATH", file=sys.stderr)

            # 3. Write environment variables
            f.write(build_env_string(env, "bash_export") + '\n')

            if cwd:
                f.write(f'cd {shlex.quote(cwd)}\n')

            # 4. Platform-specific command modifications
            if claude_path:
                if is_termux():
                    # Termux: explicit node to bypass shebang issues
                    final_node = node_path or '/data/data/com.termux/files/usr/bin/node'
                    # Quote paths for safety
                    command_str = command_str.replace(
                        'claude ',
                        f'{shlex.quote(final_node)} {shlex.quote(claude_path)} ',
                        1
                    )
                else:
                    # Mac/Linux: use full path (PATH now has node if needed)
                    command_str = command_str.replace('claude ', f'{shlex.quote(claude_path)} ', 1)
        else:
            # Windows: no PATH modification needed
            f.write(build_env_string(env, "bash_export") + '\n')
            if cwd:
                f.write(f'cd {shlex.quote(cwd)}\n')

        f.write(f'{command_str}\n')

        # Self-delete for normal mode (not background)
        if not background:
            f.write(f'rm -f {shlex.quote(script_file)}\n')

    # Make executable on Unix
    if platform.system() != 'Windows':
        os.chmod(script_file, 0o755)

# ==================== Terminal Launching ====================

def get_macos_terminal_argv() -> list[str]:
    """Return macOS Terminal.app launch command as argv list.
    Uses 'open -a Terminal' with .command files to avoid AppleScript permission popup.
    """
    return ['open', '-a', 'Terminal', '{script}']

def get_windows_terminal_argv() -> list[str]:
    """Return Windows terminal launcher as argv list."""
    from .commands.utils import format_error

    if not (bash_exe := find_bash_on_windows()):
        raise Exception(format_error("Git Bash not found"))

    if shutil.which('wt'):
        return ['wt', bash_exe, '{script}']
    return ['cmd', '/c', 'start', 'Claude Code', bash_exe, '{script}']

def get_linux_terminal_argv() -> list[str] | None:
    """Return first available Linux terminal as argv list."""
    terminals = [
        ('gnome-terminal', ['gnome-terminal', '--', 'bash', '{script}']),
        ('konsole', ['konsole', '-e', 'bash', '{script}']),
        ('xterm', ['xterm', '-e', 'bash', '{script}']),
    ]
    for term_name, argv_template in terminals:
        if shutil.which(term_name):
            return argv_template

    # WSL fallback integrated here
    if is_wsl() and shutil.which('cmd.exe'):
        if shutil.which('wt.exe'):
            return ['cmd.exe', '/c', 'start', 'wt.exe', 'bash', '{script}']
        return ['cmd.exe', '/c', 'start', 'bash', '{script}']

    return None

def windows_hidden_popen(argv: list[str], *, env: dict[str, str] | None = None, cwd: str | None = None, stdout: Any = None) -> subprocess.Popen:
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
            creationflags=CREATE_NO_WINDOW
        )
    else:
        raise RuntimeError("windows_hidden_popen called on non-Windows platform")

# Platform dispatch map
PLATFORM_TERMINAL_GETTERS = {
    'Darwin': get_macos_terminal_argv,
    'Windows': get_windows_terminal_argv,
    'Linux': get_linux_terminal_argv,
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

    if '{script}' not in template:
        raise ValueError(format_error("Custom terminal command must include {script} placeholder",
                                    'Example: open -n -a kitty.app --args bash "{script}"'))

    try:
        parts = shlex.split(template)
    except ValueError as e:
        raise ValueError(format_error(f"Invalid terminal command syntax: {e}",
                                    "Check for unmatched quotes or invalid shell syntax"))

    # Replace {script} in parsed tokens
    replaced = []
    placeholder_found = False
    for part in parts:
        if '{script}' in part:
            replaced.append(part.replace('{script}', script_file))
            placeholder_found = True
        else:
            replaced.append(part)

    if not placeholder_found:
        raise ValueError(format_error("{script} placeholder not found after parsing",
                                    "Ensure {script} is not inside environment variables"))

    return replaced

def launch_terminal(command: str, env: dict[str, str], cwd: str | None = None, background: bool = False) -> str | bool | None:
    """Launch terminal with command using unified script-first approach

    Environment precedence: config.env < shell environment
    Internal hcom vars (HCOM_LAUNCHED, etc) don't conflict with user vars.

    Args:
        command: Command string from build_claude_command
        env: Contains config.env defaults + hcom internal vars
        cwd: Working directory
        background: Launch as background process
    """
    from .commands.utils import format_error
    from .core.paths import LOGS_DIR
    import time

    # config.env defaults + internal vars, then shell env overrides
    env_vars = env.copy()

    # Ensure SHELL is in env dict BEFORE os.environ update
    # (Critical for Termux Activity Manager which launches scripts in clean environment)
    if 'SHELL' not in env_vars:
        shell_path = os.environ.get('SHELL')
        if not shell_path:
            shell_path = shutil.which('bash') or shutil.which('sh')
        if not shell_path:
            # Platform-specific fallback
            if is_termux():
                shell_path = '/data/data/com.termux/files/usr/bin/bash'
            else:
                shell_path = '/bin/bash'
        if shell_path:
            env_vars['SHELL'] = shell_path

    # Filter instance-specific vars to prevent identity inheritance from parent Claude
    # - CLAUDECODE: new instances get their own from Claude Code
    # - HCOM_* not in KNOWN_CONFIG_KEYS: identity/internal vars (HCOM_LAUNCH_*, HCOM_SESSION_ID, etc.)
    # Config vars (HCOM_TAG, etc.) are inherited - surfaced in launch context for override
    # Shell env still overrides config.env for non-HCOM user vars (ANTHROPIC_MODEL, etc)
    from .core.config import KNOWN_CONFIG_KEYS
    env_vars.update({k: v for k, v in os.environ.items()
                     if k != 'CLAUDECODE' and not (k.startswith('HCOM_') and k not in KNOWN_CONFIG_KEYS)})
    command_str = command

    # 1) Determine script extension
    # macOS default mode uses .command
    # All other cases (custom terminal, other platforms, background) use .sh
    terminal_mode = get_config().terminal
    use_command_ext = (
        not background
        and platform.system() == 'Darwin'
        and terminal_mode == 'new'
    )
    extension = '.command' if use_command_ext else '.sh'
    script_file = str(hcom_path(SCRIPTS_DIR,
        f'hcom_{os.getpid()}_{random.randint(1000,9999)}{extension}'))
    create_bash_script(script_file, env_vars, cwd, command_str, background)

    # 2) Background mode
    if background:
        logs_dir = hcom_path(LOGS_DIR)
        log_file = logs_dir / env['HCOM_BACKGROUND']

        try:
            with open(log_file, 'w', encoding='utf-8') as log_handle:
                if IS_WINDOWS:
                    # Windows: hidden bash execution with Python-piped logs
                    bash_exe = find_bash_on_windows()
                    if not bash_exe:
                        raise Exception("Git Bash not found")

                    process = windows_hidden_popen(
                        [bash_exe, script_file],
                        env=env_vars,
                        cwd=cwd,
                        stdout=log_handle
                    )
                else:
                    # Unix(Mac/Linux/Termux): detached bash execution with Python-piped logs
                    process = subprocess.Popen(
                        ['bash', script_file],
                        env=env_vars, cwd=cwd,
                        stdin=subprocess.DEVNULL,
                        stdout=log_handle, stderr=subprocess.STDOUT,
                        start_new_session=True
                    )

        except OSError as e:
            print(format_error(f"Failed to launch headless instance: {e}"), file=sys.stderr)
            return None

        # Health check
        time.sleep(0.2)
        if process.poll() is not None:
            error_output = read_file_with_retry(log_file, lambda f: f.read()[:1000], default="")
            print(format_error("Headless instance failed immediately"), file=sys.stderr)
            if error_output:
                print(f"  Output: {error_output}", file=sys.stderr)
            return None

        return str(log_file)

    # 3) Terminal modes
    if terminal_mode == 'print':
        # Print script path and contents
        try:
            with open(script_file, 'r', encoding='utf-8') as f:
                script_content = f.read()
            print(f"# Script: {script_file}")
            print(script_content)
            Path(script_file).unlink()  # Clean up immediately
            return True
        except Exception as e:
            print(format_error(f"Failed to read script: {e}"), file=sys.stderr)
            return False

    if terminal_mode == 'here':
        print("Launching Claude in current terminal...")
        if IS_WINDOWS:
            bash_exe = find_bash_on_windows()
            if not bash_exe:
                print(format_error("Git Bash not found"), file=sys.stderr)
                return False
            result = subprocess.run([bash_exe, script_file], env=env_vars, cwd=cwd)
        else:
            result = subprocess.run(['bash', script_file], env=env_vars, cwd=cwd)
        return result.returncode == 0

    # 4) New window or custom command mode
    # If terminal is not 'here' or 'print', it's either 'new' (platform default) or a custom command
    custom_cmd = None if terminal_mode == 'new' else terminal_mode

    if not custom_cmd:  # Platform default 'new' mode
        if is_termux():
            # Keep Termux as special case
            am_cmd = [
                'am', 'startservice', '--user', '0',
                '-n', 'com.termux/com.termux.app.RunCommandService',
                '-a', 'com.termux.RUN_COMMAND',
                '--es', 'com.termux.RUN_COMMAND_PATH', script_file,
                '--ez', 'com.termux.RUN_COMMAND_BACKGROUND', 'false'
            ]
            try:
                subprocess.run(am_cmd, check=False)
                return True
            except Exception as e:
                print(format_error(f"Failed to launch Termux: {e}"), file=sys.stderr)
                return False

        # Unified platform handling via helpers
        system = platform.system()
        if not (terminal_getter := PLATFORM_TERMINAL_GETTERS.get(system)):
            raise Exception(format_error(f"Unsupported platform: {system}"))

        custom_cmd = terminal_getter()
        if not custom_cmd:  # e.g., Linux with no terminals
            raise Exception(format_error("No supported terminal emulator found",
                                       "Install gnome-terminal, konsole, or xterm"))

    # Type-based dispatch for execution
    if isinstance(custom_cmd, list):
        # Our argv commands - safe execution without shell
        final_argv = [arg.replace('{script}', script_file) for arg in custom_cmd]
        try:
            if platform.system() == 'Windows':
                # Windows needs non-blocking for parallel launches
                subprocess.Popen(final_argv)
                return True  # Popen is non-blocking, can't check success
            else:
                result = subprocess.run(final_argv)
                if result.returncode != 0:
                    return False
                return True
        except Exception as e:
            print(format_error(f"Failed to launch terminal: {e}"), file=sys.stderr)
            return False
    else:
        # User-provided string commands - parse safely without shell=True
        try:
            final_argv = _parse_terminal_command(custom_cmd, script_file)
        except ValueError as e:
            print(str(e), file=sys.stderr)
            return False

        try:
            if platform.system() == 'Windows':
                # Windows needs non-blocking for parallel launches
                subprocess.Popen(final_argv)
                return True  # Popen is non-blocking, can't check success
            else:
                result = subprocess.run(final_argv)
                if result.returncode != 0:
                    return False
                return True
        except Exception as e:
            print(format_error(f"Failed to execute terminal command: {e}"), file=sys.stderr)
            return False

# ==================== Exports ====================

__all__ = [
    # Claude command building
    'build_env_string',
    'build_claude_command',
    # Script creation
    'find_bash_on_windows',
    'create_bash_script',
    # Terminal launching
    'get_macos_terminal_argv',
    'get_windows_terminal_argv',
    'get_linux_terminal_argv',
    'windows_hidden_popen',
    'PLATFORM_TERMINAL_GETTERS',
    'launch_terminal',
]
