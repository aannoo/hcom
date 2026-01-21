"""ANSI escape codes for terminal colors and control sequences.

This module provides all color codes used by the TUI. Colors are defined as
raw ANSI escape sequences for maximum performance (no function call overhead).

Color Code Format
-----------------
Standard colors use SGR (Select Graphic Rendition) codes:
    ESC[Nm where N is the color code

256-color palette uses:
    ESC[38;5;Nm for foreground (38;5 prefix)
    ESC[48;5;Nm for background (48;5 prefix)

True color (RGB) uses:
    ESC[38;2;R;G;Bm for foreground
    ESC[48;2;R;G;Bm for background

Usage
-----
    from .colors import FG_ORANGE, RESET, BOLD
    print(f"{BOLD}{FG_ORANGE}Warning!{RESET}")

Always terminate colored text with RESET to restore default formatting.
"""

# ===== Text Attributes =====
RESET = "\033[0m"  # Reset all attributes
DIM = "\033[2m"  # Dimmed/faint text
BOLD = "\033[1m"  # Bold/bright text
REVERSE = "\033[7m"  # Swap foreground/background (used for cursor)

# ===== Standard Foreground Colors =====
FG_GREEN = "\033[32m"
FG_CYAN = "\033[36m"
FG_WHITE = "\033[37m"
FG_BLACK = "\033[30m"
FG_YELLOW = "\033[33m"
FG_RED = "\033[31m"

# ===== 256-Color Foreground =====
FG_GRAY = "\033[38;5;245m"  # Mid-gray (consistent across terminals)
FG_BLUE = "\033[38;5;75m"  # Sky blue

# TUI-specific foreground colors
FG_ORANGE = "\033[38;5;208m"  # Primary accent color
FG_GOLD = "\033[38;5;220m"  # Warning/attention
FG_LIGHTGRAY = "\033[38;5;250m"  # Secondary text
FG_DELIVER = "\033[38;5;156m"  # Light green for message delivery state
FG_CLAUDE_ORANGE = "\033[38;5;214m"  # Light orange for Claude section header
FG_CUSTOM_ENV = "\033[38;5;141m"  # Light purple for Custom Env section header
FG_STALE = "\033[38;5;137m"  # Tan/brownish-grey for stale instances

# ===== Standard Background Colors =====
BG_GREEN = "\033[42m"
BG_CYAN = "\033[46m"
BG_YELLOW = "\033[43m"
BG_RED = "\033[41m"
BG_GRAY = "\033[100m"  # Bright black (dark gray)

# ===== 256-Color Background =====
BG_BLUE = "\033[48;5;69m"  # Light blue
BG_STALE = "\033[48;5;137m"  # Tan/brownish-grey for stale instances

# TUI-specific background colors
BG_ORANGE = "\033[48;5;208m"  # Launch button, active selection
BG_CHARCOAL = "\033[48;5;236m"  # Selected row background
BG_GOLD = "\033[48;5;220m"  # Warning/blocking state

# ===== Terminal Control Sequences =====
CLEAR_SCREEN = "\033[2J"  # Clear entire screen
CURSOR_HOME = "\033[H"  # Move cursor to top-left (1,1)
HIDE_CURSOR = "\033[?25l"  # Hide cursor (DEC private mode)
SHOW_CURSOR = "\033[?25h"  # Show cursor (DEC private mode)

# ===== Box Drawing Characters =====
BOX_H = "─"  # Horizontal line (U+2500)
