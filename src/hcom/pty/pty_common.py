"""Shared PTY utilities and constants.

This module contains constants and utilities used by the PTY system.
The actual PTY operations are handled by the Rust binary.
"""

from __future__ import annotations

# ==================== Termux Shebang Bypass ====================

# Re-export from shared (canonical location)
from ..shared import termux_shebang_bypass, TERMUX_NODE_PATH


# ==================== Ready Patterns ====================

# Ready patterns for PTY detection (visible when tool TUI has loaded)
GEMINI_READY_PATTERN = b"Type your message"
CLAUDE_READY_PATTERN = b"? for shortcuts"
CODEX_READY_PATTERN = b"\xe2\x80\xba "  # › (U+203A) prompt — always visible, unlike "? for shortcuts" which is dropped in narrow terminals
CLAUDE_CODEX_READY_PATTERN = CLAUDE_READY_PATTERN  # Back-compat alias


# ==================== Terminal Title ====================


def set_terminal_title(instance_name: str) -> None:
    """Set terminal window and tab title for hcom instance."""
    try:
        title = f"hcom: {instance_name}"
        with open("/dev/tty", "w") as tty_fd:
            tty_fd.write(f"\033]1;{title}\007\033]2;{title}\007")
    except (OSError, IOError):
        pass


# ==================== Message Preview ====================

# Re-export from core.messages (canonical location)
from ..core.messages import build_message_preview, PREVIEW_MAX_LEN  # noqa: E402


def build_listen_instruction(instance_name: str) -> str:
    """Build message preview for notification.

    Used by command-line message delivery (adhoc/codex modes)
    to notify instances of pending messages without marking them as read.

    Example: <hcom>luna → you</hcom>
    """
    return build_message_preview(instance_name)


__all__ = [
    # Termux shebang bypass
    "termux_shebang_bypass",
    "TERMUX_NODE_PATH",
    # Terminal title
    "set_terminal_title",
    # Magic strings
    "GEMINI_READY_PATTERN",
    "CLAUDE_CODEX_READY_PATTERN",
    # Message preview
    "PREVIEW_MAX_LEN",
    "build_message_preview",
    "build_listen_instruction",
]
