"""Launch context capture - environment snapshot for disambiguation and audit."""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from typing import Any


# Env vars to capture (if set) — only vars that help disambiguate
# which terminal, pane, or environment an instance is running in.
CONTEXT_ENV_VARS = [
    # Terminal program + pane/window IDs
    "TERM_PROGRAM",
    "TERM_SESSION_ID",
    "WINDOWID",
    # iTerm2
    "ITERM_SESSION_ID",
    # Kitty
    "KITTY_WINDOW_ID",
    "KITTY_PID",
    "KITTY_LISTEN_ON",
    # Alacritty
    "ALACRITTY_WINDOW_ID",
    # WezTerm
    "WEZTERM_PANE",
    # GNOME/KDE
    "GNOME_TERMINAL_SCREEN",
    "KONSOLE_DBUS_WINDOW",
    # Other Linux terminals
    "TERMINATOR_UUID",
    "TILIX_ID",
    "GUAKE_TAB_UUID",
    # Windows Terminal
    "WT_SESSION",
    # ConEmu
    "ConEmuHWND",
    # Multiplexers
    "TMUX_PANE",
    "STY",
    "ZELLIJ_SESSION_NAME",
    "ZELLIJ_PANE_ID",
    # SSH (connection identity, not auth)
    "SSH_TTY",
    "SSH_CONNECTION",
    # WSL
    "WSL_DISTRO_NAME",
    # IDE terminals
    "VSCODE_PID",
    "CURSOR_AGENT",
    "INSIDE_EMACS",
    "NVIM_LISTEN_ADDRESS",
    # Cloud IDEs (one per platform)
    "CODESPACE_NAME",
    "GITPOD_WORKSPACE_ID",
    "CLOUD_SHELL",
    "REPL_ID",
]


def _run_quiet(cmd: list[str], timeout: float = 1.0) -> str:
    """Run command and return stdout, empty string on any error."""
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        return result.stdout.strip() if result.returncode == 0 else ""
    except Exception:
        return ""


def capture_context() -> dict[str, Any]:
    """Capture launch context snapshot.

    Returns dict with:
        - git_branch: current git branch (empty if not in repo)
        - tty: tty device path (empty if not a tty)
        - env: dict of set env vars from CONTEXT_ENV_VARS
    """
    ctx: dict[str, Any] = {}

    # Git branch
    ctx["git_branch"] = _run_quiet(["git", "branch", "--show-current"])

    # TTY device
    ctx["tty"] = _run_quiet(["tty"])

    # Env vars (only include if set)
    env: dict[str, str] = {}
    for var in CONTEXT_ENV_VARS:
        val = os.environ.get(var)
        if val:
            env[var] = val
    ctx["env"] = env

    # Terminal info for close-on-kill
    from .thread_context import get_launched_preset
    terminal_preset = get_launched_preset() or ""
    if not terminal_preset:
        from ..terminal import detect_terminal_from_env
        terminal_preset = detect_terminal_from_env() or ""
    if terminal_preset:
        ctx["terminal_preset"] = terminal_preset
        from ..core.settings import get_merged_preset
        preset = get_merged_preset(terminal_preset)
        if preset:
            pane_id_env = preset.get("pane_id_env")
            if pane_id_env:
                pane_id = os.environ.get(pane_id_env, "")
                if pane_id:
                    ctx["pane_id"] = pane_id
    # Process ID for kitty close-by-env matching
    from .thread_context import get_process_id
    hcom_process_id = get_process_id() or ""
    if hcom_process_id:
        ctx["process_id"] = hcom_process_id

    # Terminal ID from parent's stdout capture (for custom preset {id} in close commands)
    process_id_val = ctx.get("process_id", "")
    if process_id_val:
        from .paths import hcom_path
        id_file = Path(hcom_path(".tmp", "terminal_ids", process_id_val))
        if id_file.exists():
            try:
                terminal_id = id_file.read_text().strip()
                if terminal_id:
                    ctx["terminal_id"] = terminal_id
                id_file.unlink()
            except OSError:
                pass

    return ctx


def capture_context_json() -> str:
    """Capture context and return as JSON string for DB storage."""
    return json.dumps(capture_context(), separators=(",", ":"))


__all__ = ["CONTEXT_ENV_VARS", "capture_context", "capture_context_json"]
