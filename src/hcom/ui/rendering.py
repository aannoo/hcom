"""ANSI rendering and text formatting utilities.

This module provides functions for working with ANSI-colored text in a TUI:
- Measuring visible text length (ignoring escape codes)
- Truncating text while preserving ANSI formatting
- Text wrapping that respects ANSI codes
- Visual effects (color interpolation, gradients)

Key Challenges
--------------
ANSI escape codes are invisible to the terminal but count as characters in
Python strings. This module provides utilities to:

1. Measure "visible" length (what the user sees)
2. Truncate/pad to exact visible width
3. Wrap text without breaking escape sequences
4. Handle wide characters (CJK, emoji) that occupy 2 columns

All functions use wcwidth for accurate character width measurement.
"""

import re
import shutil
import sys
import textwrap
from contextlib import contextmanager
from io import StringIO
from typing import Tuple

import wcwidth


@contextmanager
def suppress_output():
    """Capture stdout/stderr to prevent CLI output from corrupting TUI display.

    Used when calling CLI commands from within the TUI to prevent their output
    from interfering with the terminal display.

    Example:
        with suppress_output():
            cmd_stop(["all"])  # Output is captured, not displayed
    """
    old_stdout, old_stderr = sys.stdout, sys.stderr
    sys.stdout, sys.stderr = StringIO(), StringIO()
    try:
        yield
    finally:
        sys.stdout, sys.stderr = old_stdout, old_stderr


# Import ANSI codes directly to avoid circular import with shared
from .colors import RESET, FG_GRAY, FG_WHITE, BG_CHARCOAL

# Regex to match ANSI escape sequences (CSI sequences)
# Matches: ESC [ <params> <intermediate> <final>
ANSI_RE = re.compile(r"\x1B\[[0-?]*[ -/]*[@-~]")

# TUI Layout Constants
MAX_INPUT_ROWS = 8  # Maximum height for text input area


def separator_line(width: int) -> str:
    """Render a horizontal separator line.

    Args:
        width: Line width in characters.

    Returns:
        Gray horizontal line with RESET at the end.
    """
    return f"{FG_GRAY}{'─' * width}{RESET}"


def ansi_len(text: str) -> int:
    """Get visible length of text, excluding ANSI codes and accounting for wide chars.

    Uses wcwidth to correctly measure CJK characters and emoji that occupy
    2 terminal columns.

    Args:
        text: String possibly containing ANSI escape sequences.

    Returns:
        Number of terminal columns the text will occupy when displayed.

    Example:
        >>> ansi_len("\\033[31mRed\\033[0m")  # "Red" is 3 chars
        3
        >>> ansi_len("你好")  # CJK chars are 2 columns each
        4
    """
    visible = ANSI_RE.sub("", text)
    # wcwidth.wcswidth returns -1 if string contains non-printable chars
    width = wcwidth.wcswidth(visible)
    return width if width >= 0 else len(visible)


def ansi_ljust(text: str, width: int) -> str:
    """Left-justify text to width, accounting for ANSI codes.

    Pads with spaces to reach the target width based on visible length.

    Args:
        text: String possibly containing ANSI codes.
        width: Target visible width.

    Returns:
        Text padded with spaces to reach width (if shorter).
    """
    visible = ansi_len(text)
    return text + (" " * (width - visible)) if visible < width else text


def bg_ljust(text: str, width: int, bg_color: str) -> str:
    """Left-justify text with colored background padding.

    Pads with spaces in the specified background color to fill width.

    Args:
        text: String to pad.
        width: Target visible width.
        bg_color: ANSI background color code for padding.

    Returns:
        Text with colored padding and RESET at end.
    """
    visible = ansi_len(text)
    if visible < width:
        padding = " " * (width - visible)
        return f"{text}{bg_color}{padding}{RESET}"
    return text


def truncate_ansi(text: str, width: int) -> str:
    """Truncate text to width, preserving ANSI codes, with ellipsis.

    Correctly handles:
    - ANSI escape sequences (preserved but not counted)
    - Wide characters (CJK, emoji) that occupy 2 columns
    - Adds "…" and RESET when truncation occurs

    Args:
        text: String possibly containing ANSI codes.
        width: Maximum visible width (including ellipsis).

    Returns:
        Truncated text ending with "…" if truncation occurred.
    """
    if width <= 0:
        return ""
    visible_len = ansi_len(text)
    if visible_len <= width:
        return text

    visible = 0
    result = []
    i = 0
    target = width - 1  # Reserve space for ellipsis

    while i < len(text) and visible < target:
        if text[i] == "\033":
            match = ANSI_RE.match(text, i)
            if match:
                result.append(match.group())
                i = match.end()
                continue

        # Check character width (wcwidth returns -1 for non-printable, treat as 1)
        char_width = wcwidth.wcwidth(text[i])
        if char_width < 0:
            char_width = 1

        # Only add if it fits
        if visible + char_width <= target:
            result.append(text[i])
            visible += char_width
        else:
            break  # No more space
        i += 1

    result.append("…")
    result.append(RESET)
    return "".join(result)


def smart_truncate_name(name: str, width: int) -> str:
    """Truncate name with ellipsis in the middle, preserving prefix and suffix.

    Better for identifiers where both start and end are meaningful.

    Args:
        name: Name to truncate (no ANSI codes expected).
        width: Maximum length.

    Returns:
        Truncated name with middle ellipsis.

    Example:
        >>> smart_truncate_name("bees_general-purpose_2", 11)
        'bees…pose_2'
    """
    if len(name) <= width:
        return name
    if width < 5:
        return name[:width]

    # Keep prefix and suffix, put ellipsis in middle
    # Reserve 1 char for ellipsis
    available = width - 1
    prefix_len = (available + 1) // 2  # Round up for prefix
    suffix_len = available - prefix_len

    return (
        name[:prefix_len] + "…" + name[-suffix_len:]
        if suffix_len > 0
        else name[:prefix_len] + "…"
    )


def truncate_path(path: str, max_len: int) -> str:
    """Truncate file path preserving filename at the end.

    Prioritizes showing the filename over directory path.

    Args:
        path: File path to truncate.
        max_len: Maximum length.

    Returns:
        Truncated path with leading ellipsis.

    Example:
        >>> truncate_path("/Users/anno/Dev/hook-comms/src/hcom/ui/rendering.py", 20)
        '…/ui/rendering.py'
    """
    if len(path) <= max_len:
        return path
    if max_len < 8:
        return "…" + path[-(max_len - 1) :]

    # Split into directory and filename
    sep = "/" if "/" in path else "\\"
    parts = path.rsplit(sep, 1)
    if len(parts) == 2:
        dirname, filename = parts
        # If filename alone is too long, truncate it from start
        if len(filename) >= max_len - 2:
            return "…" + sep + filename[-(max_len - 2) :]
        # Otherwise keep filename, truncate directory
        remaining = max_len - len(filename) - 2  # "…" + sep
        return "…" + dirname[-remaining:] + sep + filename
    # No separator - just truncate from start
    return "…" + path[-(max_len - 1) :]


class AnsiTextWrapper(textwrap.TextWrapper):
    """TextWrapper subclass that handles ANSI escape codes correctly.

    Standard textwrap.TextWrapper counts ANSI codes as visible characters,
    causing incorrect line breaks. This subclass uses ansi_len() to measure
    visible width.

    Example:
        wrapper = AnsiTextWrapper(width=40)
        lines = wrapper.wrap(f"{FG_RED}Error: something went wrong{RESET}")
    """

    def _wrap_chunks(self, chunks):
        """Override to use visible length for width calculations."""
        lines = []
        if self.width <= 0:
            raise ValueError("invalid width %r (must be > 0)" % self.width)

        chunks.reverse()
        while chunks:
            cur_line = []
            cur_len = 0
            indent = self.subsequent_indent if lines else self.initial_indent
            width = self.width - ansi_len(indent)

            while chunks:
                chunk_len = ansi_len(chunks[-1])
                if cur_len + chunk_len <= width:
                    cur_line.append(chunks.pop())
                    cur_len += chunk_len
                else:
                    break

            if chunks and ansi_len(chunks[-1]) > width:
                if not cur_line:
                    cur_line.append(chunks.pop())

            if cur_line:
                lines.append(indent + "".join(cur_line))

        return lines


def get_terminal_size() -> Tuple[int, int]:
    """Get terminal dimensions.

    Returns:
        (columns, rows) tuple with fallback to (100, 30).
    """
    size = shutil.get_terminal_size(fallback=(100, 30))
    return size.columns, size.lines


def ease_out_quad(t: float) -> float:
    """Ease-out quadratic curve for smooth animations.

    Produces fast start, slow finish effect.

    Args:
        t: Progress from 0.0 to 1.0.

    Returns:
        Eased value from 0.0 to 1.0.
    """
    return 1 - (1 - t) ** 2


def interpolate_color_index(start: int, end: int, progress: float) -> int:
    """Interpolate between two 256-color palette indices with ease-out.

    Used for smooth color transitions in the TUI.

    Args:
        start: Starting color index (0-255).
        end: Ending color index (0-255).
        progress: Progress from 0.0 to 1.0.

    Returns:
        Interpolated color index (0-255).
    """
    # Clamp progress to [0, 1]
    progress = max(0.0, min(1.0, progress))

    # Apply ease-out curve (50% fade in first 10s)
    eased = ease_out_quad(progress)

    # Linear interpolation between indices
    return int(start + (end - start) * eased)


def get_message_pulse_colors(seconds_since: float) -> tuple[str, str]:
    """Get animated colors for message recency indicator.

    Creates a "pulse" effect that fades from white to charcoal over 5 seconds
    after a new message arrives.

    Args:
        seconds_since: Seconds since last message (0 = just now).

    Returns:
        (bg_color, fg_color) tuple of ANSI RGB escape codes.
    """
    FADE_DURATION = 5.0  # seconds

    # At rest, use charcoal bg / light gray fg
    if seconds_since >= FADE_DURATION:
        return BG_CHARCOAL, FG_WHITE

    # Progress: 0.0 = recent (white), 1.0 = quiet (charcoal)
    progress = min(1.0, seconds_since / FADE_DURATION)

    # Apply ease-out curve
    eased = ease_out_quad(progress)

    # RGB interpolation for smooth gradients
    # Background: white (255) → charcoal (48)
    bg_val = int(255 - (255 - 48) * eased)
    # Foreground: dark (18) → light gray (188)
    fg_val = int(18 + (188 - 18) * eased)

    return (
        f"\033[48;2;{bg_val};{bg_val};{bg_val}m",
        f"\033[38;2;{fg_val};{fg_val};{fg_val}m",
    )


def get_device_sync_color(seconds_since: float) -> str:
    """Get color for remote device sync indicator based on recency.

    Fades from bright cyan (just synced) to gray (30+ seconds ago).

    Args:
        seconds_since: Seconds since last sync.

    Returns:
        ANSI RGB foreground color code.
    """
    if seconds_since >= 30:
        return "\033[38;5;245m"  # Gray baseline

    # Normalize to 0-1 range over 30 seconds
    t = min(seconds_since / 30.0, 1.0)

    # Bright cyan (0, 255, 255) → Gray (148, 148, 148)
    r = int(0 + 148 * t)
    g = int(255 - 107 * t)  # 255 → 148
    b = int(255 - 107 * t)  # 255 → 148

    return f"\033[38;2;{r};{g};{b}m"
