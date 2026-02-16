"""Unified PTY handler for hcom tool integrations.

This module handles launching tools (Claude, Gemini, Codex) via the Rust PTY
wrapper. The Rust binary handles all PTY operations including terminal emulation,
message delivery gating, and text injection.

Tool-specific modules (claude.py, gemini.py, codex.py) remain as thin wrappers
for arg preprocessing, but the actual PTY work is done in Rust.
"""

from __future__ import annotations

import os
import random
import shlex
import shutil

from ..core.log import log_info, log_error
from ..core.binary import get_native_binary
from ..shared import TOOL_MARKER_VARS, HCOM_IDENTITY_VARS


def _require_native_binary() -> str:
    """Get native binary path, raising if not available."""
    native_bin = get_native_binary()
    if not native_bin:
        raise RuntimeError(
            "hcom native binary not found for this platform. "
            "Reinstall: pip install --upgrade --force-reinstall hcom"
        )
    return native_bin


# ==================== Tool Configurations ====================

# Tool-specific environment variables passed to hcom pty
TOOL_EXTRA_ENV: dict[str, dict[str, str]] = {
    "claude": {"HCOM_PTY_MODE": "1"},
    "gemini": {},
    "codex": {},
}


# ==================== Runner Script Generation ====================


def create_runner_script(
    tool: str,
    cwd: str,
    instance_name: str,
    env: dict[str, str],
    tool_args: list[str],
    *,
    run_here: bool = False,
) -> str:
    """Create a bash script that runs a tool with hcom native PTY integration.

    The env dict should be the full instance_env from launcher.py, with
    HCOM_INSTANCE_NAME and tool-specific vars (TOOL_EXTRA_ENV) already added
    by the caller (launch_pty). This script exports all vars from env, so it
    is the single source of truth for what the Rust PTY binary sees.

    Note: the outer bash script (created by launch_terminal → create_bash_script)
    also exports instance_env. This intentional redundancy means the runner
    script overrides with identical values — see create_bash_script comment.

    Args:
        tool: Tool identifier ("claude", "gemini", "codex")
        cwd: Working directory
        instance_name: HCOM instance name (for script filename/comment only)
        env: Full instance environment variables dict
        tool_args: Arguments to pass to tool command
        run_here: If True, script is for current terminal (no exec bash at end)

    Returns:
        Path to created script file
    """
    from ..core.paths import hcom_path, LAUNCH_DIR
    from ..terminal import build_env_string

    native_bin = _require_native_binary()
    script_file = str(hcom_path(LAUNCH_DIR, f"{tool}_{instance_name}_{random.randint(1000, 9999)}.sh"))

    # For new terminal launches, exec replaces this bash process with hcom
    # (eliminates one idle bash process during the session).
    # The .command wrapper handles exec bash -l after this script exits.
    use_exec = not run_here

    env_block = build_env_string(env, "bash_export")

    # Build tool args for command line
    tool_args_str = " ".join(shlex.quote(arg) for arg in tool_args)

    # Resolve binary paths for environments with minimal PATH (e.g. kitty panes).
    # The tool, hcom, python, and node may all be needed (tool runs, hooks call hcom).
    path_dirs: list[str] = []
    for bin_name in [tool, "hcom", "python3", "node"]:
        bin_path = shutil.which(bin_name)
        if bin_path:
            d = os.path.dirname(bin_path)
            if d not in path_dirs:
                path_dirs.append(d)
    path_export = f'export PATH="{":".join(path_dirs)}:$PATH"' if path_dirs else ""

    script_content = f'''#!/bin/bash
# {tool.capitalize()} hcom native PTY runner ({instance_name})
# Using: {native_bin}
cd {shlex.quote(cwd)}

unset {' '.join(TOOL_MARKER_VARS)}
unset {' '.join(HCOM_IDENTITY_VARS)}
{env_block}
{path_export}

{"exec " if use_exec else ""}{shlex.quote(native_bin)} pty {tool} {tool_args_str}
'''

    with open(script_file, "w") as f:
        f.write(script_content)
    os.chmod(script_file, 0o755)

    log_info(
        "pty",
        "native.script",
        script=script_file,
        tool=tool,
        instance=instance_name,
    )

    return script_file


# ==================== Launch ====================


def launch_pty(
    tool: str,
    cwd: str,
    env: dict[str, str],
    instance_name: str,
    tool_args: list[str],
    *,
    run_here: bool = False,
) -> str | None:
    """Launch a tool in a terminal via native PTY wrapper.

    Args:
        tool: Tool identifier ("claude", "gemini", "codex")
        cwd: Working directory
        env: Full instance_env dict from launcher (config.toml + instance vars).
             Will be augmented with HCOM_INSTANCE_NAME and tool-specific vars.
        instance_name: HCOM instance name
        tool_args: Arguments to pass to tool command
        run_here: If True, run in current terminal (blocking)

    Returns:
        instance_name on success, None on failure
    """
    from ..terminal import launch_terminal

    if not env.get("HCOM_PROCESS_ID"):
        log_error(
            "pty",
            "pty.exit",
            "HCOM_PROCESS_ID not set in env",
            instance=instance_name,
            tool=tool,
        )
        return None

    # Add PTY-specific vars to env. HCOM_INSTANCE_NAME is a hint for the Rust
    # delivery thread startup — the authoritative name comes from process
    # binding lookup (allows for name changes during session).
    runner_env = env.copy()
    runner_env["HCOM_INSTANCE_NAME"] = instance_name
    runner_env.update(TOOL_EXTRA_ENV.get(tool, {}))

    script_file = create_runner_script(
        tool,
        cwd,
        instance_name,
        runner_env,
        tool_args,
        run_here=run_here,
    )

    success = launch_terminal(f"bash {shlex.quote(script_file)}", env, cwd=cwd, run_here=run_here)
    return instance_name if success else None


__all__ = [
    "create_runner_script",
    "launch_pty",
]
