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

from .pty_common import (
    GEMINI_READY_PATTERN,
    CLAUDE_CODEX_READY_PATTERN,
)
from ..core.log import log_info, log_error
from ..core.binary import get_native_binary
from ..shared import TOOL_MARKER_VARS, HCOM_IDENTITY_VARS


def _require_native_binary() -> str:
    """Get native binary path, raising if not available."""
    native_bin = get_native_binary()
    if not native_bin:
        raise RuntimeError(
            "hcom binary not found. "
            "Build with: ./build.sh"
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
    process_id: str,
    tag: str,
    tool_args: list[str],
    *,
    run_here: bool = False,
    extra_env: dict[str, str] | None = None,
    runner_module: str | None = None,
    runner_function: str | None = None,
    runner_extra_kwargs: str = "",
) -> str:
    """Create a bash script that runs a tool with hcom native PTY integration.

    Args:
        tool: Tool identifier ("claude", "gemini", "codex")
        cwd: Working directory
        instance_name: HCOM instance name
        process_id: HCOM process ID
        tag: Instance tag prefix
        tool_args: Arguments to pass to tool command
        run_here: If True, script is for current terminal (no exec bash at end)
        extra_env: Additional environment variables
        runner_module: Ignored (legacy compatibility)
        runner_function: Ignored (legacy compatibility)
        runner_extra_kwargs: Ignored (legacy compatibility)

    Returns:
        Path to created script file
    """
    # Ignore legacy Python PTY parameters
    del runner_module, runner_function, runner_extra_kwargs

    from ..core.paths import hcom_path, LAUNCH_DIR

    native_bin = _require_native_binary()
    script_file = str(hcom_path(LAUNCH_DIR, f"{tool}_{instance_name}_{random.randint(1000, 9999)}.sh"))

    # For new terminal launches, exec replaces this bash process with hcom
    # (eliminates one idle bash process during the session).
    # The .command wrapper handles exec bash -l after this script exits.
    use_exec = not run_here

    # Export HCOM_DIR if set
    hcom_dir = os.environ.get("HCOM_DIR", "")
    hcom_dir_export = f'export HCOM_DIR="{hcom_dir}"' if hcom_dir else "# HCOM_DIR not set"

    # Build env exports
    # HCOM_INSTANCE_NAME is a hint for delivery thread startup - the authoritative
    # name comes from process binding lookup (allows for name changes during session)
    env_exports = [
        f'export HCOM_PROCESS_ID="{process_id}"',
        f'export HCOM_TAG="{tag}"',
        f'export HCOM_INSTANCE_NAME="{instance_name}"',
        "export HCOM_LAUNCHED=1",
        hcom_dir_export,
    ]
    if os.environ.get("HCOM_VIA_SHIM"):
        env_exports.append("export HCOM_VIA_SHIM=1")
    if os.environ.get("HCOM_PTY_DEBUG"):
        env_exports.append("export HCOM_PTY_DEBUG=1")

    # Add tool-specific env
    tool_env = TOOL_EXTRA_ENV.get(tool, {})
    for key, value in tool_env.items():
        env_exports.append(f'export {key}="{value}"')

    # Add caller-provided extra env
    if extra_env:
        for key, value in extra_env.items():
            env_exports.append(f'export {key}="{value}"')

    env_block = "\n".join(env_exports)

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
    env: dict,
    instance_name: str,
    tool_args: list[str],
    *,
    tag: str = "",
    run_here: bool = False,
    extra_env: dict[str, str] | None = None,
    runner_module: str | None = None,
    runner_function: str | None = None,
    runner_extra_kwargs: str = "",
) -> str | None:
    """Launch a tool in a terminal via native PTY wrapper.

    Args:
        tool: Tool identifier ("claude", "gemini", "codex")
        cwd: Working directory
        env: Environment variables dict
        instance_name: HCOM instance name
        tool_args: Arguments to pass to tool command
        tag: Instance tag prefix (optional)
        run_here: If True, run in current terminal (blocking)
        extra_env: Additional environment variables
        runner_module: Ignored (legacy compatibility)
        runner_function: Ignored (legacy compatibility)
        runner_extra_kwargs: Ignored (legacy compatibility)

    Returns:
        instance_name on success, None on failure
    """
    from ..terminal import launch_terminal

    process_id = env.get("HCOM_PROCESS_ID")
    if not process_id:
        log_error(
            "pty",
            "pty.exit",
            "HCOM_PROCESS_ID not set in env",
            instance=instance_name,
            tool=tool,
        )
        return None

    # Forward launch context vars from env dict (not os.environ — that would
    # leak grandparent values in nested launches)
    merged_extra_env = dict(extra_env) if extra_env else {}
    for var in ("HCOM_LAUNCHED_BY", "HCOM_LAUNCH_BATCH_ID", "HCOM_LAUNCH_EVENT_ID", "HCOM_LAUNCHED_PRESET"):
        if val := env.get(var):
            merged_extra_env.setdefault(var, val)

    script_file = create_runner_script(
        tool,
        cwd,
        instance_name,
        process_id,
        tag,
        tool_args,
        run_here=run_here,
        extra_env=merged_extra_env,
        runner_module=runner_module,
        runner_function=runner_function,
        runner_extra_kwargs=runner_extra_kwargs,
    )

    success = launch_terminal(f"bash {shlex.quote(script_file)}", env, cwd=cwd, run_here=run_here)
    return instance_name if success else None


__all__ = [
    "create_runner_script",
    "launch_pty",
    # Re-exports for compatibility
    "GEMINI_READY_PATTERN",
    "CLAUDE_CODEX_READY_PATTERN",
]
