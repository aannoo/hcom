"""Claude Code PTY integration for hcom.

Thin wrapper around unified pty_handler functions.
"""

from __future__ import annotations

from .pty_handler import run_pty_with_hcom
from .pty_common import CLAUDE_CODEX_READY_PATTERN

# Ready pattern - Claude shows "? for shortcuts" in status bar when idle
READY_PATTERN = CLAUDE_CODEX_READY_PATTERN


def run_claude_with_hcom(claude_args: list[str] | None = None) -> int:
    """Run Claude with hcom PTY integration. Blocks until Claude exits.

    Reads HCOM_PROCESS_ID from env (set by launcher).
    Instance name is resolved from process binding by run_pty_with_hcom.
    """
    # run_pty_with_hcom resolves instance_name from process binding
    return run_pty_with_hcom("claude", "", claude_args or [])


__all__ = ["run_claude_with_hcom", "READY_PATTERN"]
