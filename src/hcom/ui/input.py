"""Keyboard input handling and text editing.

This module provides cross-platform keyboard input for the TUI and text input
editing functions used by both Manage and Launch screens.

Key Features
------------
- Cross-platform keyboard input (Unix termios/tty, Windows msvcrt)
- UTF-8 multi-byte character handling
- Paste detection (distinguishes manual Enter from pasted newlines)
- Special key recognition (arrows, Ctrl+*, etc.)

Text Input Functions
--------------------
The text_input_* functions provide cursor-based text editing:
- text_input_insert: Insert text at cursor
- text_input_backspace: Delete character before cursor
- text_input_move_left/right: Move cursor

These functions operate on (buffer, cursor) tuples and return new
(buffer, cursor) values without mutating the originals.
"""

from __future__ import annotations
import os
import select
import sys
from typing import List, Optional

import wcwidth

# Import from rendering module (single source of truth)
from .rendering import ANSI_RE, MAX_INPUT_ROWS, ansi_len

# Import ANSI codes directly to avoid circular import with shared
from .colors import (
    DIM,
    FG_GRAY,
    FG_LIGHTGRAY,
    FG_WHITE,
    HIDE_CURSOR,
    RESET,
    REVERSE,
    SHOW_CURSOR,
)

# Platform detection
IS_WINDOWS = os.name == "nt"


def slice_by_visual_width(text: str, max_width: int) -> tuple[str, int]:
    """Slice text to fit within visual width, handling wide chars and ANSI codes.

    Used for text wrapping in the input area.

    Args:
        text: Text to slice (may contain ANSI codes).
        max_width: Maximum visual width in terminal columns.

    Returns:
        (chunk_text, chars_consumed) tuple where:
        - chunk_text: Substring that fits within max_width
        - chars_consumed: Number of characters consumed from input
    """
    visual_width = 0
    char_pos = 0

    while char_pos < len(text) and visual_width < max_width:
        # Skip ANSI codes (preserve them but don't count their width)
        if char_pos < len(text) and text[char_pos : char_pos + 1] == "\x1b":
            match = ANSI_RE.match(text, char_pos)
            if match:
                char_pos = match.end()
                continue

        if char_pos >= len(text):
            break

        # Check character width (wcwidth returns -1 for non-printable, 0 for zero-width)
        char = text[char_pos]
        char_width = wcwidth.wcwidth(char)
        if char_width < 0:
            char_width = 1  # Treat non-printable as width 1

        # Check if it fits
        if visual_width + char_width <= max_width:
            visual_width += char_width
            char_pos += 1
        else:
            break  # No more space

    return text[:char_pos], char_pos


class KeyboardInput:
    """Cross-platform keyboard input handler.

    Provides non-blocking keyboard input with special key recognition.
    Use as a context manager to properly set/restore terminal settings.

    Unix: Uses termios/tty for raw mode, select for non-blocking reads.
    Windows: Uses msvcrt for direct console input.

    Special Keys Returned
    ---------------------
    - "UP", "DOWN", "LEFT", "RIGHT": Arrow keys
    - "ENTER": Enter key (manual press, not pasted newline)
    - "ESC": Escape key
    - "BACKSPACE": Backspace/Delete
    - "SPACE": Spacebar
    - "TAB": Tab key
    - "CTRL_C", "CTRL_D", "CTRL_K", "CTRL_G", "CTRL_R": Control combinations
    - Single character: Printable characters
    - "\\n": Pasted newline (part of multi-character paste)

    Example:
        with KeyboardInput() as kbd:
            while True:
                if kbd.has_input():
                    key = kbd.get_key()
                    if key == "CTRL_D":
                        break
                    print(f"Pressed: {key}")
    """

    def __init__(self):
        """Initialize keyboard handler (platform-specific setup)."""
        self.is_windows = IS_WINDOWS
        if not self.is_windows:
            import termios
            import tty

            self.termios = termios
            self.tty = tty
            self.fd = sys.stdin.fileno()
            self.old_settings = None

    def __enter__(self):
        """Enter context: set terminal to cbreak mode and hide cursor."""
        if not self.is_windows:
            try:
                self.old_settings = self.termios.tcgetattr(self.fd)
                self.tty.setcbreak(self.fd)
            except Exception:
                self.old_settings = None
        sys.stdout.write(HIDE_CURSOR)
        sys.stdout.flush()
        return self

    def __exit__(self, *args):
        """Exit context: restore terminal settings and show cursor."""
        if not self.is_windows and self.old_settings:
            self.termios.tcsetattr(self.fd, self.termios.TCSADRAIN, self.old_settings)
        sys.stdout.write(SHOW_CURSOR)
        sys.stdout.flush()

    def has_input(self) -> bool:
        """Check if input is available without blocking.

        Returns:
            True if there's pending input to read.
        """
        if self.is_windows:
            import msvcrt

            return msvcrt.kbhit()  # type: ignore[attr-defined]
        else:
            try:
                return bool(select.select([self.fd], [], [], 0.0)[0])
            except (InterruptedError, OSError):
                return False

    def get_key(self) -> Optional[str]:
        """Read single key press, returning special key name or character.

        Non-blocking: returns None if no input available.

        Returns:
            Special key name (e.g., "UP", "ENTER", "CTRL_D"),
            single character for printable keys,
            "\\n" for pasted newlines,
            or None if no input.
        """
        if self.is_windows:
            import msvcrt

            if not msvcrt.kbhit():  # type: ignore[attr-defined]
                return None
            ch = msvcrt.getwch()  # type: ignore[attr-defined]
            if ch in ("\x00", "\xe0"):
                ch2 = msvcrt.getwch()  # type: ignore[attr-defined]
                keys = {"H": "UP", "P": "DOWN", "K": "LEFT", "M": "RIGHT"}
                return keys.get(ch2, None)
            # Distinguish manual Enter from pasted newlines (Windows)
            if ch in ("\r", "\n"):
                # If more input is immediately available, it's likely a paste
                if msvcrt.kbhit():  # type: ignore[attr-defined]
                    return "\n"  # Pasted newline, keep as literal
                else:
                    return "ENTER"  # Manual Enter key press
            if ch == "\x1b":
                return "ESC"
            if ch in ("\x08", "\x7f"):
                return "BACKSPACE"
            if ch == " ":
                return "SPACE"
            if ch == "\t":
                return "TAB"
            return ch if ch else None
        else:
            try:
                has_data = select.select([self.fd], [], [], 0.0)[0]
            except (InterruptedError, OSError):
                return None
            if not has_data:
                return None
            try:
                first_byte = os.read(self.fd, 1)
                if not first_byte:
                    return None
                # Determine UTF-8 sequence length from first byte
                b = first_byte[0]
                if b < 0x80:
                    # ASCII (1 byte)
                    ch = first_byte.decode("utf-8")
                elif b < 0xC0:
                    # Invalid leading byte (continuation byte), skip
                    return None
                elif b < 0xE0:
                    # 2-byte sequence
                    remaining = os.read(self.fd, 1)
                    ch = (first_byte + remaining).decode("utf-8", errors="replace")
                elif b < 0xF0:
                    # 3-byte sequence
                    remaining = os.read(self.fd, 2)
                    ch = (first_byte + remaining).decode("utf-8", errors="replace")
                else:
                    # 4-byte sequence
                    remaining = os.read(self.fd, 3)
                    ch = (first_byte + remaining).decode("utf-8", errors="replace")
                # Skip replacement characters from malformed sequences
                if ch == "\ufffd":
                    return None
            except (OSError, UnicodeDecodeError):
                return None
            if ch == "\x1b":
                try:
                    has_escape_data = select.select([self.fd], [], [], 0.1)[0]
                except (InterruptedError, OSError):
                    return "ESC"
                if has_escape_data:
                    try:
                        next1 = os.read(self.fd, 1).decode("utf-8", errors="ignore")
                        if next1 == "[":
                            next2 = os.read(self.fd, 1).decode("utf-8", errors="ignore")
                            keys = {"A": "UP", "B": "DOWN", "C": "RIGHT", "D": "LEFT"}
                            if next2 in keys:
                                return keys[next2]
                    except (OSError, UnicodeDecodeError):
                        pass
                return "ESC"
            # Distinguish manual Enter from pasted newlines
            if ch in ("\r", "\n"):
                # If more input is immediately available, it's likely a paste
                try:
                    has_paste_data = select.select([self.fd], [], [], 0.0)[0]
                except (InterruptedError, OSError):
                    return "ENTER"
                if has_paste_data:
                    return "\n"  # Pasted newline, keep as literal
                else:
                    return "ENTER"  # Manual Enter key press
            if ch in ("\x7f", "\x08"):
                return "BACKSPACE"
            if ch == " ":
                return "SPACE"
            if ch == "\t":
                return "TAB"
            if ch == "\x03":
                return "CTRL_C"
            if ch == "\x04":
                return "CTRL_D"
            if ch == "\x0b":
                return "CTRL_K"
            if ch == "\x07":
                return "CTRL_G"
            if ch == "\x12":
                return "CTRL_R"
            return ch


# ===== Text Input Helper Functions =====
# Shared between MANAGE and LAUNCH screens for consistent text editing.


def text_input_insert(buffer: str, cursor: int, text: str) -> tuple[str, int]:
    """Insert text at cursor position.

    Strips ANSI codes from pasted text to prevent display corruption.

    Args:
        buffer: Current text buffer.
        cursor: Current cursor position (0 to len(buffer)).
        text: Text to insert (may contain ANSI codes which are stripped).

    Returns:
        (new_buffer, new_cursor) tuple.
    """
    # Strip ANSI codes from pasted text (prevents cursor/layout issues)
    clean_text = ANSI_RE.sub("", text)
    new_buffer = buffer[:cursor] + clean_text + buffer[cursor:]
    new_cursor = cursor + len(clean_text)
    return new_buffer, new_cursor


def text_input_backspace(buffer: str, cursor: int) -> tuple[str, int]:
    """Delete character before cursor.

    Args:
        buffer: Current text buffer.
        cursor: Current cursor position.

    Returns:
        (new_buffer, new_cursor) tuple. No change if cursor is at position 0.
    """
    if cursor > 0:
        new_buffer = buffer[: cursor - 1] + buffer[cursor:]
        new_cursor = cursor - 1
        return new_buffer, new_cursor
    return buffer, cursor


def text_input_move_left(cursor: int) -> int:
    """Move cursor left one position.

    Args:
        cursor: Current cursor position.

    Returns:
        New cursor position (clamped to 0).
    """
    return max(0, cursor - 1)


def text_input_move_right(buffer: str, cursor: int) -> int:
    """Move cursor right one position.

    Args:
        buffer: Current text buffer (for length check).
        cursor: Current cursor position.

    Returns:
        New cursor position (clamped to buffer length).
    """
    return min(len(buffer), cursor + 1)


def calculate_text_input_rows(
    text: str, width: int, max_rows: int = MAX_INPUT_ROWS
) -> int:
    """Calculate rows needed to display wrapped text with literal newlines.

    Used for dynamic input area sizing.

    Args:
        text: Text content (may contain literal \\n characters).
        width: Terminal width for wrapping calculation.
        max_rows: Maximum rows to return.

    Returns:
        Number of rows needed (1 to max_rows).
    """
    if not text:
        return 1

    # Guard against invalid width
    if width <= 0:
        return max_rows

    lines = text.split("\n")
    total_rows = 0
    for line in lines:
        if not line:
            total_rows += 1
        else:
            # Use visual width (accounts for wide chars and ANSI codes)
            total_rows += max(1, (ansi_len(line) + width - 1) // width)
    return min(total_rows, max_rows)


def render_text_input(
    buffer: str,
    cursor: int,
    width: int,
    max_rows: int,
    prefix: str = "> ",
    send_state: str | None = None,
) -> List[str]:
    """Render text input with cursor, wrapping, and multi-line support.

    Creates a visually styled text input area with:
    - Visible cursor (inverse video on character or trailing space)
    - Automatic line wrapping at terminal width
    - Literal newline handling (multi-line input)
    - Visual feedback for send state

    Args:
        buffer: Text content to display.
        cursor: Cursor position (0 to len(buffer)).
        width: Terminal width for wrapping.
        max_rows: Maximum rows to render.
        prefix: Prefix for first line (e.g., "> ").
        send_state: Visual state - None (normal), "sending" (dimmed),
                    or "sent" (lighter color after send).

    Returns:
        List of formatted line strings ready for display.
    """
    # Determine colors based on send state
    if send_state == "sending":
        prefix_color = DIM + FG_GRAY
        text_color = DIM + FG_GRAY
    elif send_state == "sent":
        prefix_color = FG_LIGHTGRAY
        text_color = FG_WHITE
    else:
        prefix_color = FG_GRAY
        text_color = FG_WHITE

    if not buffer:
        return [f"{prefix_color}{prefix}{REVERSE} {RESET}{RESET}"]

    line_width = width - len(prefix)
    # Guard against invalid width (terminal too narrow)
    if line_width <= 0:
        return [f"{prefix_color}{prefix}{RESET}"]  # Just show prefix if no room

    before = buffer[:cursor]

    # Cursor inverts colors of character at position (or shows inverted space at end)
    if cursor < len(buffer):
        # Cursor inverts the character at cursor position
        cursor_char = buffer[cursor]
        after = buffer[cursor + 1 :]
        full = before + REVERSE + cursor_char + RESET + after
    else:
        # Cursor at end - show inverted space after last char
        full = before + REVERSE + " " + RESET

    # Split on literal newlines first
    lines = full.split("\n")

    # Wrap each line if needed
    wrapped = []
    for line_idx, line in enumerate(lines):
        if not line:
            # Empty line (from consecutive newlines or trailing newline)
            line_prefix = prefix if line_idx == 0 else " " * len(prefix)
            wrapped.append(
                f"{prefix_color if line_idx == 0 else text_color}{line_prefix}{RESET}"
            )
        else:
            # Wrap long lines by visual width (handles wide chars and ANSI codes)
            char_offset = 0
            is_first_chunk = True
            while char_offset < len(line):
                chunk, consumed = slice_by_visual_width(line[char_offset:], line_width)
                if not consumed:  # Safety: avoid infinite loop
                    break
                is_prefix_line = line_idx == 0 and is_first_chunk
                line_prefix = prefix if is_prefix_line else " " * len(prefix)
                pcolor = prefix_color if is_prefix_line else text_color
                wrapped.append(
                    f"{pcolor}{line_prefix}{RESET}{text_color}{chunk}{RESET}"
                )
                char_offset += consumed
                is_first_chunk = False

    # Pad or truncate to max_rows
    result = wrapped + [""] * max(0, max_rows - len(wrapped))
    return result[:max_rows]
